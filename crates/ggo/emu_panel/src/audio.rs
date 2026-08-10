//! Native audio output for the emulator pane: stream the APU's mixed ring
//! to the default `cpal` output device, with a mute toggle and an underrun
//! counter the pane surfaces.
//!
//! # Why the pane owns an output stream at all (F5.4 R6's open question)
//!
//! F3 deferred audio and named the standalone `ggo-emu` binary as the
//! with-audio path (see [`crate::drive`]'s "what is deliberately not
//! ported"). The reason to reopen it is structural, not a change of taste.
//!
//! `ggo-ide`'s emulator page keeps ONE persistent emu thread for the whole
//! life of the page and opens the device on it at spawn time
//! (`emu/thread.rs::spawn_with_audio`), so the device stays open across
//! every idle stretch between runs -- exactly the failure mode a docked
//! panel must not have, and a real reason to have deferred it there. This
//! fork's [`crate::drive::start`] spawns a **per-run** thread that
//! terminates when the run ends. Opening the device inside that thread's
//! body makes the device's lifetime *identical* to the run's, by
//! construction: there is no "close it when idle" bookkeeping to get
//! wrong, because there is no thread to hold it once the run is over.
//! [`AudioOut`] is a plain local in [`crate::drive`]'s run loop; its `Drop`
//! (which stops the stream and releases the device) runs on the way out of
//! that function, on the same thread that opened it.
//!
//! Deferring to the standalone binary instead would mean "audio works if
//! you leave the editor and run the cart in a terminal", which is not
//! audio in the panel. The APU already *runs* inside a pane run
//! (`runtime.rs::handle_vsync_wait` advances it one frame per
//! `vsync_wait`, which is what makes the perf sim's APU counters real);
//! its samples were simply being mixed and discarded. The whole cost of
//! wiring them up is one `Apu::copy_since` per presented frame.
//!
//! ## The tap: emu thread -> `RingWriter` -> `RingReader` -> cpal callback
//!
//! `ggo_emu_core::apu::Apu` lives inside `Peripherals`, entirely on the
//! emulator thread. The cpal realtime callback runs on a separate thread
//! cpal itself owns, so samples have to be *copied* across rather than
//! shared by reference. [`channel`] returns a cheap [`RingWriter`]/
//! [`RingReader`] pair sharing a small `Mutex<VecDeque<i16>>` -- the same
//! queue design `ggo-emu/src/audio.rs` uses in production, and a dedicated
//! mutex held by nobody else, so the callback never waits on anything the
//! emulator thread holds while stepping.
//!
//! **The queue mutex is locked exactly once per cpal callback invocation**
//! (in [`build_stream`]), not once per resampled sample: [`fill`] -- the
//! pure drain step, and the one piece of this module unit-testable with no
//! device at all -- takes the already-locked `VecDeque` rather than
//! locking anything itself.
//!
//! [`RingWriter::push`] caps the queue at [`MAX_QUEUED`] by dropping the
//! OLDEST samples, so a callback that never runs (no device, a stalled
//! host) can't grow it without bound, and a reader that resumes gets
//! bounded latency rather than an ever-growing backlog.
//!
//! There is deliberately no "reset the ring for a new run" call, which
//! `ggo-ide`'s equivalent needs on every `EmuCmd::Start` (its emu thread
//! is persistent, so a fault's last half-second of samples would otherwise
//! play ahead of the next run's first real audio). Here [`channel`] is
//! called inside the per-run thread and both halves are locals of that
//! run: a new run cannot inherit a previous run's queue because the
//! previous run's queue no longer exists.
//!
//! ## Mute is a *submission* gate, not just a volume of zero
//!
//! [`RingWriter::push`] drops the frame's samples while muted AND clears
//! whatever is still queued, so unmuting resumes from live audio instead
//! of replaying up to [`MAX_QUEUED`] of what was mixed while silent. The
//! reader side reads the same flag through [`AudioStatus::is_live`], so a
//! muted run emits clean silence and -- critically -- never counts an
//! underrun: without that, the counter would climb at device-callback rate
//! (tens of thousands per second) for as long as the user left it muted,
//! which would make the one number this feature exists to make debuggable
//! meaningless.
//!
//! ## No device is a normal machine, not an error
//!
//! [`start_output`] never returns an error and never panics. A host with
//! no usable output device (CI, a session with no sound server) records
//! the reason in [`AudioStatus`] and the run proceeds silently --
//! [`AudioState::Unavailable`] is what the pane renders, carrying that
//! reason verbatim rather than a bare "no audio".
//!
//! ## Thread affinity (the Windows COM apartment rule)
//!
//! cpal's WASAPI backend touches COM to enumerate and open a device, and
//! COM apartments are thread-affine: initialising one on a thread winit
//! has already initialised differently does not fail cleanly, it can panic
//! or wedge the message pump. So [`AudioOut::start`] is only ever called
//! from the emulator thread -- a plain, uninitialised OS thread -- never
//! from the UI thread, and the resulting `cpal::Stream` never leaves it.
//! Only the plain atomics in [`AudioStatus`] cross back to the UI thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

