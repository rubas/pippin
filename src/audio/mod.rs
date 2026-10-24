//! The Mac's sound: AAC-ELD decoding and playback.
//!
//! The session's `ctrl-rx` thread decodes each packet and pushes the PCM
//! into a [`Player`], which plays it on the default output device through
//! a small jitter buffer. The device is opened when sound starts and closed
//! after a few seconds of silence, since the host keeps sending silent
//! packets whenever nothing is playing. When the device fails, the next
//! try waits longer each time, up to a few seconds.

pub mod aac;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

use aac::{CHANNELS, SAMPLE_RATE};

/// Audio buffered before playback starts or resumes after an underrun.
/// Covers network jitter and the gap between packets (10 ms each).
const TARGET: Duration = Duration::from_millis(40);
/// Above this much buffered audio, the excess is dropped so a stall never
/// replays late audio and clock drift between the Mac and the sound card
/// can't build up delay.
const MAX_BUFFERED: Duration = Duration::from_millis(100);
/// Close the device after this long without sound.
const IDLE_CLOSE: Duration = Duration::from_secs(5);
/// Device period we ask for: 10 ms.
const PERIOD_FRAMES: u32 = 480;
/// After the output device fails, wait this long before it is opened again,
/// and double the wait on each failure in a row, up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(8);
/// How often the open device is checked for idle time and errors.
const CHECK_EVERY: Duration = Duration::from_millis(250);

fn samples(d: Duration) -> usize {
    (d.as_micros() as usize * SAMPLE_RATE as usize / 1_000_000) * CHANNELS
}

/// Interleaved samples waiting to be played.
#[derive(Default)]
struct Jitter {
    queue: VecDeque<i16>,
    /// Playing (rather than refilling after an underrun).
    primed: bool,
    last_sound: Option<Instant>,
    underruns: u64,
    dropped: u64,
}

impl Jitter {
    /// Queues one decoded packet. Returns whether it holds sound.
    fn push(&mut self, pcm: &[i16]) -> bool {
        let sound = pcm.iter().any(|&s| s != 0);
        // The host sends digital silence whenever nothing plays; it's
        // indistinguishable from the underrun fill, so skip it. Keep the
        // silence after a sound: it keeps the gap to the next sound, and it
        // fills a sound shorter than the target up to the target, else that
        // sound never starts. Before the device plays, the queue stops at
        // the target, so the trim never cuts the sound away.
        if sound {
            self.last_sound = Some(Instant::now());
        } else if self.queue.is_empty() || (!self.primed && self.queue.len() >= samples(TARGET)) {
            return false;
        }
        self.queue.extend(pcm);
        if self.queue.len() > samples(MAX_BUFFERED) {
            let excess = self.queue.len() - samples(TARGET);
            self.queue.drain(..excess);
            self.dropped += excess as u64;
        }
        sound
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.primed = false;
    }

    fn pull<T: SizedSample + FromSample<i16>>(&mut self, out: &mut [T]) {
        if !self.primed && self.queue.len() >= samples(TARGET) {
            self.primed = true;
        }
        let mut n = 0;
        if self.primed {
            n = out.len().min(self.queue.len());
            for (o, s) in out.iter_mut().zip(self.queue.drain(..n)) {
                *o = T::from_sample(s);
            }
            if n < out.len() {
                self.primed = false;
                self.underruns += 1;
            }
        }
        out[n..].fill(T::EQUILIBRIUM);
    }
}

struct Inner {
    jitter: Mutex<Jitter>,
    /// The output thread has (or is opening) a stream.
    running: AtomicBool,
    wake: Sender<()>,
}

/// Plays decoded audio. Cheap to clone; shared by every session of a run so
/// a reconnect keeps the same output thread.
#[derive(Clone)]
pub struct Player {
    inner: Arc<Inner>,
}

impl Player {
    /// Starts the output thread if audio can work at all: libfdk-aac loads
    /// and there is an output device.
    pub fn new() -> Result<Self> {
        aac::available()?;
        cpal::default_host().default_output_device().context("no audio output device")?;
        let (wake, rx) = crossbeam_channel::bounded(1);
        let inner = Arc::new(Inner { jitter: Mutex::default(), running: AtomicBool::new(false), wake });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || output_thread(weak, rx))
            .context("spawning audio thread")?;
        Ok(Self { inner })
    }

    /// Queues one decoded packet of interleaved stereo.
    pub fn push(&self, pcm: &[i16]) {
        let sound = self.inner.jitter.lock().push(pcm);
        if sound && !self.inner.running.swap(true, Ordering::AcqRel) {
            let _ = self.inner.wake.try_send(());
        }
    }

    /// Drops anything buffered, e.g. when the session it came from ends.
    pub fn flush(&self) {
        self.inner.jitter.lock().clear();
    }
}

