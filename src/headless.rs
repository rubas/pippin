//! The viewer without a window, for scripted tests: it connects, decodes
//! the video and shows nothing. The control FIFO sends the input and asks
//! for snapshots.

use std::sync::Arc;

use anyhow::{Result, bail};
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

/// Runs one session with a Mac display of `size` points at `scale` pixels
/// per point. `quit` on the control FIFO ends it without an error; it does
/// not reconnect.
pub fn run(cfg: Config, size: (u32, u32), scale: f64) -> Result<()> {
    let op = cfg.op.clone();
    let pixels = |v: u32| (v as f64 * scale).round() as u32;
    let request = move || crate::ui::display_request((pixels(size.0), pixels(size.1)), scale, op.lock().render_scale);
    let (tx, rx) = crossbeam_channel::unbounded();
    let session = Session::start(cfg, request(), Arc::new(ChannelSink(tx)));
    let result = serve(&session, &rx, request);
    session.shutdown();
    result
}

fn serve(session: &Session, events: &Receiver<Event>, request: impl Fn() -> DisplayRequest) -> Result<()> {
    let mut canvas = Canvas::default();
    let mut snapshots = Vec::new();
    for event in events {
        match event {
            Event::Connected => tracing::info!("connected; decoding without a window"),
            Event::Frame => canvas.put(session.frames().take()),
            Event::Canvas { width, height } => {
                tracing::info!("canvas {width}x{height}");
                canvas.resize(width, height);
            }
            Event::Cursor(_) => {}
            Event::Closed(None) => return Ok(()),
            Event::Closed(Some(reason)) | Event::Lost(reason) => bail!(reason),
            Event::Reconfigure => session.resize(request()),
            Event::Snapshot(path) => snapshots.push(path),
        }
        // A snapshot asked for before the first whole picture waits for it.
        if !snapshots.is_empty()
            && let Some(img) = canvas.image()
        {
            for path in snapshots.drain(..) {
                match snapshot::write_png(&img, &path) {
                    Ok(()) => tracing::info!("snapshot {}", path.display()),
                    Err(e) => tracing::warn!("snapshot {}: {e:#}", path.display()),
                }
            }
        }
    }
    // The session holds the sender, so the events never run out.
    Ok(())
}