/// Cap on buffered stereo samples (i16 elements) between the emulator
/// thread's [`RingWriter`] and the cpal callback's [`RingReader`]: two ring
/// lengths, mirroring `ggo-emu/src/audio.rs::MAX_QUEUED` exactly, for the
/// same reason -- bound latency by dropping the oldest samples rather than
/// drifting an ever-larger backlog.
const MAX_QUEUED: usize = ggo_emu_core::apu::RING_LEN * 2;

/// One interleaved stereo sample pair -- the unit [`fill`] resamples in.
const STEREO_PAIR: usize = 2;

// ------------------------------------------------------------ shared state

/// What the pane shows about audio, derived from [`AudioStatus`].
///
/// A single enum rather than a bag of booleans because the states are
/// genuinely exclusive and their *labels* differ: "no run has opened a
/// device yet" must not read the same as "this machine has no device", and
/// neither may read as "muted by the user".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioState {
    /// No run has tried to open a device yet -- nothing to say.
    Idle,
    /// The device could not be opened. Carries cpal's own reason verbatim
    /// (see [`start_output`]); the run is silent but otherwise unaffected.
    Unavailable(String),
    /// A device is (or was) open for this run.
    Live { muted: bool, underruns: u32 },
}

impl AudioState {
    /// The stats-row segment, or `None` when there is nothing worth a
    /// line. `Idle` is the only silent state: an underrun count of zero is
    /// still the diagnostic doing its job, so it is always shown once a
    /// device is open.
    pub fn label(&self) -> Option<String> {
        match self {
            AudioState::Idle => None,
            AudioState::Unavailable(reason) => Some(format!("audio unavailable — {reason}")),
            AudioState::Live { muted, underruns } => Some(format!(
                "audio {} · underruns {underruns}",
                if *muted { "muted" } else { "on" }
            )),
        }
    }

    /// Whether the pane can offer a working mute toggle. False for
    /// [`AudioState::Unavailable`], where the run is already silent and
    /// there is nothing for the button to change.
    pub fn is_toggleable(&self) -> bool {
        !matches!(self, AudioState::Unavailable(_))
    }
}

/// The atomics behind [`AudioStatus`]. Split out only so `AudioStatus`
/// itself is a cheap `Clone` of one `Arc`.
#[derive(Default)]
struct Shared {
    /// Set once [`AudioOut::start`] has succeeded on the emulator thread.
    available: AtomicBool,
    /// UI-toggled, read by both ends of the ring every frame/callback.
    /// Deliberately NOT cleared by [`AudioStatus::reset_for_run`]: mute is
    /// a user preference, not run state.
    muted: AtomicBool,
    /// Count of [`fill`] calls that had to emit at least one silent sample
    /// while live.
    underruns: AtomicU32,
    /// Whether the emulator thread is actively producing samples right
    /// now. See [`fill`] for why the callback needs this.
    running: AtomicBool,
    /// Why the device could not be opened. Written once by the emulator
    /// thread, read by the UI thread; the realtime callback never touches
    /// it, which is why a `Mutex` is fine here and nowhere else.
    reason: Mutex<Option<String>>,
}

/// Cross-thread audio state and controls: read and toggled by the UI
/// thread, read by the cpal callback, written by the emulator thread.
///
/// Owned by the *panel*, not by a run, so the mute preference survives
/// across runs and can be set before one ever starts. A clone is handed to
/// [`crate::drive::start`], which is what connects it to a live device.
#[derive(Clone, Default)]
pub struct AudioStatus {
    shared: Arc<Shared>,
}

