//! The one thread that writes to the control connection. Every other thread
//! queues its messages, so a slow socket never blocks the window, and the
//! record sequence numbers follow the queue order.
//!
//! While the socket is busy, messages wait in the queue. Then consecutive
//! pointer moves merge into the latest position. A move never merges across
//! a button or key event, so a click lands where the pointer was.

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::proto::messages::{self, BTN_SCROLL_DOWN, BTN_SCROLL_LEFT, BTN_SCROLL_RIGHT, BTN_SCROLL_UP};
use crate::proto::record::RecordSealer;

/// A pointer event: the position in stream pixels and the RFB button mask.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Pointer {
    pub buttons: u8,
    pub x: u16,
    pub y: u16,
}

/// One message for the Mac, in the order the session made it.
#[derive(Debug, PartialEq)]
pub(super) enum Outgoing {
    /// Sealed with the record key by the writer.
    Pointer(Pointer),
    Message(Vec<u8>),
}

/// The pointer events of whole wheel steps at `at`: `steps.0` positive
/// scrolls right, `steps.1` positive scrolls down. The Mac takes each press
/// and release of a wheel button as one step.
pub(super) fn scroll(at: Pointer, (dx, dy): (i32, i32)) -> Vec<Pointer> {
    let mut out = Vec::new();
    for (steps, back, forward) in [(dy, BTN_SCROLL_UP, BTN_SCROLL_DOWN), (dx, BTN_SCROLL_LEFT, BTN_SCROLL_RIGHT)] {
        let wheel = if steps < 0 { back } else { forward };
        for _ in 0..steps.unsigned_abs() {
            out.push(Pointer { buttons: at.buttons | wheel, ..at });
            out.push(at);
        }
    }
    out
}

/// Merges each run of pointer moves into its last move. A move keeps the
/// button mask of the pointer event before it; `buttons` is the mask of the
/// last pointer event written, and this updates it.
fn merge(queued: impl IntoIterator<Item = Outgoing>, buttons: &mut u8) -> Vec<Outgoing> {
    let mut out: Vec<Outgoing> = Vec::new();
    let mut after_move = false;
    for item in queued {
        let is_move = match &item {
            Outgoing::Pointer(p) => std::mem::replace(buttons, p.buttons) == p.buttons,
            Outgoing::Message(_) => false,
        };
        match out.last_mut() {
            Some(last) if is_move && after_move => *last = item,
            _ => out.push(item),
        }
        after_move = is_move;
    }
    out
}

/// Writes what the queue brings until the socket fails or the session
/// closes the queue. A write fails only on a dead connection, which the
/// reader also sees, so it ends the session. Once the queue is closed and
/// empty, this resets the connection, so the host tears the session down at
/// once instead of waiting out its own timeouts.
pub(super) fn run(queue: Receiver<Outgoing>, mut stream: TcpStream, mut sealer: RecordSealer, record_key: [u8; 16]) {
    let mut buttons = 0;
    let mut bytes = Vec::new();
    while let Ok(first) = queue.recv() {
        bytes.clear();
        for item in merge(std::iter::once(first).chain(queue.try_iter()), &mut buttons) {
            let msg = match item {
                Outgoing::Pointer(p) => messages::encrypted_pointer_event(&record_key, p.buttons, p.x, p.y),
                Outgoing::Message(m) => m,
            };
            bytes.extend(sealer.seal(&msg));
        }
        if let Err(e) = stream.write_all(&bytes) {
            tracing::debug!("control send failed: {e}");
            return;
        }
    }
    let _ = socket2::SockRef::from(&stream).set_linger(Some(Duration::ZERO));
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::messages::{BTN_LEFT, encrypted_pointer_event, key_event};
    use crate::session::tests::{KEYS, host};

    fn at(buttons: u8, x: u16, y: u16) -> Outgoing {
        Outgoing::Pointer(Pointer { buttons, x, y })
    }

    #[test]
    fn move_move_click_move_sends_move_click_move_in_that_order() {
        let (client, mut host) = host();
        let (tx, rx) = crossbeam_channel::unbounded();
        // All of it waits in the queue before the writer starts, as it does
        // while the socket is busy.
        for item in [at(0, 10, 10), at(0, 20, 20), at(BTN_LEFT, 20, 20), at(0, 20, 20), at(0, 30, 30)] {
            tx.send(item).unwrap();
        }
        drop(tx);
        run(rx, client, RecordSealer::new(&KEYS), KEYS.key);
        let ptr = |buttons, x, y| Some(encrypted_pointer_event(&KEYS.key, buttons, x, y));
        assert_eq!(host.next(), ptr(0, 20, 20));
        assert_eq!(host.next(), ptr(BTN_LEFT, 20, 20));
        assert_eq!(host.next(), ptr(0, 20, 20));
        assert_eq!(host.next(), ptr(0, 30, 30));
        assert_eq!(host.next(), None);
    }

    #[test]
    fn a_drag_keeps_its_press_point_and_merges_only_its_moves() {
        let mut buttons = 0;
        let queued = [at(BTN_LEFT, 5, 5), at(BTN_LEFT, 6, 6), at(BTN_LEFT, 7, 7), at(0, 7, 7)];
        assert_eq!(merge(queued, &mut buttons), [at(BTN_LEFT, 5, 5), at(BTN_LEFT, 7, 7), at(0, 7, 7)]);
        assert_eq!(buttons, 0);
    }

    #[test]
    fn a_key_between_two_moves_keeps_both_moves() {
        let mut buttons = 0;
        let key = || Outgoing::Message(key_event(true, 0x61));
        assert_eq!(merge([at(0, 1, 1), key(), at(0, 2, 2), at(0, 3, 3)], &mut buttons), [at(0, 1, 1), key(), at(0, 3, 3)]);
    }

    #[test]
    fn a_held_button_from_the_last_batch_makes_the_next_event_a_move() {
        let mut buttons = BTN_LEFT;
        assert_eq!(merge([at(BTN_LEFT, 1, 1), at(BTN_LEFT, 2, 2)], &mut buttons), [at(BTN_LEFT, 2, 2)]);
    }

    #[test]
    fn two_steps_right_press_and_release_button_7_twice() {
        let p = |buttons| Pointer { buttons, x: 100, y: 50 };
        assert_eq!(scroll(p(0), (2, 0)), [p(0x40), p(0), p(0x40), p(0)]);
        assert_eq!(scroll(p(0), (-1, 0)), [p(0x20), p(0)]);
    }

    #[test]
    fn one_step_up_with_the_left_button_held_keeps_the_button() {
        let p = |buttons| Pointer { buttons, x: 1, y: 2 };
        assert_eq!(scroll(p(BTN_LEFT), (0, -1)), [p(BTN_LEFT | 0x08), p(BTN_LEFT)]);
    }

    #[test]
    fn scroll_steps_survive_the_merge() {
        let mut buttons = 0;
        let steps: Vec<_> = scroll(Pointer { buttons: 0, x: 1, y: 1 }, (0, 3)).into_iter().map(Outgoing::Pointer).collect();
        assert_eq!(merge(steps, &mut buttons).len(), 6);
    }
}
