//! Shares text and images between the local clipboard and the host
//! pasteboard, in the directions that [`Mode`] allows.
//!
//! Remote changes arrive pushed over the control channel (auto-pasteboard);
//! local changes are found by polling, since neither X11 nor Wayland offer a
//! portable change notification through `arboard`. Text goes through
//! `arboard`; on Wayland images go through `wl-clipboard-rs` directly so PNG
//! bytes pass untouched instead of round-tripping through RGBA. Clipboard
//! contents are never logged.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::proto::clipboard::{Content, send_png, send_text};
use crate::session::{Control, Shared};

const POLL: Duration = Duration::from_millis(500);
/// Larger texts are not mirrored to the host.
const MAX_SEND: usize = 8 << 20;
/// Larger encoded images are not mirrored to the host.
const MAX_SEND_IMAGE: usize = 32 << 20;
const PNG_MIME: &str = "image/png";

/// Which way the clipboard is shared with the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// Not shared.
    Off,
    /// What you copy on the Mac comes to this machine.
    Receive,
    /// What you copy on this machine goes to the Mac.
    Send,
    /// Both directions.
    Both,
}

impl Mode {
    pub fn receives(self) -> bool {
        matches!(self, Mode::Receive | Mode::Both)
    }

    pub fn sends(self) -> bool {
        matches!(self, Mode::Send | Mode::Both)
    }
}

fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// One clipboard value. Images are always PNG-encoded.
#[derive(Clone)]
enum Value {
    Text(String),
    Png(Vec<u8>),
}

impl Value {
    fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        match self {
            Value::Text(t) => (0u8, t).hash(&mut h),
            Value::Png(p) => (1u8, p).hash(&mut h),
        }
        h.finish()
    }
}

/// Tracks both sides by fingerprint so a value set by one side isn't echoed
/// back to it. A value counts as synced only after its transfer succeeded.
#[derive(Default)]
struct Mirror {
    /// Last local value seen.
    local: Option<u64>,
    /// Last value both sides hold.
    synced: Option<u64>,
}

impl Mirror {
    /// A local value is new to both sides.
    fn local_is_new(&self, fp: u64) -> bool {
        self.local != Some(fp) && self.synced != Some(fp)
    }

    /// A host value is not on the local clipboard yet.
    fn host_is_new(&self, fp: u64) -> bool {
        self.synced != Some(fp) || self.local != Some(fp)
    }

    /// A local value that stays local.
    fn seen_locally(&mut self, fp: u64) {
        self.local = Some(fp);
    }

    /// Both sides now hold the value.
    fn synced(&mut self, fp: u64) {
        self.local = Some(fp);
        self.synced = Some(fp);
    }
}

/// Re-encodes a host TIFF as PNG.
fn tiff_to_png(tiff: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(tiff, image::ImageFormat::Tiff)?;
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(png)
}

fn host_value(content: Content) -> Option<Value> {
    match content {
        Content::Text(t) => Some(Value::Text(normalize(&t))),
        Content::Image { tiff: false, data } => Some(Value::Png(data)),
        Content::Image { tiff: true, data } => match tiff_to_png(&data) {
            Ok(png) => Some(Value::Png(png)),
            Err(e) => {
                tracing::warn!("host clipboard image unreadable: {e}");
                None
            }
        },
    }
}

fn is_text_mime(m: &str) -> bool {
    m.starts_with("text/plain") || matches!(m, "UTF8_STRING" | "STRING" | "TEXT")
}

/// A clipboard on this machine.
trait Board {
    fn read(&mut self) -> Option<Value>;
    fn write(&mut self, v: &Value) -> anyhow::Result<()>;
}

/// The local clipboard.
struct Local {
    // Must outlive any text we set: on Linux this process serves the
    // selection until another client takes it.
    clip: arboard::Clipboard,
    wayland: bool,
}

