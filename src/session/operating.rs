//! The operating point: the levers that trade picture quality for
//! robustness on a lossy link, switchable mid-session for experiments.
//!
//! Fast levers take effect at once: `recovery` (the next keyframe request),
//! `hold` (the next access unit with a gap) and `after_loss` (the next
//! access unit fed to the decoder). Slow ones renegotiate:
//! * `render_scale` asks the host for a display with fewer backing pixels,
//!   the same path as a window resize: SetDisplayConfiguration, the host's
//!   layout answer, a 0x1c re-offer, then a new SSRC group starting with an
//!   IDR (see `ControlReader::layout`).
//! * `fps` and `ltr` change the media offer, so they are applied with a
//!   0x1c re-offer (same keys and SSRCs) on the open connection. The host
//!   restarts its encoder on a re-offer after a resize; whether it also
//!   honours changed flags or LTR in a re-offer without a geometry change is
//!   unverified. If the stream keeps its old rate, `restart` reconnects,
//!   and the new session offers the current operating point from the start.
//!
//! A debug control FIFO (`PIPPIN_CONTROL_FIFO=/path`) takes one command
//! per line; see [`parse_command`]. For scripted tests, it also sends keys
//! and pointer input to the Mac and asks for snapshots of the picture.

use std::ffi::{CString, OsStr};
use std::fmt;
use std::io::{Error, ErrorKind, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::Sender;
use parking_lot::Mutex;

use super::{Control, DisplaySize, Event, Feedback, KeyframeCause, Shared};
use crate::proto::messages::{BTN_LEFT, BTN_RIGHT};
use crate::proto::offers::OfferOptions;
use crate::rtp::assembler::HoldMode;
use crate::rtp::rtcp::Recovery;
use crate::ui::keymap;

/// Offered frame rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Fps {
    #[value(name = "60")]
    Sixty,
    #[value(name = "30")]
    Thirty,
}

impl Fps {
    pub fn value(self) -> u32 {
        match self {
            Fps::Sixty => 60,
            Fps::Thirty => 30,
        }
    }
}

/// What the decoder does between a loss and the next keyframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AfterLoss {
    /// Drop every picture until the next keyframe: the picture freezes.
    Idr,
    /// Keep decoding, incomplete access units included (the slices that
    /// arrived): the picture moves but is smeared until the next keyframe.
    Conceal,
}

impl AfterLoss {
    pub fn name(self) -> &'static str {
        match self {
            AfterLoss::Idr => "idr",
            AfterLoss::Conceal => "conceal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [AfterLoss::Idr, AfterLoss::Conceal].into_iter().find(|m| m.name() == s)
    }
}

/// Where the client sits between quality and robustness. Held in
/// [`Shared`] and kept across reconnects.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OperatingPoint {
    /// Fraction of the window's pixels the host renders: 1 or 0.5. Slow.
    pub render_scale: f64,
    /// Slow: a re-offer.
    pub fps: Fps,
    /// Fast.
    pub recovery: Recovery,
    /// Fast.
    pub hold: HoldMode,
    /// LTR offered (slow: a re-offer) and LTR acks sent (stop at once).
    pub ltr: bool,
    /// Fast.
    pub after_loss: AfterLoss,
}

impl OperatingPoint {
    pub fn offer_options(&self) -> OfferOptions {
        OfferOptions { fps60: self.fps == Fps::Sixty, ltr: self.ltr }
    }
}

impl fmt::Display for OperatingPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "render_scale {} fps {} recovery {} hold {} ltr {} after_loss {}",
            self.render_scale,
            self.fps.value(),
            self.recovery.name(),
            self.hold.name(),
            if self.ltr { "on" } else { "off" },
            self.after_loss.name(),
        )
    }
}

pub type SharedOp = Arc<Mutex<OperatingPoint>>;

/// One lever setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lever {
    RenderScale(f64),
    Fps(Fps),
    Recovery(Recovery),
    Hold(HoldMode),
    Ltr(bool),
    AfterLoss(AfterLoss),
}