impl AudioStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    /// Set mute directly. Test-only: the pane's only production path is
    /// [`Self::toggle_mute`] (a button and an action, both toggles), so a
    /// setter with no caller would be speculative API.
    #[cfg(test)]
    pub(crate) fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    /// Flip the mute flag, returning the NEW state (so a caller can report
    /// it without a second load that could race a concurrent toggle).
    pub fn toggle_mute(&self) -> bool {
        !self.shared.muted.fetch_xor(true, Ordering::Relaxed)
    }

    pub fn underruns(&self) -> u32 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// Whether samples should actually be flowing right now: a run is
    /// stepping AND the user has not muted it. Both ends of the ring gate
    /// on this -- see this module's doc on mute being a submission gate.
    pub fn is_live(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed) && !self.is_muted()
    }

    /// Mirror whether the emulator thread is stepping frames. `false`
    /// before the device opens and after the run loop exits.
    pub fn set_running(&self, running: bool) {
        self.shared.running.store(running, Ordering::Relaxed);
    }

    /// The emulator thread's report that the device opened. Also the
    /// cheapest way for a test elsewhere in the crate to put the status
    /// into the device-open state without a device.
    pub fn mark_available(&self) {
        *self.shared.reason.lock().unwrap() = None;
        self.shared.available.store(true, Ordering::Relaxed);
    }

    /// The emulator thread's report that it could not open a device, with
    /// the reason the pane will show. Public because it is also the
    /// cheapest way for a test to put the status into the device-less
    /// state without a device-less machine.
    pub fn mark_unavailable(&self, reason: impl Into<String>) {
        *self.shared.reason.lock().unwrap() = Some(reason.into());
        self.shared.available.store(false, Ordering::Relaxed);
    }

    /// Zero the per-run counters and clear any previous run's failure
    /// reason, so a machine that grew a sound card between runs is not
    /// still reporting the old excuse. Keeps `muted` -- see [`Shared`].
    pub fn reset_for_run(&self) {
        self.shared.underruns.store(0, Ordering::Relaxed);
        self.shared.available.store(false, Ordering::Relaxed);
        *self.shared.reason.lock().unwrap() = None;
    }

    /// Count an underrun by hand. Test-only: the real increments happen
    /// inside [`fill`] on the cpal callback's own thread, which no test
    /// can drive, so this is how a test elsewhere in the crate produces a
    /// non-zero count to assert the pane surfaces.
    #[cfg(test)]
    pub(crate) fn record_underrun(&self) {
        self.shared.underruns.fetch_add(1, Ordering::Relaxed);
    }

    /// What the pane renders. `Unavailable` outranks `muted`: a run with
    /// no device is silent for a reason the user cannot toggle away.
    pub fn state(&self) -> AudioState {
        if let Some(reason) = self.shared.reason.lock().unwrap().clone() {
            return AudioState::Unavailable(reason);
        }
        if !self.shared.available.load(Ordering::Relaxed) {
            return AudioState::Idle;
        }
        AudioState::Live {
            muted: self.is_muted(),
            underruns: self.underruns(),
        }
    }
}

// ------------------------------------------------------------------- ring

/// The emulator-thread half of the tap: push each presented frame's
/// freshly-mixed samples in.
#[derive(Clone)]
pub struct RingWriter {
    queue: Arc<Mutex<VecDeque<i16>>>,
    status: AudioStatus,
}

impl RingWriter {
    /// Append `samples` (interleaved stereo), trimming back under
    /// [`MAX_QUEUED`] from the front on overflow. The trim takes an EVEN
    /// count so L/R phase is never split across the cut.
    ///
    /// While muted this submits nothing and clears whatever is queued --
    /// see this module's doc.
    pub fn push(&self, samples: &[i16]) {
        if self.status.is_muted() {
            let mut queue = self.queue.lock().unwrap();
            queue.clear();
            return;
        }
        if samples.is_empty() {
            return;
        }
        let mut queue = self.queue.lock().unwrap();
        queue.extend(samples.iter().copied());
        let excess = queue.len().saturating_sub(MAX_QUEUED);
        if excess > 0 {
            queue.drain(..(excess + (excess & 1)));
        }
    }
}

/// The cpal-callback half of the tap.
pub struct RingReader {
    queue: Arc<Mutex<VecDeque<i16>>>,
    status: AudioStatus,
}