impl Board for Local {
    fn read(&mut self) -> Option<Value> {
        if !self.wayland {
            if let Ok(t) = self.clip.get_text() {
                return Some(Value::Text(normalize(&t)));
            }
            let img = self.clip.get_image().ok()?;
            let rgba = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())?;
            let mut png = Vec::new();
            rgba.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
            return Some(Value::Png(png));
        }
        use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents, get_mime_types};
        // Cheap: only lists what the owner offers.
        let mimes = get_mime_types(ClipboardType::Regular, Seat::Unspecified).ok()?;
        if mimes.iter().any(|m| is_text_mime(m)) {
            return self.clip.get_text().ok().map(|t| Value::Text(normalize(&t)));
        }
        if !mimes.contains(PNG_MIME) {
            return None;
        }
        let (mut pipe, _) = get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Specific(PNG_MIME)).ok()?;
        let mut png = Vec::new();
        pipe.by_ref().take(MAX_SEND_IMAGE as u64 + 1).read_to_end(&mut png).ok()?;
        Some(Value::Png(png))
    }

    fn write(&mut self, v: &Value) -> anyhow::Result<()> {
        match v {
            Value::Text(t) => self.clip.set_text(t.clone())?,
            Value::Png(png) if self.wayland => {
                use wl_clipboard_rs::copy::{MimeType, Options, Source};
                // Serves the selection from a background thread until replaced.
                Options::new().copy(Source::Bytes(png.clone().into_boxed_slice()), MimeType::Specific(PNG_MIME.into()))?;
            }
            Value::Png(png) => {
                let rgba = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.into_rgba8();
                let (w, h) = rgba.dimensions();
                self.clip.set_image(arboard::ImageData {
                    width: w as usize,
                    height: h as usize,
                    bytes: rgba.into_raw().into(),
                })?;
            }
        }
        Ok(())
    }
}

/// Moves values between a local board and the host. A failed transfer
/// leaves the state as it was, so the next tick tries it again.
struct Sharing {
    mode: Mode,
    mirror: Mirror,
    /// The latest host value that is not on the local clipboard yet.
    pending: Option<Value>,
    /// The write of `pending` failed and was reported.
    reported: bool,
}

impl Sharing {
    /// `current` is on the local clipboard at connect time. It stays local;
    /// only later changes go to the host.
    fn new(mode: Mode, current: Option<Value>) -> Self {
        let mut mirror = Mirror::default();
        if let Some(v) = current {
            mirror.seen_locally(v.fingerprint());
        }
        Self { mode, mirror, pending: None, reported: false }
    }

    /// The host pasteboard changed. Its latest value replaces an older one
    /// that is still pending.
    fn host(&mut self, content: Content) {
        if self.mode.receives() {
            self.pending = host_value(content).filter(|v| self.mirror.host_is_new(v.fingerprint()));
            self.reported = false;
        }
    }

    /// Sends a new local value to the host, then writes a pending host value
    /// to the local board. A new local value replaces a pending host value,
    /// because it is the newer copy.
    fn tick(&mut self, board: &mut impl Board, send: impl FnOnce(&[u8]) -> std::io::Result<()>) {
        if let Some(v) = board.read() {
            self.local(v, send);
        }
        let Some(v) = &self.pending else { return };
        match board.write(v) {
            Ok(()) => {
                tracing::debug!("local clipboard updated from host");
                self.mirror.synced(v.fingerprint());
                self.pending = None;
            }
            Err(e) if !self.reported => {
                tracing::warn!("setting local clipboard failed, trying again: {e}");
                self.reported = true;
            }
            Err(e) => tracing::debug!("setting local clipboard failed again: {e}"),
        }
    }