impl Lever {
    fn set(self, op: &mut OperatingPoint) {
        match self {
            Lever::RenderScale(v) => op.render_scale = v,
            Lever::Fps(v) => op.fps = v,
            Lever::Recovery(v) => op.recovery = v,
            Lever::Hold(v) => op.hold = v,
            Lever::Ltr(v) => op.ltr = v,
            Lever::AfterLoss(v) => op.after_loss = v,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Set(Lever),
    /// Ask every tile for a keyframe now (e.g. to measure keyframe size).
    Keyframe,
    /// End the session and reconnect with the current operating point.
    Restart,
    /// Press these keysyms in order, then release them in reverse order: a
    /// shortcut such as `cmd+ctrl+f`.
    Key(Vec<u32>),
    /// Press and release the keysym of each character in turn.
    Type(Vec<u32>),
    /// Move the pointer to this point of the Mac display.
    Pointer(u32, u32),
    /// Press and release this button (an RFB button bit) at the pointer.
    Click(u8),
    /// Scroll whole wheel steps at the pointer: positive scrolls right and
    /// down.
    Scroll(i32, i32),
    /// Write the latest picture to this PNG file.
    Snapshot(PathBuf),
    /// End the session and exit.
    Quit,
}

pub fn parse_on_off(s: &str) -> Result<bool, String> {
    match s {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("expected on or off, got {s:?}")),
    }
}

/// Parses a control line: `render_scale 1|0.5`, `fps 60|30`,
/// `recovery fir|pli|fir+pli`, `hold auto|on|off`, `ltr on|off`,
/// `after_loss idr|conceal`,
/// `keyframe` or `restart` (which ends the session like a dropped connection,
/// so it exits under `--no-reconnect`).
///
/// Input and output for scripted tests: `key <combo>` (key names joined by
/// `+`, such as `cmd+ctrl+f`; see [`keymap::by_name`]), `type <text>` (the
/// rest of the line), `pointer <x> <y>` (in points of the Mac display),
/// `click [left|right]`, `scroll <dx> <dy>` (whole wheel steps at the
/// pointer, positive scrolls right and down), `snapshot <path.png>` and
/// `quit`.
///
/// Blank lines and lines that start with `#` give `Ok(None)`. A `#` later
/// in a line is part of it, as in `snapshot /tmp/a#1.png`.
pub fn parse_command(line: &str) -> Result<Option<Command>, String> {
    let line = line.trim_end_matches(['\r', '\n']);
    // The text keeps its spaces.
    if let Some(text) = line.trim_start().strip_prefix("type ") {
        if text.is_empty() || text.chars().any(char::is_control) {
            return Err("type needs printable text".into());
        }
        return Ok(Some(Command::Type(text.chars().map(keymap::char_keysym).collect())));
    }
    let line = line.trim();
    if line.starts_with('#') {
        return Ok(None);
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    let Some((&cmd, args)) = words.split_first() else { return Ok(None) };
    let need = |what: &str| match args {
        [arg] => Ok(*arg),
        [] => Err(format!("{cmd} needs {what}")),
        _ => Err(format!("too many arguments: {line:?}")),
    };
    let lever = match cmd {
        "render_scale" => match need("1 or 0.5")?.parse::<f64>() {
            Ok(v) if v == 1.0 || v == 0.5 => Lever::RenderScale(v),
            _ => return Err("render_scale must be 1 or 0.5".into()),
        },
        "fps" => match need("60 or 30")? {
            "60" => Lever::Fps(Fps::Sixty),
            "30" => Lever::Fps(Fps::Thirty),
            other => return Err(format!("fps must be 60 or 30, got {other:?}")),
        },
        "recovery" => {
            let v = need("fir, pli or fir+pli")?;
            Lever::Recovery(Recovery::parse(v).ok_or_else(|| format!("unknown recovery {v:?}"))?)
        }
        "hold" => {
            let v = need("auto, on or off")?;
            Lever::Hold(HoldMode::parse(v).ok_or_else(|| format!("unknown hold mode {v:?}"))?)
        }
        "ltr" => Lever::Ltr(parse_on_off(need("on or off")?)?),
        "after_loss" => {
            let v = need("idr or conceal")?;
            Lever::AfterLoss(AfterLoss::parse(v).ok_or_else(|| format!("unknown after_loss {v:?}"))?)
        }
        "keyframe" | "restart" | "quit" if !args.is_empty() => return Err(format!("{cmd} takes no argument")),
        "keyframe" => return Ok(Some(Command::Keyframe)),
        "restart" => return Ok(Some(Command::Restart)),
        "quit" => return Ok(Some(Command::Quit)),
        "key" => {
            let keys = need("a key such as cmd+q")?.split('+').map(|k| keymap::by_name(k).ok_or_else(|| format!("unknown key {k:?}")));
            return Ok(Some(Command::Key(keys.collect::<Result<_, _>>()?)));
        }
        "type" => return Err("type needs printable text".into()),
        "pointer" => {
            let [x, y] = args else { return Err("pointer needs x and y in points".into()) };
            let point = |v: &str| v.parse().map_err(|_| format!("pointer needs whole points, got {v:?}"));
            return Ok(Some(Command::Pointer(point(x)?, point(y)?)));
        }
        "click" => {
            return Ok(Some(Command::Click(match args {
                [] | ["left"] => BTN_LEFT,
                ["right"] => BTN_RIGHT,
                _ => return Err(format!("click takes left or right, got {args:?}")),
            })));
        }
        "scroll" => {
            let [dx, dy] = args else { return Err("scroll needs dx and dy in wheel steps".into()) };
            let steps = |v: &str| v.parse().map_err(|_| format!("scroll needs whole steps, got {v:?}"));
            return Ok(Some(Command::Scroll(steps(dx)?, steps(dy)?)));
        }
        "snapshot" => return Ok(Some(Command::Snapshot(need("a path")?.into()))),
        _ => return Err(format!("unknown command {cmd:?}")),
    };
    Ok(Some(Command::Set(lever)))
}

/// A point of the Mac display → the stream pixel under it, the pointer's
/// coordinate space, clamped to the display.
fn to_pixels((x, y): (u32, u32), d: DisplaySize) -> (u16, u16) {
    let scale = |v: u32, pt: u16, px: u16| (v as u64 * px as u64 / pt.max(1) as u64).min(px.saturating_sub(1) as u64) as u16;
    (scale(x, d.points.0, d.pixels.0), scale(y, d.points.1, d.pixels.1))
}

/// Moves the session to `new`: fast levers are read where they're used, so
/// storing them is enough; slow ones start their renegotiation here.
pub(super) fn apply(shared: &Shared, fb: &Sender<Feedback>, new: OperatingPoint) {
    let old = std::mem::replace(&mut *shared.op_handle().lock(), new);
    if old == new {
        return;
    }
    tracing::info!("operating point: {new} (was {old})");
    if new.render_scale != old.render_scale {
        // The window asks for the display; the host's layout answer then
        // triggers the re-offer.
        shared.sink.emit(Event::Reconfigure);
    }
    if new.offer_options() != old.offer_options()
        && let Some(c) = shared.control.get()
    {
        tracing::info!("re-offering media session for the new frame rate / LTR setting");
        c.set_offer_options(new.offer_options());
        shared.touch(&shared.renegotiated);
        c.reoffer();
        let _ = fb.send(Feedback::Keyframe(None, KeyframeCause::Reoffer));
    }
}

/// Where the FIFO's pointer is in stream pixels. The pointer stays in
/// points, so a click after a change of the backing scale lands on the
/// same point of the Mac display.
fn pointer_pixels(at: Option<(u32, u32)>, display: Option<DisplaySize>) -> Result<(u16, u16), &'static str> {
    let at = at.ok_or("click and scroll need a pointer command first")?;
    let display = display.ok_or("no display layout yet, so the pointer has no position")?;
    Ok(to_pixels(at, display))
}

/// Runs one command. `pointer` is where the FIFO's pointer is, in points,
/// once a `pointer` command placed it.
fn run_command(shared: &Shared, control: &Control, fb: &Sender<Feedback>, pointer: &mut Option<(u32, u32)>, cmd: Command) {
    match cmd {
        // A new render scale needs a display request, which the Mac's own
        // display does not take.
        Command::Set(Lever::RenderScale(_)) if shared.max_backing.is_none() => {
            tracing::warn!("control FIFO: render_scale needs pippin's virtual display; this session streams the Mac's own display");
        }
        Command::Set(lever) => {
            let mut op = shared.op();
            lever.set(&mut op);
            apply(shared, fb, op);
        }
        Command::Keyframe => {
            tracing::info!("control: keyframe requested");
            let _ = fb.send(Feedback::Keyframe(None, KeyframeCause::Other));
        }
        Command::Restart => {
            tracing::info!("control: restarting the session");
            shared.sink.emit(Event::Lost("restart requested on the control FIFO".into()));
        }
        // Input is never logged: `type` may carry a password.
        Command::Key(keys) => {
            for &k in &keys {
                control.key(true, k);
            }
            for &k in keys.iter().rev() {
                control.key(false, k);
            }
        }
        Command::Type(keys) => {
            for k in keys {
                control.key(true, k);
                control.key(false, k);
            }
        }
        Command::Pointer(x, y) => {
            *pointer = Some((x, y));
            match pointer_pixels(*pointer, *shared.display.lock()) {
                Ok((px, py)) => control.pointer(0, px, py),
                Err(e) => tracing::warn!("control FIFO: {e}"),
            }
        }
        Command::Click(button) => match pointer_pixels(*pointer, *shared.display.lock()) {
            Ok((x, y)) => {
                control.pointer(button, x, y);
                control.pointer(0, x, y);
            }
            Err(e) => tracing::warn!("control FIFO: {e}"),
        },
        Command::Scroll(dx, dy) => match pointer_pixels(*pointer, *shared.display.lock()) {
            Ok((x, y)) => control.scroll(0, x, y, (dx, dy)),
            Err(e) => tracing::warn!("control FIFO: {e}"),
        },
        Command::Snapshot(path) => shared.sink.emit(Event::Snapshot(path)),
        Command::Quit => {
            tracing::info!("control: quitting");
            shared.sink.emit(Event::Closed(None));
        }
    }
}

/// Creates the FIFO at `path` if it is missing, and opens it. Whoever can
/// write to the FIFO drives the session, so pippin opens only a FIFO, not a
/// symlink, that belongs to this user and has no group or other
/// permissions, in a directory with the same rule, such as
/// `$XDG_RUNTIME_DIR`.
pub(super) fn open_fifo(path: &Path) -> std::io::Result<std::fs::File> {
    // Split at the last slash by hand: `Path::file_name` drops a trailing
    // slash, and a writer cannot open `pippin.ctl/`.
    let bytes = path.as_os_str().as_bytes();
    let (dir_path, name) = match bytes.iter().rposition(|&b| b == b'/') {
        Some(0) => (Path::new("/"), &bytes[1..]),
        Some(i) => (Path::new(OsStr::from_bytes(&bytes[..i])), &bytes[i + 1..]),
        None => (Path::new("."), bytes),
    };
    if name.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "the path has no file name"));
    }
    let name = CString::new(name).map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
    // The FIFO is created and opened relative to this descriptor, so the
    // checks hold for the directory that was checked. O_DIRECTORY refuses a
    // parent FIFO instead of waiting for its writer.
    let dir = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY).open(dir_path)?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    only_for(uid, &dir.metadata()?).map_err(|why| {
        Error::other(format!("the directory {} {why}; use a private directory such as $XDG_RUNTIME_DIR", dir_path.display()))
    })?;
    // SAFETY: an open directory descriptor and a NUL-terminated name.
    if unsafe { libc::mkfifoat(dir.as_raw_fd(), name.as_ptr(), 0o600) } != 0 {
        let e = Error::last_os_error();
        if e.kind() != ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    // Read-write, so opening doesn't wait for a writer and the FIFO never
    // reads as closed between writers.
    let flags = libc::O_RDWR | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: an open directory descriptor and a NUL-terminated name.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        let e = Error::last_os_error();
        return Err(if e.raw_os_error() == Some(libc::ELOOP) { Error::other("the path is a symlink") } else { e });
    }
    // SAFETY: openat returned a new descriptor that nothing else owns.
    let f = unsafe { std::fs::File::from_raw_fd(fd) };
    let meta = f.metadata()?;
    if !meta.file_type().is_fifo() {
        return Err(Error::other("not a FIFO"));
    }
    only_for(uid, &meta).map_err(|why| Error::other(format!("the FIFO {why}")))?;
    Ok(f)
}

