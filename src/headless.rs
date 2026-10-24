//! The viewer without a window, for scripted tests: it connects, decodes
//! the video and shows nothing. The control FIFO sends the input and asks
//! for snapshots.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::proto::messages::DisplayRequest;
use crate::session::{Config, Event, EventSink, Session};
use crate::snapshot::{self, Canvas};

struct ChannelSink(Sender<Event>);

impl EventSink for ChannelSink {
    fn emit(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

/// Runs one session. With `display`, the Mac creates a virtual display of
/// that many points at that many pixels per point; with `None`, it streams
/// its own main display. Only `quit` on the control FIFO ends it without an
/// error, and only when no snapshot still waits. It does not reconnect, and
/// a snapshot that fails to write ends it too.
pub fn run(cfg: Config, display: Option<((u32, u32), f64)>) -> Result<()> {
    let op = cfg.op.clone();
    let request = move || {
        let (size, scale) = display?;
        let pixels = |v: u32| (v as f64 * scale).round() as u32;
        // No monitor and no GPU texture limit: the Mac display never grows past the requested size.
        let full = (pixels(size.0), pixels(size.1));
        Some(crate::ui::display_request(full, scale, op.lock().render_scale, full))
    };
    let (tx, rx) = crossbeam_channel::unbounded();
    let session = Session::start(cfg, request(), Arc::new(ChannelSink(tx)));
    let result = serve(&session, &rx, request);
    session.shutdown();
    result
}

fn serve(session: &Session, events: &Receiver<Event>, request: impl Fn() -> Option<DisplayRequest>) -> Result<()> {
    let mut canvas = Canvas::default();
    let mut snapshots = Vec::new();
    for event in events {
        match event {
            Event::Connected => tracing::info!("connected; decoding without a window"),
            Event::Frame => {
                canvas.put(session.frames().take());
                session.presented();
            }
            Event::Canvas { width, height } => {
                tracing::info!("canvas {width}x{height}");
                canvas.resize(width, height);
            }
            Event::Cursor(_) => {}
            Event::Closed(reason) => return closed(reason, &snapshots),
            Event::Lost(reason) => bail!(reason),
            Event::Reconfigure => {
                if let Some(req) = request() {
                    session.resize(req);
                }
            }
            Event::Snapshot(path) => snapshots.push(path),
        }
        // A snapshot asked for before the first whole picture waits for it.
        if !snapshots.is_empty()
            && let Some(img) = canvas.image()
        {
            for path in snapshots.drain(..) {
                snapshot::write_png(&img, &path).with_context(|| format!("snapshot {}", path.display()))?;
                tracing::info!("snapshot {}", path.display());
            }
        }
    }
    // The session holds the sender, so the events never run out.
    Ok(())
}

/// How the run ends when the session closes for good: `reason` is `None`
/// after `quit`. Snapshots that still wait for a whole picture are an error.
fn closed(reason: Option<String>, waiting: &[PathBuf]) -> Result<()> {
    if let Some(reason) = reason {
        bail!(reason);
    }
    if !waiting.is_empty() {
        let paths: Vec<_> = waiting.iter().map(|p| p.display().to_string()).collect();
        bail!("quit before the first whole picture; no snapshot written to {}", paths.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_with_waiting_snapshots_is_an_error() {
        assert!(closed(None, &[]).is_ok());
        let e = closed(None, &["/run/t/0.png".into(), "/run/t/1.png".into()]).unwrap_err();
        assert_eq!(e.to_string(), "quit before the first whole picture; no snapshot written to /run/t/0.png, /run/t/1.png");
        assert_eq!(closed(Some("host ended the session".into()), &[]).unwrap_err().to_string(), "host ended the session");
    }
}