/// How long to wait before the output device is opened again, after it
/// failed `failures` times in a row: 1 s, 2 s, 4 s, then 8 s.
fn retry_delay(failures: u32) -> Duration {
    RETRY_FIRST.saturating_mul(2u32.saturating_pow(failures)).min(RETRY_MAX)
}

/// Opens the device when sound starts and closes it once it stops. When
/// the device fails to open or fails while it plays, waits [`retry_delay`]
/// before the next try. Owns the stream because cpal streams can't move
/// between threads. Exits when the last [`Player`] is dropped.
fn output_thread(inner: std::sync::Weak<Inner>, wake: Receiver<()>) {
    let mut failures = 0;
    while wake.recv().is_ok() {
        let Some(shared) = inner.upgrade() else { return };
        let opened = Instant::now();
        // `_stream` keeps the device open until `play_until_idle` returns.
        match open_stream(shared.clone()).and_then(|(_stream, errors)| play_until_idle(&shared, &errors)) {
            Ok(()) => failures = 0,
            Err(e) => {
                // A stream that played longer than the longest wait ends
                // the run of failures.
                if opened.elapsed() > RETRY_MAX {
                    failures = 0;
                }
                let delay = retry_delay(failures);
                if failures == 0 {
                    tracing::warn!("no sound: {e:#}; next try in {delay:?}");
                } else {
                    tracing::debug!("audio output: {e:#}; next try in {delay:?}");
                }
                failures += 1;
                std::thread::sleep(delay);
                // Sound that came during the wait is late now.
                shared.jitter.lock().clear();
            }
        }
        shared.running.store(false, Ordering::Release);
        // Pushes that saw `running` still set while we were closing didn't
        // wake us; their sound would sit unplayed until the next one.
        if shared.jitter.lock().last_sound.is_some_and(|t| t.elapsed() < IDLE_CLOSE)
            && !shared.running.swap(true, Ordering::AcqRel)
        {
            let _ = shared.wake.try_send(());
        }
    }
}

/// Keeps the stream open until no sound came for [`IDLE_CLOSE`], or until
/// the stream reports an error.
fn play_until_idle(shared: &Inner, errors: &Receiver<cpal::StreamError>) -> Result<()> {
    tracing::debug!("audio output opened");
    loop {
        // The stream holds the sender, so the channel can't disconnect.
        if let Ok(e) = errors.recv_timeout(CHECK_EVERY) {
            return Err(e).context("audio output failed");
        }
        let mut j = shared.jitter.lock();
        if j.last_sound.is_none_or(|t| t.elapsed() > IDLE_CLOSE) {
            tracing::debug!("audio output closed ({} underruns, {} samples dropped)", j.underruns, j.dropped);
            // Under the same lock as the check: a sound pushed after it
            // stays queued for the next stream.
            j.clear();
            return Ok(());
        }
    }
}

/// Opens and starts the output stream. The receiver gets the stream's
/// errors; after one, the stream is dead.
fn open_stream(inner: Arc<Inner>) -> Result<(cpal::Stream, Receiver<cpal::StreamError>)> {
    let device = cpal::default_host().default_output_device().context("no audio output device")?;
    let rate = cpal::SampleRate(SAMPLE_RATE);
    let supported = device
        .supported_output_configs()?
        .filter(|c| c.channels() as usize == CHANNELS && c.min_sample_rate() <= rate && rate <= c.max_sample_rate())
        .max_by_key(|c| match c.sample_format() {
            SampleFormat::F32 => 2,
            SampleFormat::I16 => 1,
            _ => 0,
        })
        .context("the output device doesn't do 48 kHz stereo")?
        .with_sample_rate(rate);
    let mut config = supported.config();
    if let cpal::SupportedBufferSize::Range { min, max } = supported.buffer_size() {
        config.buffer_size = cpal::BufferSize::Fixed(PERIOD_FRAMES.clamp(*min, *max));
    }
    let (tx, errors) = crossbeam_channel::bounded(1);
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, inner, tx)?,
        SampleFormat::I16 => build::<i16>(&device, &config, inner, tx)?,
        f => return Err(anyhow!("unsupported sample format {f}")),
    };
    stream.play()?;
    Ok((stream, errors))
}