/// Why another user than `uid` has access to the file of `meta`, if one does.
fn only_for(uid: u32, meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.uid() != uid {
        return Err(format!("belongs to uid {}, not to uid {uid}", meta.uid()));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!("has mode {mode:03o}, which gives group or other users access"));
    }
    Ok(())
}

/// The longest control line. A longer one is dropped up to its newline.
const MAX_LINE: usize = 4096;

/// Splits the bytes read from the FIFO into lines.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
    /// The current line went past [`MAX_LINE`].
    overlong: bool,
}

impl Lines {
    /// Adds `bytes` and returns the lines they completed, `None` for each
    /// line that was too long.
    fn push(&mut self, bytes: &[u8]) -> Vec<Option<String>> {
        let mut lines = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                lines.push((!self.overlong).then(|| String::from_utf8_lossy(&self.buf).into_owned()));
                self.buf.clear();
                self.overlong = false;
            } else if self.buf.len() < MAX_LINE && !self.overlong {
                self.buf.push(b);
            } else {
                self.buf.clear();
                self.overlong = true;
            }
        }
        lines
    }
}

/// Reads control commands from the FIFO until the session stops or a
/// `quit` arrives.
pub(super) fn control_fifo(shared: Arc<Shared>, control: Arc<Control>, mut f: std::fs::File, fb: Sender<Feedback>) {
    let mut lines = Lines::default();
    let mut pointer = None;
    let mut buf = [0u8; 1024];
    while !shared.stopped() {
        let mut pfd = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd; a short timeout so shutdown isn't held up.
        if unsafe { libc::poll(&mut pfd, 1, 200) } <= 0 {
            continue;
        }
        let n = match f.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => 0,
            Err(e) => {
                tracing::warn!("control FIFO: {e}");
                return;
            }
        };
        for line in lines.push(&buf[..n]) {
            // Never log the line: `type` may carry a password.
            let Some(line) = line else {
                tracing::warn!("control FIFO: dropped a line longer than {MAX_LINE} bytes");
                continue;
            };
            match parse_command(&line) {
                Ok(Some(Command::Quit)) => {
                    run_command(&shared, &control, &fb, &mut pointer, Command::Quit);
                    return;
                }
                Ok(Some(cmd)) => run_command(&shared, &control, &fb, &mut pointer, cmd),
                Ok(None) => {}
                Err(e) => tracing::warn!("control FIFO: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn set(l: Lever) -> Result<Option<Command>, String> {
        Ok(Some(Command::Set(l)))
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command("render_scale 0.5"), set(Lever::RenderScale(0.5)));
        assert_eq!(parse_command("  render_scale   1 \n"), set(Lever::RenderScale(1.0)));
        assert_eq!(parse_command("fps 30"), set(Lever::Fps(Fps::Thirty)));
        assert_eq!(parse_command("fps 60"), set(Lever::Fps(Fps::Sixty)));
        assert_eq!(parse_command("recovery pli"), set(Lever::Recovery(Recovery::Pli)));
        assert_eq!(parse_command("recovery fir+pli"), set(Lever::Recovery(Recovery::FirPli)));
        assert_eq!(parse_command("recovery fir"), set(Lever::Recovery(Recovery::Fir)));
        assert_eq!(parse_command("hold off"), set(Lever::Hold(HoldMode::Off)));
        assert_eq!(parse_command("hold auto"), set(Lever::Hold(HoldMode::Auto)));
        assert_eq!(parse_command("ltr off"), set(Lever::Ltr(false)));
        assert_eq!(parse_command("after_loss conceal"), set(Lever::AfterLoss(AfterLoss::Conceal)));
        assert_eq!(parse_command("after_loss idr"), set(Lever::AfterLoss(AfterLoss::Idr)));
        assert_eq!(parse_command("keyframe"), Ok(Some(Command::Keyframe)));
        assert_eq!(parse_command("restart\r"), Ok(Some(Command::Restart)));
        assert_eq!(parse_command(""), Ok(None));
        assert_eq!(parse_command("   # just a comment"), Ok(None));
    }

    #[test]
    fn rejects_bad_commands() {
        for bad in [
            "render_scale 0.75",
            "render_scale",
            "render_scale x",
            "fps 24",
            "recovery idr",
            "hold maybe",
            "ltr yes",
            "after_loss ltr",
            "after_loss",
            "restart now",
            "fps 30 60",
            "warp 9",
            "key",
            "key cmd+",
            "key command+q",
            "key cmd+q now",
            "type",
            "type a\tb",
            "pointer 10",
            "pointer 10 -5",
            "pointer 1.5 2",
            "click middle",
            "scroll 1",
            "scroll 1.5 0",
            "scroll 1 2 3",
            "snapshot",
            "snapshot a.png b.png",
            "quit now",
            // Only a whole line is a comment.
            "hold auto # comment",
        ] {
            assert!(parse_command(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn parses_input_and_snapshot_commands() {
        let cmd = |l| parse_command(l).unwrap().unwrap();
        assert_eq!(cmd("key cmd+ctrl+f"), Command::Key(vec![0xffeb, 0xffe3, 0x66]));
        assert_eq!(cmd("key return\n"), Command::Key(vec![0xff0d]));
        assert_eq!(cmd("key esc"), Command::Key(vec![0xff1b]));
        // The rest of the line, spaces and `#` included.
        assert_eq!(cmd("type a b#é\r\n"), Command::Type(vec![0x61, 0x20, 0x62, 0x23, 0xe9]));
        assert_eq!(cmd("pointer 1280 720"), Command::Pointer(1280, 720));
        assert_eq!(cmd("click"), Command::Click(BTN_LEFT));
        assert_eq!(cmd("click left"), Command::Click(BTN_LEFT));
        assert_eq!(cmd("click right"), Command::Click(BTN_RIGHT));
        assert_eq!(cmd("scroll 0 -3"), Command::Scroll(0, -3));
        assert_eq!(cmd("scroll 2 0"), Command::Scroll(2, 0));
        assert_eq!(cmd("snapshot /tmp/run/1.png"), Command::Snapshot("/tmp/run/1.png".into()));
        assert_eq!(cmd("snapshot /tmp/a#1.png"), Command::Snapshot("/tmp/a#1.png".into()));
        assert_eq!(cmd("type #1"), Command::Type(vec![0x23, 0x31]));
        assert_eq!(cmd("key cmd+plus"), Command::Key(vec![0xffeb, 0x2b]));
        assert_eq!(cmd("quit"), Command::Quit);
    }

    #[test]
    fn places_points_on_stream_pixels() {
        let retina = DisplaySize { points: (1280, 720), pixels: (2560, 1440) };
        assert_eq!(to_pixels((0, 0), retina), (0, 0));
        assert_eq!(to_pixels((640, 360), retina), (1280, 720));
        // Past the edge stays on the last pixel.
        assert_eq!(to_pixels((5000, 720), retina), (2559, 1439));
        let flat = DisplaySize { points: (2560, 1440), pixels: (2560, 1440) };
        assert_eq!(to_pixels((100, 200), flat), (100, 200));
    }

    #[test]
    fn a_click_converts_its_point_with_the_layout_of_the_moment() {
        let flat = DisplaySize { points: (1280, 720), pixels: (1280, 720) };
        let retina = DisplaySize { points: (1280, 720), pixels: (2560, 1440) };
        assert_eq!(pointer_pixels(Some((100, 50)), Some(flat)), Ok((100, 50)));
        assert_eq!(pointer_pixels(Some((100, 50)), Some(retina)), Ok((200, 100)));
        assert!(pointer_pixels(None, Some(flat)).is_err());
        assert!(pointer_pixels(Some((100, 50)), None).is_err());
    }

    #[test]
    fn drops_a_line_that_is_too_long_up_to_its_newline() {
        let mut lines = Lines::default();
        let long = "x".repeat(MAX_LINE) + "key c";
        assert_eq!(lines.push(format!("key a\n{long}").as_bytes()), vec![Some("key a".to_string())]);
        // The tail of the long line arrives later, and still belongs to it.
        assert_eq!(lines.push(b" more\nkey b\n"), vec![None, Some("key b".to_string())]);
        let exact = "y".repeat(MAX_LINE);
        assert_eq!(lines.push(format!("{exact}\n").as_bytes()), vec![Some(exact)]);
    }

    /// Runs the FIFO reader with a local TCP pair in place of the Mac, and
    /// reads what reaches the other end.
    #[test]
    fn fifo_sends_input_and_stops_after_quit() {
        use crate::proto::messages::encrypted_pointer_event;
        use crate::session::tests::{KEYS, connected};

        let (shared, control, mut host, events) = connected(Some((2560, 1440)));
        *shared.display.lock() = Some(DisplaySize { points: (1280, 720), pixels: (2560, 1440) });
        let dir = PrivateDir::new("fifo-input-test");
        let path = dir.join("pippin.ctl");
        let fifo = open_fifo(&path).unwrap();
        let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let (fb, _fb_rx) = crossbeam_channel::unbounded();
        let reader = {
            let (shared, control) = (shared.clone(), control.clone());
            std::thread::spawn(move || control_fifo(shared, control, fifo, fb))
        };

        let long = "x".repeat(MAX_LINE) + "key c";
        writer.write_all(format!("pointer 100 50\n{long}\n").as_bytes()).unwrap();
        assert_eq!(host.next(), Some(encrypted_pointer_event(&KEYS.key, 0, 200, 100)));
        // One step right is a press and release of button 7.
        writer.write_all(b"scroll 1 0\n").unwrap();
        assert_eq!(host.next(), Some(encrypted_pointer_event(&KEYS.key, 0x40, 200, 100)));
        assert_eq!(host.next(), Some(encrypted_pointer_event(&KEYS.key, 0, 200, 100)));
        // The backing scale changes before the click: it lands on the same point.
        *shared.display.lock() = Some(DisplaySize { points: (1280, 720), pixels: (1280, 720) });
        writer.write_all(b"click\nquit\nkey b\n").unwrap();
        assert_eq!(host.next(), Some(encrypted_pointer_event(&KEYS.key, BTN_LEFT, 100, 50)));
        assert_eq!(host.next(), Some(encrypted_pointer_event(&KEYS.key, 0, 100, 50)));
        // Neither the tail of the long line nor the key after quit arrives.
        assert_eq!(host.next(), None);
        assert!(matches!(events.recv().unwrap(), Event::Closed(None)));
        shared.stop.store(true, std::sync::atomic::Ordering::Release);
        reader.join().unwrap();
    }

    /// The operating point and the events after `render_scale 0.5`.
    fn half_render_scale(max_backing: Option<(u32, u32)>) -> (f64, Vec<Event>) {
        let (shared, control, _host, events) = crate::session::tests::connected(max_backing);
        let (fb, _fb_rx) = crossbeam_channel::unbounded();
        run_command(&shared, &control, &fb, &mut None, Command::Set(Lever::RenderScale(0.5)));
        (shared.op().render_scale, events.try_iter().collect())
    }

    #[test]
    fn render_scale_asks_the_viewer_for_a_new_virtual_display() {
        let (scale, events) = half_render_scale(Some((2560, 1440)));
        assert_eq!(scale, 0.5);
        assert!(matches!(events[..], [Event::Reconfigure]));
    }

    #[test]
    fn render_scale_is_refused_on_the_macs_own_display() {
        let (scale, events) = half_render_scale(None);
        assert_eq!(scale, 1.0);
        assert!(events.is_empty());
    }

    /// A directory that only this user can enter. Dropping it removes it.
    struct PrivateDir(PathBuf);

    impl PrivateDir {
        fn new(name: &str) -> Self {
            use std::os::unix::fs::DirBuilderExt;
            let path = std::env::temp_dir().join(format!("pippin-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for PrivateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn refusal(path: &Path) -> String {
        open_fifo(path).unwrap_err().to_string()
    }

    #[test]
    fn refuses_a_symlink_to_a_private_fifo() {
        let dir = PrivateDir::new("fifo-symlink-test");
        drop(open_fifo(&dir.join("real.ctl")).unwrap());
        std::os::unix::fs::symlink(dir.join("real.ctl"), dir.join("link.ctl")).unwrap();
        assert_eq!(refusal(&dir.join("link.ctl")), "the path is a symlink");
    }

    #[test]
    fn refuses_a_group_writable_fifo() {
        let dir = PrivateDir::new("fifo-group-test");
        let path = dir.join("pippin.ctl");
        drop(open_fifo(&path).unwrap());
        chmod(&path, 0o620);
        assert_eq!(refusal(&path), "the FIFO has mode 620, which gives group or other users access");
    }

    #[test]
    fn refuses_a_fifo_in_a_world_writable_directory() {
        let dir = PrivateDir::new("fifo-open-dir-test");
        chmod(&dir.0, 0o777);
        let path = dir.join("pippin.ctl");
        let want = format!("the directory {} has mode 777, which gives group or other users access", dir.0.display());
        assert!(refusal(&path).starts_with(&want));
        // pippin creates nothing in a directory that it refuses.
        assert!(!path.exists());
    }

    #[test]
    fn refuses_a_path_with_a_trailing_slash() {
        let dir = PrivateDir::new("fifo-slash-test");
        let path = dir.join("pippin.ctl");
        assert_eq!(refusal(&dir.0.join("pippin.ctl/")), "the path has no file name");
        assert!(!path.exists());
    }

    #[test]
    fn refuses_a_fifo_as_the_directory_without_waiting() {
        let dir = PrivateDir::new("fifo-parent-test");
        let pipe = dir.join("pipe");
        drop(open_fifo(&pipe).unwrap());
        let e = open_fifo(&pipe.join("pippin.ctl")).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR));
    }

    #[test]
    fn opens_a_private_fifo_without_a_writer() {
        let dir = PrivateDir::new("fifo-test");
        let path = dir.join("pippin.ctl");
        let mut f = open_fifo(&path).expect("created and opened");
        let mut buf = [0u8; 16];
        assert_eq!(f.read(&mut buf).unwrap_err().kind(), ErrorKind::WouldBlock);
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().write_all(b"fps 30\n").unwrap();
        let n = f.read(&mut buf).unwrap();
        assert_eq!(parse_command(std::str::from_utf8(&buf[..n]).unwrap()), set(Lever::Fps(Fps::Thirty)));
        drop(f);
        // A regular file is refused rather than read in a busy loop.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert_eq!(refusal(&path), "not a FIFO");
    }

    #[test]
    fn levers_set_their_field() {
        let mut op = OperatingPoint {
            render_scale: 1.0,
            fps: Fps::Sixty,
            recovery: Recovery::FirPli,
            hold: HoldMode::Auto,
            ltr: true,
            after_loss: AfterLoss::Idr,
        };
        let before = op;
        Lever::Hold(HoldMode::Off).set(&mut op);
        Lever::Recovery(Recovery::Pli).set(&mut op);
        assert_eq!(op.offer_options(), before.offer_options());
        Lever::Fps(Fps::Thirty).set(&mut op);
        Lever::RenderScale(0.5).set(&mut op);
        Lever::AfterLoss(AfterLoss::Conceal).set(&mut op);
        assert_eq!(op.offer_options(), OfferOptions { fps60: false, ltr: true });
        assert_eq!(op.to_string(), "render_scale 0.5 fps 30 recovery pli hold off ltr on after_loss conceal");
    }
}
