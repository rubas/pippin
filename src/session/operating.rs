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

use std::fmt;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
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
/// `click [left|right]`, `snapshot <path.png>` and `quit`.
///
/// Blank lines and `#` comments give `Ok(None)`.
pub fn parse_command(line: &str) -> Result<Option<Command>, String> {
    let line = line.trim_end_matches(['\r', '\n']);
    // The text keeps its spaces and any `#`.
    if let Some(text) = line.trim_start().strip_prefix("type ") {
        if text.is_empty() || text.chars().any(char::is_control) {
            return Err("type needs printable text".into());
        }
        return Ok(Some(Command::Type(text.chars().map(keymap::char_keysym).collect())));
    }
    let line = line.split('#').next().unwrap_or_default().trim();
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

/// Runs one command. `pointer` is where the FIFO's pointer is, in stream
/// pixels, once a `pointer` command placed it.
fn run_command(shared: &Shared, control: &Control, fb: &Sender<Feedback>, pointer: &mut Option<(u16, u16)>, cmd: Command) {
    match cmd {
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
            let Some(display) = *shared.display.lock() else {
                return tracing::warn!("control FIFO: no display layout yet, so pointer has no position to go to");
            };
            let (px, py) = to_pixels((x, y), display);
            *pointer = Some((px, py));
            control.pointer(0, px, py);
        }
        Command::Click(button) => {
            let Some((x, y)) = *pointer else {
                return tracing::warn!("control FIFO: click needs a pointer command first");
            };
            control.pointer(button, x, y);
            control.pointer(0, x, y);
        }
        Command::Snapshot(path) => shared.sink.emit(Event::Snapshot(path)),
        Command::Quit => {
            tracing::info!("control: quitting");
            shared.sink.emit(Event::Closed(None));
        }
    }
}

fn open_fifo(path: &Path) -> std::io::Result<std::fs::File> {
    if !path.exists() {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // SAFETY: a valid NUL-terminated path.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // Read-write, so opening doesn't wait for a writer and the FIFO never
    // reads as closed between writers.
    let f = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path)?;
    if !f.metadata()?.file_type().is_fifo() {
        return Err(std::io::Error::other("not a FIFO"));
    }
    Ok(f)
}

/// Reads control commands from the FIFO at `path` until the session stops.
pub(super) fn control_fifo(shared: Arc<Shared>, control: Arc<Control>, path: PathBuf, fb: Sender<Feedback>) {
    let mut f = match open_fifo(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("PIPPIN_CONTROL_FIFO {}: {e}", path.display());
            return;
        }
    };
    tracing::info!("reading control commands from {}", path.display());
    let mut pending = Vec::new();
    let mut pointer = None;
    let mut buf = [0u8; 1024];
    while !shared.stopped() {
        let mut pfd = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd; a short timeout so shutdown isn't held up.
        if unsafe { libc::poll(&mut pfd, 1, 200) } <= 0 {
            continue;
        }
        match f.read(&mut buf) {
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                tracing::warn!("control FIFO: {e}");
                return;
            }
        }
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            match parse_command(&String::from_utf8_lossy(&line)) {
                Ok(Some(cmd)) => run_command(&shared, &control, &fb, &mut pointer, cmd),
                Ok(None) => {}
                Err(e) => tracing::warn!("control FIFO: {e}"),
            }
        }
        if pending.len() > 4096 {
            pending.clear();
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
        assert_eq!(parse_command("hold auto # comment"), set(Lever::Hold(HoldMode::Auto)));
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
            "snapshot",
            "snapshot a.png b.png",
            "quit now",
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
        assert_eq!(cmd("snapshot /tmp/run/1.png"), Command::Snapshot("/tmp/run/1.png".into()));
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
    fn opens_fifo_without_a_writer() {
        let path = std::env::temp_dir().join(format!("pippin-fifo-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut f = open_fifo(&path).expect("created and opened");
        let mut buf = [0u8; 16];
        assert_eq!(f.read(&mut buf).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().write_all(b"fps 30\n").unwrap();
        let n = f.read(&mut buf).unwrap();
        assert_eq!(parse_command(std::str::from_utf8(&buf[..n]).unwrap()), set(Lever::Fps(Fps::Thirty)));
        drop(f);
        // A regular file is refused rather than read in a busy loop.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert!(open_fifo(&path).is_err());
        std::fs::remove_file(&path).unwrap();
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
