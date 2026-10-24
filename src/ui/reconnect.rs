//! When to bring a dropped session back: what a session end means for the
//! window, and how long to wait between attempts.

use std::time::{Duration, Instant};

const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);
/// Stop reconnecting once this long has passed since the drop without a
/// picture.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10 * 60);

/// Doubling delay between reconnect attempts, capped at [`MAX_DELAY`].
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { next: FIRST_DELAY }
    }
}

impl Backoff {
    /// Delay before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (d * 2).min(MAX_DELAY);
        d
    }

    /// A session came up; the next drop starts again from the first delay.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// What the window does when a session ends.
#[derive(Debug, PartialEq, Eq)]
pub enum OnEnd {
    Exit,
    Retry(Duration),
    /// Reconnecting has gone on too long without a picture.
    GiveUp,
}

/// Session lifecycle as the window sees it.
#[derive(Debug, Default)]
pub struct Reconnector {
    enabled: bool,
    /// A session reached `Connected` at least once. Until then failures are
    /// reported and end the program, as without reconnecting.
    connected_once: bool,
    reconnecting: bool,
    backoff: Backoff,
    /// When the current outage began: the first drop since the latest
    /// picture. `None` while the session works.
    down_since: Option<Instant>,
}

impl Reconnector {
    pub fn new(enabled: bool) -> Self {
        Reconnector { enabled, ..Default::default() }
    }

    pub fn reconnecting(&self) -> bool {
        self.reconnecting
    }

    /// The session finished its handshake. That alone doesn't prove the
    /// host is healthy (it may accept and then send nothing), so the
    /// backoff keeps growing until [`Self::video`].
    pub fn connected(&mut self) {
        self.connected_once = true;
        self.reconnecting = false;
    }

    /// A picture arrived: the session really works, so the next drop
    /// starts a new outage, with the first delay and the full budget.
    pub fn video(&mut self) {
        self.down_since = None;
        self.backoff.reset();
    }

    /// The session ended; `retryable` is false for failures another attempt
    /// can't fix, such as rejected credentials.
    pub fn ended(&mut self, retryable: bool) -> OnEnd {
        self.ended_at(retryable, Instant::now())
    }

    fn ended_at(&mut self, retryable: bool, now: Instant) -> OnEnd {
        if !(self.enabled && self.connected_once && retryable) {
            return OnEnd::Exit;
        }
        let down_since = *self.down_since.get_or_insert(now);
        if now.duration_since(down_since) >= GIVE_UP_AFTER {
            return OnEnd::GiveUp;
        }
        self.reconnecting = true;
        OnEnd::Retry(self.backoff.next_delay())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> OnEnd {
        OnEnd::Retry(Duration::from_secs(s))
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = Backoff::default();
        let got: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(got, [1, 2, 4, 8, 16, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next_delay(), FIRST_DELAY);
    }

    #[test]
    fn initial_failure_exits() {
        let mut r = Reconnector::new(true);
        assert_eq!(r.ended(true), OnEnd::Exit);
        assert!(!r.reconnecting());
    }

    #[test]
    fn drop_after_connect_retries_until_connected() {
        let mut r = Reconnector::new(true);
        r.connected();
        assert_eq!(r.ended(true), secs(1));
        assert!(r.reconnecting());
        // Failed attempts keep backing off.
        assert_eq!(r.ended(true), secs(2));
        assert_eq!(r.ended(true), secs(4));
        r.connected();
        r.video();
        assert!(!r.reconnecting());
        assert_eq!(r.ended(true), secs(1));
    }

    #[test]
    fn backoff_resets_only_after_video() {
        let mut r = Reconnector::new(true);
        r.connected();
        r.video();
        assert_eq!(r.ended(true), secs(1));
        // Handshakes that succeed but never deliver video keep backing off.
        r.connected();
        assert_eq!(r.ended(true), secs(2));
        r.connected();
        assert_eq!(r.ended(true), secs(4));
        r.connected();
        r.video();
        assert_eq!(r.ended(true), secs(1));
    }

    #[test]
    fn auth_rejection_exits_even_while_reconnecting() {
        let mut r = Reconnector::new(true);
        r.connected();
        assert_eq!(r.ended(true), secs(1));
        assert_eq!(r.ended(false), OnEnd::Exit);
    }

    #[test]
    fn hour_long_session_gets_the_full_budget_at_its_first_drop() {
        let mut r = Reconnector::new(true);
        r.connected();
        r.video();
        // A still screen sends no picture for an hour, then the link drops.
        let drop = Instant::now() + Duration::from_secs(3600);
        assert_eq!(r.ended_at(true, drop), secs(1));
        assert_eq!(r.ended_at(true, drop + GIVE_UP_AFTER - Duration::from_secs(1)), secs(2));
        assert_eq!(r.ended_at(true, drop + GIVE_UP_AFTER), OnEnd::GiveUp);
    }

    #[test]
    fn second_drop_after_a_recovery_starts_a_new_budget() {
        let mut r = Reconnector::new(true);
        r.connected();
        r.video();
        // The first outage uses 9 of its 10 minutes, then video is back.
        let t0 = Instant::now();
        assert_eq!(r.ended_at(true, t0), secs(1));
        assert_eq!(r.ended_at(true, t0 + Duration::from_secs(540)), secs(2));
        r.connected();
        r.video();
        let second = t0 + Duration::from_secs(600);
        assert_eq!(r.ended_at(true, second), secs(1));
        assert_eq!(r.ended_at(true, second + GIVE_UP_AFTER - Duration::from_secs(1)), secs(2));
        assert_eq!(r.ended_at(true, second + GIVE_UP_AFTER), OnEnd::GiveUp);
    }

    #[test]
    fn handshake_without_a_picture_keeps_the_outage_budget() {
        let mut r = Reconnector::new(true);
        r.connected();
        r.video();
        let t0 = Instant::now();
        assert_eq!(r.ended_at(true, t0), secs(1));
        // The host accepts again but never sends a picture.
        r.connected();
        assert_eq!(r.ended_at(true, t0 + GIVE_UP_AFTER), OnEnd::GiveUp);
    }

    #[test]
    fn disabled_never_retries() {
        let mut r = Reconnector::new(false);
        r.connected();
        assert_eq!(r.ended(true), OnEnd::Exit);
    }
}