impl RingReader {
    /// How many samples are currently queued. Only the tests read this --
    /// it is the observable effect of [`RingWriter::push`]'s mute gate and
    /// cap trim (and, from [`crate::drive`]'s tests, of the per-frame APU
    /// tap) without exposing the queue itself as real API.
    #[cfg(test)]
    pub(crate) fn queued_len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

/// Build a fresh, empty tap sharing `status`. Cheap -- two small `Arc`s --
/// and it touches no audio hardware, so it is safe to call unconditionally
/// whether or not an [`AudioOut`] ever starts.
pub fn channel(status: AudioStatus) -> (RingWriter, RingReader) {
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    (
        RingWriter {
            queue: Arc::clone(&queue),
            status: status.clone(),
        },
        RingReader { queue, status },
    )
}

/// Pure drain decision for one already-locked block of interleaved stereo
/// samples.
///
/// - `live == false` (not stepping, or muted): ALWAYS emits silence, never
///   touches `underruns`, and never drains the queue -- no matter what is
///   still sitting in it. Without this, a paused or muted run would count
///   an underrun at device-callback rate for as long as it stayed that
///   way, and stale samples would play back as if the run were still
///   producing them.
/// - `live == true`: drains `out.len()` samples oldest-first, pads the
///   remainder with silence, and increments `underruns` ONCE (not once per
///   silent sample) if the queue came up short.
///
/// No locking here on purpose: `queue` is a plain `&mut VecDeque`, so
/// [`build_stream`] can lock the ring once per callback and pass the guard
/// straight through. That is also what makes this function -- where all the
/// underrun accounting lives -- unit-testable with no device and no thread.
pub fn fill(out: &mut [i16], queue: &mut VecDeque<i16>, live: bool, underruns: &AtomicU32) {
    if !live {
        out.fill(0);
        return;
    }
    let mut underran = false;
    for slot in out.iter_mut() {
        *slot = match queue.pop_front() {
            Some(sample) => sample,
            None => {
                underran = true;
                0
            }
        };
    }
    if underran {
        underruns.fetch_add(1, Ordering::Relaxed);
    }
}

// ----------------------------------------------------------------- device

/// A running cpal output stream fed from a [`RingReader`]. Holds the
/// `cpal::Stream` for its `Drop` (which stops the stream and releases the
/// device) plus the device's real sample rate for a log line. Must live and
/// die on the thread that created it -- see this module's doc.
pub struct AudioOut {
    _stream: cpal::Stream,
    pub device_rate: u32,
}

impl AudioOut {
    /// Open the default output device and start streaming `ring`,
    /// resampling from `mix_rate` (the APU's fixed rate) to whatever the
    /// device reports. `Err` -- never a panic -- when there is no usable
    /// device or its sample format is not one cpal's `FromSample<i16>`
    /// covers. [`start_output`] is the caller that turns that into the
    /// pane's degraded state; nothing else should call this directly.
    ///
    /// Must only be called from the emulator thread.
    fn start(ring: RingReader, mix_rate: u32) -> Result<AudioOut, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        // cpal 0.17 reports the rate as a plain `u32` (0.15's `SampleRate`
        // newtype is gone) -- ggo-ide is pinned to the older crate and
        // still unwraps a `.0` here.
        let device_rate = config.sample_rate();
        if device_rate == 0 {
            return Err("the output device reports a 0 Hz sample rate".to_string());
        }
        let channels = config.channels() as usize;
        if channels == 0 {
            return Err("the output device reports 0 channels".to_string());
        }
        let stream_config: cpal::StreamConfig = config.config();
        let stream = match config.sample_format() {
            cpal::SampleFormat::I16 => build_stream::<i16>(
                &device,
                &stream_config,
                channels,
                device_rate,
                mix_rate,
                ring,
            ),
            cpal::SampleFormat::U16 => build_stream::<u16>(
                &device,
                &stream_config,
                channels,
                device_rate,
                mix_rate,
                ring,
            ),
            cpal::SampleFormat::F32 => build_stream::<f32>(
                &device,
                &stream_config,
                channels,
                device_rate,
                mix_rate,
                ring,
            ),
            other => return Err(format!("unsupported output sample format: {other:?}")),
        }
        .ok_or_else(|| "the output stream could not be built".to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(AudioOut {
            _stream: stream,
            device_rate,
        })
    }
}

/// Open the device for a run, recording the outcome in `status`.
///
/// Infallible by design: a machine with no audio device is normal (CI, a
/// session with no sound server), and a run must never fail or hang
/// because of it. `None` means the run is silent and
/// [`AudioStatus::state`] now reports [`AudioState::Unavailable`] with the
/// reason; the ring still exists and [`RingWriter::push`] still bounds
/// itself, so nothing else in the drive loop needs a second code path.
///
/// Must only be called from the emulator thread -- see this module's doc.
pub fn start_output(status: &AudioStatus, ring: RingReader, mix_rate: u32) -> Option<AudioOut> {
    match AudioOut::start(ring, mix_rate) {
        Ok(out) => {
            status.mark_available();
            Some(out)
        }
        Err(reason) => {
            status.mark_unavailable(reason);
            None
        }
    }
}

/// Build the typed output stream: resamples mix-rate stereo pairs from
/// `ring` to `device_rate` through an integer phase accumulator
/// (repeat/skip), the same resampler math `ggo-emu/src/audio.rs` uses.
///
/// The ring's mutex and the live flag are read exactly ONCE per callback,
/// however many source pairs the resample loop below consumes.
fn build_stream<T: SizedSample + FromSample<i16>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    device_rate: u32,
    mix_rate: u32,
    ring: RingReader,
) -> Option<cpal::Stream> {
    // Resampler state: integer phase accumulator + the currently-held pair.
    let mut acc: u32 = 0;
    let mut cur = [0i16; STEREO_PAIR];
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let mut queue = ring.queue.lock().unwrap();
                // One read for the whole callback. Mute is folded in here
                // rather than zeroing each output frame separately, so a
                // muted run takes exactly the same silent, uncounted path
                // a paused one does.
                let live = ring.status.is_live();
                let underruns = &ring.status.shared.underruns;
                for frame in data.chunks_mut(channels) {
                    // Consume mix_rate/device_rate input pairs per output
                    // frame (repeat when slower, skip when faster).
                    acc += mix_rate;
                    while acc >= device_rate {
                        acc -= device_rate;
                        fill(&mut cur, &mut queue, live, underruns);
                    }
                    frame[0] = T::from_sample(cur[0]);
                    if channels >= STEREO_PAIR {
                        frame[1] = T::from_sample(cur[1]);
                    }
                    for extra in frame.iter_mut().skip(STEREO_PAIR) {
                        *extra = T::from_sample(0i16);
                    }
                }
            },
            |err| log::warn!("ggo emu panel: audio stream error: {err}"),
            None,
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bogus starting count, so the tests prove `fill` increments FROM the
    /// existing value rather than resetting it.
    const SEED_UNDERRUNS: u32 = 5;

    fn live_channel() -> (RingWriter, RingReader, AudioStatus) {
        let status = AudioStatus::new();
        status.set_running(true);
        let (writer, reader) = channel(status.clone());
        (writer, reader, status)
    }

    // ------------------------------------------------- fill / underruns

    #[test]
    fn fill_drains_available_samples_without_touching_the_underrun_counter() {
        let mut queue: VecDeque<i16> = VecDeque::from(vec![1, 2, 3, 4, 5, 6]);
        let underruns = AtomicU32::new(0);

        let mut out = [0i16; 4];
        fill(&mut out, &mut queue, true, &underruns);
        assert_eq!(
            out,
            [1, 2, 3, 4],
            "live and behind the writer: oldest first"
        );
        assert_eq!(underruns.load(Ordering::Relaxed), 0);

        let mut out = [0i16; 2];
        fill(&mut out, &mut queue, true, &underruns);
        assert_eq!(out, [5, 6], "the remainder is still queued for next time");
        assert_eq!(underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn fill_emits_silence_and_counts_one_underrun_when_live_and_caught_up() {
        let mut queue: VecDeque<i16> = VecDeque::new();
        let underruns = AtomicU32::new(SEED_UNDERRUNS);

        let mut out = [7i16; 4]; // non-zero sentinel, so silence is provable
        fill(&mut out, &mut queue, true, &underruns);

        assert_eq!(out, [0, 0, 0, 0]);
        assert_eq!(
            underruns.load(Ordering::Relaxed),
            SEED_UNDERRUNS + 1,
            "exactly one underrun for the call, added to the existing count"
        );
    }

    #[test]
    fn fill_pads_a_partial_drain_with_silence_and_still_counts_one_underrun() {
        let mut queue: VecDeque<i16> = VecDeque::from(vec![9, 9]); // one pair
        let underruns = AtomicU32::new(0);

        let mut out = [1i16; 6]; // asks for three
        fill(&mut out, &mut queue, true, &underruns);

        assert_eq!(out, [9, 9, 0, 0, 0, 0], "what was there, then silence");
        assert_eq!(
            underruns.load(Ordering::Relaxed),
            1,
            "one underrun for the whole short call, not one per silent sample"
        );
    }

    #[test]
    fn fill_emits_silence_and_never_counts_or_drains_while_not_live() {
        let mut queue: VecDeque<i16> = VecDeque::from(vec![9, 9, 9, 9]);
        let underruns = AtomicU32::new(0);

        let mut out = [7i16; 4];
        fill(&mut out, &mut queue, false, &underruns);

        assert_eq!(out, [0, 0, 0, 0], "silence regardless of the leftovers");
        assert_eq!(underruns.load(Ordering::Relaxed), 0);
        assert_eq!(queue.len(), 4, "the queue is left untouched, not drained");
    }

    /// The steady state that would otherwise spin the counter at
    /// device-callback rate: nothing queued and nothing live.
    #[test]
    fn fill_does_not_spin_the_underrun_counter_on_an_empty_queue_while_not_live() {
        let mut queue: VecDeque<i16> = VecDeque::new();
        let underruns = AtomicU32::new(0);

        for _ in 0..1_000 {
            let mut out = [1i16; 2];
            fill(&mut out, &mut queue, false, &underruns);
            assert_eq!(out, [0, 0]);
        }
        assert_eq!(
            underruns.load(Ordering::Relaxed),
            0,
            "1000 calls, zero counted"
        );
    }

    // ------------------------------------------------------- RingWriter

    #[test]
    fn push_is_a_no_op_on_an_empty_slice() {
        let (writer, reader, _status) = live_channel();
        writer.push(&[]);
        assert_eq!(reader.queued_len(), 0);
    }

    #[test]
    fn push_drops_the_oldest_samples_past_the_cap() {
        let (writer, reader, _status) = live_channel();
        // Values count up, so the survivors are identifiable.
        let overflow: Vec<i16> = (0..(MAX_QUEUED as i32 + 10)).map(|v| v as i16).collect();
        writer.push(&overflow);

        assert!(
            reader.queued_len() <= MAX_QUEUED,
            "the queue must stay bounded, got {}",
            reader.queued_len()
        );
        let underruns = AtomicU32::new(0);
        let mut first = [0i16; 2];
        let mut queue = reader.queue.lock().unwrap();
        fill(&mut first, &mut queue, true, &underruns);
        assert!(
            first[0] as i32 > 0,
            "the oldest (lowest-valued) samples were the ones trimmed, got {first:?}"
        );
    }

    // ------------------------------------------------------------- mute

    /// Mute is a submission gate: nothing new is queued, and what WAS
    /// queued is dropped so unmuting resumes from live audio.
    #[test]
    fn mute_stops_submitting_frames_and_clears_what_was_already_queued() {
        let (writer, reader, status) = live_channel();
        writer.push(&[1, 2, 3, 4]);
        assert_eq!(reader.queued_len(), 4);

        status.set_muted(true);
        writer.push(&[5, 6, 7, 8]);
        assert_eq!(
            reader.queued_len(),
            0,
            "a muted run submits nothing and keeps nothing"
        );

        status.set_muted(false);
        writer.push(&[9, 10]);
        assert_eq!(reader.queued_len(), 2, "unmuting resumes submission");
    }

    /// The counter must not become meaningless while muted -- the muted
    /// run's empty queue is not an underrun, it is the point.
    #[test]
    fn a_muted_run_is_not_live_so_the_underrun_counter_never_moves() {
        let (writer, reader, status) = live_channel();
        status.set_muted(true);
        writer.push(&[1, 2, 3, 4]);

        for _ in 0..1_000 {
            let mut out = [7i16; 2];
            let mut queue = reader.queue.lock().unwrap();
            fill(
                &mut out,
                &mut queue,
                status.is_live(),
                &status.shared.underruns,
            );
            assert_eq!(out, [0, 0], "muted output is silence");
        }
        assert_eq!(status.underruns(), 0);
    }

    #[test]
    fn toggle_mute_flips_and_reports_the_new_state() {
        let status = AudioStatus::new();
        assert!(!status.is_muted(), "runs start unmuted");
        assert!(status.toggle_mute(), "toggling reports the NEW state");
        assert!(status.is_muted());
        assert!(!status.toggle_mute());
        assert!(!status.is_muted());
    }

    #[test]
    fn is_live_needs_both_a_running_run_and_no_mute() {
        let status = AudioStatus::new();
        assert!(!status.is_live(), "not running yet");
        status.set_running(true);
        assert!(status.is_live());
        status.set_muted(true);
        assert!(!status.is_live(), "muted while running is not live");
        status.set_muted(false);
        status.set_running(false);
        assert!(!status.is_live(), "unmuted but stopped is not live");
    }

    // ------------------------------------------------------ AudioState

    #[test]
    fn state_is_idle_before_a_run_opens_a_device() {
        let status = AudioStatus::new();
        assert_eq!(status.state(), AudioState::Idle);
        assert_eq!(status.state().label(), None, "idle says nothing");
    }

    /// The device-less machine: the reason is what the pane shows, and it
    /// outranks the mute flag because the user cannot toggle it away.
    #[test]
    fn an_absent_device_yields_the_reason_state() {
        let status = AudioStatus::new();
        status.mark_unavailable("no default output device");

        assert_eq!(
            status.state(),
            AudioState::Unavailable("no default output device".into())
        );
        assert_eq!(
            status.state().label().as_deref(),
            Some("audio unavailable — no default output device")
        );
        assert!(
            !status.state().is_toggleable(),
            "there is nothing for the mute button to change"
        );

        status.set_muted(true);
        assert!(
            matches!(status.state(), AudioState::Unavailable(_)),
            "an unavailable device outranks the mute preference"
        );
    }

    #[test]
    fn an_open_device_reports_mute_and_the_underrun_count() {
        let status = AudioStatus::new();
        status.mark_available();
        assert_eq!(
            status.state(),
            AudioState::Live {
                muted: false,
                underruns: 0
            }
        );
        assert_eq!(
            status.state().label().as_deref(),
            Some("audio on · underruns 0")
        );
        assert!(status.state().is_toggleable());

        status.set_muted(true);
        status.shared.underruns.store(3, Ordering::Relaxed);
        assert_eq!(
            status.state().label().as_deref(),
            Some("audio muted · underruns 3")
        );
    }

    #[test]
    fn reset_for_run_clears_the_counters_and_the_reason_but_keeps_the_mute_preference() {
        let status = AudioStatus::new();
        status.set_muted(true);
        status.mark_unavailable("no default output device");
        status.shared.underruns.store(9, Ordering::Relaxed);

        status.reset_for_run();

        assert_eq!(
            status.state(),
            AudioState::Idle,
            "last run's excuse is gone"
        );
        assert_eq!(status.underruns(), 0);
        assert!(status.is_muted(), "mute is a preference, not run state");
    }

    // --------------------------------------------------- the real device

    /// The robustness bar, on whatever machine happens to run this: opening
    /// the real default output device must either work or record a legible
    /// reason. It must never panic, never hang, and never leave the status
    /// in a state the pane cannot render.
    ///
    /// Deliberately not `#[ignore]`d -- "a machine with no audio device is
    /// normal in CI" is precisely the claim being made, and a test that
    /// only runs when someone remembers to pass `--ignored` does not check
    /// it. The stream is dropped immediately and the ring is never fed
    /// while `running` is false, so at most this plays a few milliseconds
    /// of true silence.
    #[test]
    fn opening_the_real_device_either_works_or_degrades_with_a_reason() {
        let status = AudioStatus::new();
        let (_writer, reader) = channel(status.clone());
        let out = start_output(&status, reader, ggo_emu_core::apu::MIX_RATE);

        match (out, status.state()) {
            (Some(out), AudioState::Live { .. }) => {
                assert!(out.device_rate > 0, "a live device must report a rate");
            }
            (None, AudioState::Unavailable(reason)) => {
                assert!(!reason.is_empty(), "the degraded state must say why");
            }
            (out, state) => panic!(
                "open and state disagree: stream {:?}, state {state:?}",
                out.is_some()
            ),
        }
    }
}