    fn local(&mut self, v: Value, send: impl FnOnce(&[u8]) -> std::io::Result<()>) {
        let fp = v.fingerprint();
        if matches!(&v, Value::Text(t) if t.is_empty()) || !self.mirror.local_is_new(fp) {
            return;
        }
        self.pending = None;
        if !self.mode.sends() {
            self.mirror.seen_locally(fp);
            return;
        }
        let msg = match &v {
            Value::Text(t) if t.len() > MAX_SEND => {
                tracing::info!("local clipboard text too large to share ({} bytes)", t.len());
                self.mirror.seen_locally(fp);
                return;
            }
            Value::Png(p) if p.len() > MAX_SEND_IMAGE => {
                tracing::info!("local clipboard image too large to share ({} bytes)", p.len());
                self.mirror.seen_locally(fp);
                return;
            }
            Value::Text(t) => send_text(t),
            Value::Png(p) => send_png(p),
        };
        match send(&msg) {
            Ok(()) => {
                tracing::debug!("sent local clipboard to host ({} bytes)", msg.len());
                self.mirror.synced(fp);
            }
            Err(e) => tracing::warn!("sending local clipboard to host failed ({} bytes), trying again: {e}", msg.len()),
        }
    }
}

pub(crate) fn run(shared: Arc<Shared>, control: Arc<Control>, remote: Receiver<Content>, mode: Mode) {
    let clip = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("local clipboard unavailable, clipboard sharing disabled: {e}");
            // Keep draining so the control reader never blocks.
            while !shared.is_stopped() {
                let _ = remote.recv_timeout(POLL);
            }
            return;
        }
    };
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut local = Local { clip, wayland };
    let mut sync = Sharing::new(mode, local.read());
    while !shared.is_stopped() {
        match remote.recv_timeout(POLL) {
            Ok(content) => sync.host(content),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        sync.tick(&mut local, |msg| control.try_send(msg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::clipboard::text_from_message;

    fn fp(s: &str) -> u64 {
        Value::Text(normalize(s)).fingerprint()
    }

    /// A local clipboard in memory whose writes can fail.
    #[derive(Default)]
    struct Fake {
        value: Option<Value>,
        broken: bool,
    }

    impl Board for Fake {
        fn read(&mut self) -> Option<Value> {
            self.value.clone()
        }

        fn write(&mut self, v: &Value) -> anyhow::Result<()> {
            anyhow::ensure!(!self.broken, "no clipboard owner");
            self.value = Some(v.clone());
            Ok(())
        }
    }

    impl Fake {
        fn holding(text: &str) -> Self {
            Self { value: Some(Value::Text(text.into())), broken: false }
        }

        fn text(&self) -> Option<&str> {
            match &self.value {
                Some(Value::Text(t)) => Some(t),
                _ => None,
            }
        }
    }

    /// Runs one tick and returns the texts sent to the host.
    fn tick(sync: &mut Sharing, board: &mut Fake) -> Vec<String> {
        let mut sent = Vec::new();
        sync.tick(board, |msg| {
            sent.push(text_from_message(msg).expect("text message"));
            Ok(())
        });
        sent
    }

    /// Runs one tick whose send to the host fails.
    fn tick_send_fails(sync: &mut Sharing, board: &mut Fake) -> usize {
        let mut tries = 0;
        sync.tick(board, |_| {
            tries += 1;
            Err(std::io::ErrorKind::BrokenPipe.into())
        });
        tries
    }

    #[test]
    fn mode_off_shares_nothing() {
        let mut board = Fake::holding("linux at connect");
        let mut sync = Sharing::new(Mode::Off, board.read());
        sync.host(Content::Text("from mac".into()));
        board.value = Some(Value::Text("copied on linux".into()));
        assert!(tick(&mut sync, &mut board).is_empty());
        assert_eq!(board.text(), Some("copied on linux"));
    }

    #[test]
    fn mode_receive_takes_the_mac_copy_and_keeps_the_linux_copy() {
        let mut board = Fake::holding("linux at connect");
        let mut sync = Sharing::new(Mode::Receive, board.read());
        sync.host(Content::Text("from mac".into()));
        assert!(tick(&mut sync, &mut board).is_empty());
        assert_eq!(board.text(), Some("from mac"));
        board.value = Some(Value::Text("copied on linux".into()));
        assert!(tick(&mut sync, &mut board).is_empty());
    }

    #[test]
    fn mode_send_sends_the_linux_copy_and_ignores_the_mac_copy() {
        let mut board = Fake::holding("linux at connect");
        let mut sync = Sharing::new(Mode::Send, board.read());
        sync.host(Content::Text("from mac".into()));
        assert!(tick(&mut sync, &mut board).is_empty());
        assert_eq!(board.text(), Some("linux at connect"));
        board.value = Some(Value::Text("copied on linux".into()));
        assert_eq!(tick(&mut sync, &mut board), ["copied on linux"]);
    }

    #[test]
    fn mode_both_shares_each_way_without_echo() {
        let mut board = Fake::default();
        let mut sync = Sharing::new(Mode::Both, None);
        sync.host(Content::Text("a\r\nb".into()));
        // Our own write read back locally is not sent back.
        assert!(tick(&mut sync, &mut board).is_empty());
        assert_eq!(board.text(), Some("a\nb"));
        board.value = Some(Value::Text("c".into()));
        assert_eq!(tick(&mut sync, &mut board), ["c"]);
        // The host echoing our value back doesn't touch the local clipboard.
        sync.host(Content::Text("c".into()));
        assert!(sync.pending.is_none());
        // Polling the same value again sends nothing.
        assert!(tick(&mut sync, &mut board).is_empty());
    }

    #[test]
    fn failed_send_is_tried_again_on_the_next_tick() {
        let mut board = Fake::default();
        let mut sync = Sharing::new(Mode::Both, None);
        board.value = Some(Value::Text("copied on linux".into()));
        assert_eq!(tick_send_fails(&mut sync, &mut board), 1);
        assert_eq!(sync.mirror.synced, None);
        assert_eq!(tick(&mut sync, &mut board), ["copied on linux"]);
        assert_eq!(sync.mirror.synced, Some(fp("copied on linux")));
        assert!(tick(&mut sync, &mut board).is_empty());
    }

    #[test]
    fn failed_local_write_is_tried_again_and_does_not_send_the_old_copy() {
        let mut board = Fake { broken: true, ..Fake::holding("old linux copy") };
        let mut sync = Sharing::new(Mode::Both, board.read());
        sync.host(Content::Text("from mac".into()));
        // The old local value must not look like a new copy to send.
        assert!(tick(&mut sync, &mut board).is_empty());
        assert!(tick(&mut sync, &mut board).is_empty());
        assert_eq!(sync.mirror.synced, None);
        board.broken = false;
        assert!(tick(&mut sync, &mut board).is_empty());
        assert_eq!(board.text(), Some("from mac"));
        assert_eq!(sync.mirror.synced, Some(fp("from mac")));
    }

    #[test]
    fn new_linux_copy_replaces_a_mac_copy_that_failed_to_write() {
        let mut board = Fake { broken: true, ..Fake::default() };
        let mut sync = Sharing::new(Mode::Both, None);
        sync.host(Content::Text("from mac".into()));
        assert!(tick(&mut sync, &mut board).is_empty());
        board = Fake::holding("copied on linux");
        assert_eq!(tick(&mut sync, &mut board), ["copied on linux"]);
        assert!(sync.pending.is_none());
        assert_eq!(board.text(), Some("copied on linux"));
    }

    #[test]
    fn text_and_image_fingerprints_differ() {
        assert_ne!(Value::Text("x".into()).fingerprint(), Value::Png(b"x".to_vec()).fingerprint());
    }

    #[test]
    fn host_tiff_becomes_png() {
        let img = image::RgbaImage::from_fn(3, 2, |x, y| image::Rgba([x as u8 * 80, y as u8 * 120, 7, 255]));
        let mut tiff = Vec::new();
        img.write_to(&mut Cursor::new(&mut tiff), image::ImageFormat::Tiff).unwrap();
        let Some(Value::Png(png)) = host_value(Content::Image { tiff: true, data: tiff }) else { panic!("no png") };
        let back = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap().into_rgba8();
        assert_eq!(back, img);
    }
}