fn build<T: SizedSample + FromSample<i16>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    inner: Arc<Inner>,
    errors: Sender<cpal::StreamError>,
) -> Result<cpal::Stream> {
    Ok(device.build_output_stream(
        config,
        move |out: &mut [T], _| inner.jitter.lock().pull(out),
        // A dead device reports the same error over and over; the first
        // one closes the stream.
        move |e| {
            let _ = errors.try_send(e);
        },
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_target_then_plays() {
        let mut j = Jitter::default();
        let packet = vec![1000i16; 480 * CHANNELS];
        let mut out = vec![0f32; 240 * CHANNELS];
        j.push(&packet);
        j.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "plays before the buffer reaches the target");
        for _ in 0..3 {
            j.push(&packet);
        }
        j.pull(&mut out);
        assert!(out.iter().all(|&s| s > 0.0));
    }

    #[test]
    fn underrun_refills_before_resuming() {
        let mut j = Jitter::default();
        let packet = vec![1000i16; 480 * CHANNELS];
        for _ in 0..4 {
            j.push(&packet);
        }
        let mut out = vec![0f32; 2000 * CHANNELS];
        j.pull(&mut out);
        assert_eq!(j.underruns, 1);
        assert!(out[..1920 * CHANNELS].iter().all(|&s| s > 0.0));
        assert!(out[1920 * CHANNELS..].iter().all(|&s| s == 0.0));
        j.push(&packet);
        j.pull(&mut out[..2]);
        assert_eq!(out[0], 0.0, "resumed with less than the target buffered");
    }

    #[test]
    fn backlog_is_trimmed_to_target() {
        let mut j = Jitter::default();
        let packet = vec![1i16; 480 * CHANNELS];
        for _ in 0..11 {
            j.push(&packet);
        }
        assert_eq!(j.queue.len(), samples(TARGET));
    }

    /// Feeds `input` as 10 ms packets, `#` for sound and `.` for host
    /// silence, and then 1 s of silence. The device opens after `open_after`
    /// packets and then pulls one 10 ms period after each packet. Returns
    /// what it played in the same notation, from the first to the last sound.
    fn play(input: &str, open_after: usize) -> String {
        let mut j = Jitter::default();
        let sound = vec![1000i16; 480 * CHANNELS];
        let silence = vec![0i16; 480 * CHANNELS];
        let mut out = vec![0f32; 480 * CHANNELS];
        let mut played = String::new();
        for (i, c) in input.chars().chain(std::iter::repeat_n('.', 100)).enumerate() {
            j.push(if c == '#' { &sound } else { &silence });
            if i >= open_after {
                j.pull(&mut out);
                played.push(if out.iter().all(|&s| s > 0.0) { '#' } else { '.' });
            }
        }
        played.trim_matches('.').to_string()
    }

    #[test]
    fn a_50ms_sound_plays_in_full() {
        assert_eq!(play("#####", 0), "#####");
    }

    #[test]
    fn a_20ms_click_plays_in_full() {
        assert_eq!(play("##", 0), "##");
    }

    #[test]
    fn a_20ms_click_plays_when_the_device_opens_150ms_late() {
        assert_eq!(play("##", 15), "##");
    }

    #[test]
    fn a_20ms_gap_between_two_sounds_is_kept() {
        assert_eq!(play("####..####", 0), "####..####");
    }

    #[test]
    fn host_silence_alone_queues_nothing() {
        let mut j = Jitter::default();
        assert!(!j.push(&[0i16; 480 * CHANNELS]));
        assert!(j.queue.is_empty());
    }

    #[test]
    fn device_retries_wait_1_2_4_then_8_seconds() {
        let delays: Vec<u64> = (0..6).map(|failures| retry_delay(failures).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 8, 8]);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;

    /// Needs a sound device; plays inaudible (±1 LSB) samples.
    #[test]
    #[ignore]
    fn plays_on_default_device() {
        let p = Player::new().unwrap();
        let packet: Vec<i16> = (0..480 * CHANNELS).map(|i| if i % 4 < 2 { 1 } else { -1 }).collect();
        for _ in 0..100 {
            p.push(&packet);
            std::thread::sleep(Duration::from_millis(10));
        }
        let j = p.inner.jitter.lock();
        assert!(p.inner.running.load(Ordering::Acquire));
        assert!(j.queue.len() <= samples(MAX_BUFFERED), "device isn't consuming: {}", j.queue.len());
        eprintln!("buffered {} samples, {} underruns, {} dropped", j.queue.len(), j.underruns, j.dropped);
    }
}
