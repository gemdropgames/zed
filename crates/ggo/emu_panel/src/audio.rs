//! Native audio output for the emulator pane: stream the APU's mixed ring
//! to the default `cpal` output device, with a mute toggle and a dropout
//! counter the pane surfaces.
//!
//! # Why the pane owns an output stream at all (F5.4 R6's open question)
//!
//! F3 deferred audio and named the standalone `ggo-emu` binary as the
//! with-audio path (see [`crate::drive`]'s module doc). The reason to
//! reopen it is structural.
//!
//! `ggo-ide`'s emulator page keeps ONE persistent emu thread for the whole
//! life of the page and opens the device on it at spawn time
//! (`emu/thread.rs::spawn_with_audio`), so the device stays open across
//! every idle stretch between runs -- not something a docked panel should
//! do. This fork's [`crate::drive::start`] spawns a **per-run** thread that
//! terminates when the run ends, so opening the device inside that thread's
//! body ties the device's lifetime to the run's: [`AudioOut`] is a local in
//! [`crate::drive`]'s run loop, and its `Drop` (which stops the stream and
//! releases the device) runs on the way out of that function, on the thread
//! that opened it -- on a normal return and on an unwind alike.
//!
//! Deferring to the standalone binary instead would mean "audio works if
//! you leave the editor and run the cart in a terminal", which is not audio
//! in the panel. The APU already *runs* inside a pane run
//! (`runtime.rs::handle_vsync_wait` advances it one frame per
//! `vsync_wait`); its samples were simply being discarded.
//!
//! # Run-scoped vs panel-scoped state -- the distinction this module got
//! wrong once
//!
//! [`AudioStatus`] is **panel**-scoped: mute is a user preference that must
//! survive a run ending, and the previous run's dropout count is exactly
//! what someone wants to still be reading after the run that produced it.
//! The panel holds one for its whole life and hands clones to runs.
//!
//! Whether audio is *flowing right now* is **run**-scoped, and it lives in
//! the ring ([`channel`]'s `primed` flag), which is created inside the
//! per-run thread and dropped with it. Putting it in `AudioStatus` instead
//! was a real bug: the panel restarts a cart by stopping run A and
//! immediately starting run B on the *same* `AudioStatus`, and A's
//! teardown (which lands milliseconds later, after `Session::wait` joins
//! off-thread) then cleared a flag B had already set -- leaving B
//! permanently silent while the pane cheerfully rendered "audio on". A
//! per-run flag cannot be clobbered by another run, because it is a
//! different allocation.
//!
//! ## The tap: emu thread -> `RingWriter` -> `RingReader` -> cpal callback
//!
//! `ggo_emu_core::apu::Apu` lives inside `Peripherals`, entirely on the
//! emulator thread. The cpal realtime callback runs on a separate thread
//! cpal owns, so samples are *copied* across rather than shared.
//! [`channel`] returns a [`RingWriter`]/[`RingReader`] pair sharing a small
//! `Mutex<VecDeque<i16>>` -- the same queue design `ggo-emu/src/audio.rs`
//! uses in production, and a dedicated mutex nobody else takes, so the
//! callback never waits on anything the emulator thread holds while
//! stepping.
//!
//! **The queue mutex is locked exactly once per cpal callback invocation**
//! (in [`build_stream`]), not once per resampled pair: [`Resampler::render`]
//! drains a whole callback's worth from the already-locked queue, and
//! [`fill`] -- the pure per-pair step -- locks nothing at all.
//!
//! [`RingWriter::push`] caps the queue at [`MAX_QUEUED`] by dropping the
//! OLDEST samples, so a callback that never runs (no device, a stalled
//! host) can't grow it without bound, and a reader that resumes gets
//! bounded latency rather than an ever-growing backlog.
//!
//! There is deliberately no "reset the ring for a new run" call, which
//! `ggo-ide`'s equivalent needs on every `EmuCmd::Start` (its emu thread is
//! persistent, so a fault's last samples would play ahead of the next run's
//! first real audio). Here [`channel`] is called inside the per-run thread
//! and both halves are locals of that run: a new run cannot inherit a
//! previous run's queue because it no longer exists.
//!
//! ## What "live" means, and why it is not just "running"
//!
//! The callback only drains -- and only counts a shortfall -- while the
//! ring is **primed**: [`RingWriter::push`] has fed it at least once and
//! neither muted nor dropped it since. That is stricter than "a run is
//! going", and deliberately so, because both edges are otherwise
//! asymmetric:
//!
//! - **Run start.** The device opens before the run loop's first
//!   `vsync_wait`, so there is a window where a stream is playing from a
//!   ring nothing has written yet. Counting there reports a dropout for
//!   every cart launch.
//! - **Unmute.** The reader goes live the instant the flag flips, but the
//!   writer only refills at the next vsync (up to 16.7 ms later) and
//!   [`RingWriter::push`] cleared the queue on the way *in* to mute. Every
//!   unmute would inject ~1000 phantom dropouts.
//!
//! Priming closes both: the counter only moves once there is genuinely
//! something to have run out of.
//!
//! Mute is a **submission gate**, not a volume of zero -- [`RingWriter::push`]
//! drops the frame's samples AND clears the queue, so unmuting resumes from
//! live audio instead of replaying up to [`MAX_QUEUED`] of what was mixed
//! while silent. The reader also reads the mute flag directly, so muting is
//! instant rather than waiting for the next frame.
//!
//! ## The counter counts dropout EVENTS, not samples
//!
//! [`Resampler::render`] increments `underruns` at most **once per cpal
//! callback**, however many source pairs inside it came up empty. That unit
//! is the point: the callback is the buffer the device is about to play, so
//! one increment is one audible glitch. Counting per source pair instead --
//! which is what a naive port of `tools/ggo-ide/src/emu/audio.rs` does,
//! because its `fill` is called with a two-element slice -- reports the
//! APU's mix rate (~32020) per second of dropout and puts a five-digit
//! number in front of a user who cannot act on it. The pane renders this as
//! "dropouts" for exactly that reason.
//!
//! ## No device is a normal machine, not an error
//!
//! [`start_output`] never returns an error and never panics. A host with no
//! usable output device (CI, a session with no sound server) records the
//! reason in [`AudioStatus`] and the run proceeds silently --
//! [`AudioState::Unavailable`] is what the pane renders, carrying that
//! reason verbatim rather than a bare "no audio".
//!
//! ## Thread affinity (the Windows COM apartment rule)
//!
//! cpal's WASAPI backend touches COM to enumerate and open a device, and
//! COM apartments are thread-affine: initialising one on a thread winit has
//! already initialised differently does not fail cleanly, it can panic or
//! wedge the message pump. So [`AudioOut::start`] is only ever called from
//! the emulator thread -- a plain, uninitialised OS thread -- never from the
//! UI thread, and the resulting `cpal::Stream` never leaves it. Only the
//! plain atomics in [`AudioStatus`] cross back to the UI thread.

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
///
/// 16384 `i16` = 8192 stereo pairs ≈ **256 ms** at the APU's mix rate
/// (`ggo_emu_core::apu::MIX_RATE`, 32020 Hz).
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
    /// A device is open for the current run, or was for the last one.
    Live { muted: bool, dropouts: u32 },
}

impl AudioState {
    /// The stats-row segment, or `None` when there is nothing worth a line.
    ///
    /// `running` is the panel's own "is a session live right now", which
    /// this type deliberately does not track: whether a run is going is
    /// run-scoped state, and the one authority on it is the panel's
    /// `session` field. Without it an idle pane would keep advertising
    /// "audio on" with no device open anywhere.
    ///
    /// The count is rendered as **dropouts** rather than a bare number
    /// because the unit is the actionable part -- one dropout is one cpal
    /// buffer that came up short, i.e. one audible glitch. See this
    /// module's doc.
    pub fn label(&self, running: bool) -> Option<String> {
        match self {
            AudioState::Idle => None,
            AudioState::Unavailable(reason) => Some(format!("audio unavailable — {reason}")),
            AudioState::Live { muted, dropouts } if running => Some(format!(
                "audio {} · {dropouts} dropouts",
                if *muted { "muted" } else { "on" }
            )),
            // The run is over; the device closed with its thread. The count
            // is still worth reading, but it belongs to a past run and must
            // say so.
            AudioState::Live { dropouts, .. } => {
                Some(format!("audio idle · {dropouts} dropouts last run"))
            }
        }
    }

    /// Whether the pane can offer a working mute toggle. False for
    /// [`AudioState::Unavailable`], where the run is already silent and
    /// there is nothing for the button to change.
    pub fn is_toggleable(&self) -> bool {
        !matches!(self, AudioState::Unavailable(_))
    }
}

/// The emulator thread's report about opening a device, for this run.
///
/// One `Mutex`-guarded value rather than an `AtomicBool` beside a
/// `Mutex<Option<String>>`: two separately-published fields have an
/// interleaving where the pane reads a cleared reason against a stale
/// availability flag and renders [`AudioState::Idle`] for a frame. There is
/// no ordering to get right if there is only one thing to read, and the
/// realtime callback never touches this, so a lock costs nothing that
/// matters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Verdict {
    /// No run has tried yet.
    #[default]
    Pending,
    Open,
    Failed(String),
}

/// The state behind [`AudioStatus`]. Split out so `AudioStatus` itself is a
/// cheap `Clone` of one `Arc`.
#[derive(Default)]
struct Shared {
    /// UI-toggled, read by both ends of the ring. `Relaxed` is correct and
    /// matches the reference implementation: it gates nothing but its own
    /// value, and a one-buffer delay in observing a mute is inaudible.
    muted: AtomicBool,
    /// Dropout EVENTS -- cpal callbacks that came up short. Written by the
    /// realtime callback, read by the UI thread. See this module's doc on
    /// why the unit is the callback and not the sample.
    underruns: AtomicU32,
    /// Written once per run by the emulator thread, read by the UI thread.
    verdict: Mutex<Verdict>,
}

/// Cross-thread audio state and controls: read and toggled by the UI
/// thread, read by the cpal callback, written by the emulator thread.
///
/// **Panel-scoped** -- see this module's doc for the distinction and for
/// what went wrong when run-scoped state was kept in here.
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

    /// Dropout events counted so far this run -- see [`Shared::underruns`].
    pub fn dropouts(&self) -> u32 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// The emulator thread's report that the device opened. Also the
    /// cheapest way for a test elsewhere in the crate to reach the
    /// device-open state without a device.
    pub fn mark_available(&self) {
        *self.shared.verdict.lock().unwrap() = Verdict::Open;
    }

    /// The emulator thread's report that it could not open a device, with
    /// the reason the pane will show. Public because it is also the
    /// cheapest way for a test to reach the device-less state without a
    /// device-less machine.
    pub fn mark_unavailable(&self, reason: impl Into<String>) {
        *self.shared.verdict.lock().unwrap() = Verdict::Failed(reason.into());
    }

    /// Zero the counter and clear the previous run's verdict, so a machine
    /// that grew a sound card between runs is not still reporting the old
    /// excuse. Keeps `muted` -- see [`Shared::muted`].
    ///
    /// Note what this does NOT need to touch: whether audio is flowing.
    /// That is the ring's business and dies with the run that owned it.
    pub fn reset_for_run(&self) {
        self.shared.underruns.store(0, Ordering::Relaxed);
        *self.shared.verdict.lock().unwrap() = Verdict::Pending;
    }

    /// Count a dropout by hand. Test-only: the real increments happen in
    /// [`Resampler::render`] on the cpal callback's own thread, which no
    /// test can drive, so this is how a test elsewhere in the crate
    /// produces a non-zero count to assert the pane surfaces.
    #[cfg(test)]
    pub(crate) fn record_dropout(&self) {
        self.shared.underruns.fetch_add(1, Ordering::Relaxed);
    }

    /// What the pane renders. A failed open outranks the mute flag: a run
    /// with no device is silent for a reason the user cannot toggle away.
    pub fn state(&self) -> AudioState {
        match &*self.shared.verdict.lock().unwrap() {
            Verdict::Pending => AudioState::Idle,
            Verdict::Failed(reason) => AudioState::Unavailable(reason.clone()),
            Verdict::Open => AudioState::Live {
                muted: self.is_muted(),
                dropouts: self.dropouts(),
            },
        }
    }
}

// ------------------------------------------------------------------- ring

/// The emulator-thread half of the tap: push each presented frame's
/// freshly-mixed samples in.
///
/// Not `Clone`: its `Drop` is load-bearing (it unprimes the ring), and a
/// second handle whose drop silenced a still-running run would be a trap.
pub struct RingWriter {
    queue: Arc<Mutex<VecDeque<i16>>>,
    /// Run-scoped. See [`channel`].
    primed: Arc<AtomicBool>,
    status: AudioStatus,
}

impl RingWriter {
    /// Append `samples` (interleaved stereo), trimming back under
    /// [`MAX_QUEUED`] from the front on overflow. The trim takes an EVEN
    /// count so L/R phase is never split across the cut.
    ///
    /// A successful push is what **primes** the ring; muting unprimes it
    /// and drops what was queued. See this module's doc.
    pub fn push(&self, samples: &[i16]) {
        if self.status.is_muted() {
            self.primed.store(false, Ordering::Relaxed);
            self.queue.lock().unwrap().clear();
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
        drop(queue);
        self.primed.store(true, Ordering::Relaxed);
    }
}

impl RingWriter {
    /// The run is parked (debugger pause): nothing will feed the ring
    /// until it resumes, so unprime it -- the callback outputs silence and
    /// counts no dropouts against a ring nobody is writing -- and drop
    /// what was queued so resuming doesn't replay a stale burst. The next
    /// [`Self::push`] re-primes it.
    pub fn idle(&self) {
        self.primed.store(false, Ordering::Relaxed);
        self.queue.lock().unwrap().clear();
    }
}

impl Drop for RingWriter {
    /// The run is over -- nothing will feed this ring again, so the
    /// callback must stop draining and stop counting immediately.
    ///
    /// Doing this in `Drop` rather than as an explicit call on the way out
    /// of the run loop is what makes it hold for a **panicking** emulator
    /// thread too: an unwind drops locals, and a run that died mid-frame
    /// must not leave the pane counting dropouts against a ring nobody is
    /// writing.
    fn drop(&mut self) {
        self.primed.store(false, Ordering::Relaxed);
    }
}

/// The cpal-callback half of the tap.
pub struct RingReader {
    queue: Arc<Mutex<VecDeque<i16>>>,
    primed: Arc<AtomicBool>,
    status: AudioStatus,
}

impl RingReader {
    /// Whether the callback should drain and count right now: the ring has
    /// been fed and not muted since. See this module's doc.
    pub fn is_live(&self) -> bool {
        self.primed.load(Ordering::Relaxed) && !self.status.is_muted()
    }

    /// How many samples are currently queued. Only the tests read this --
    /// it is the observable effect of [`RingWriter::push`]'s mute gate and
    /// cap trim (and, from [`crate::drive`]'s tests, of the per-frame APU
    /// tap) without exposing the queue itself as real API.
    #[cfg(test)]
    pub(crate) fn queued_len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

/// Build a fresh, empty tap sharing `status`.
///
/// The `primed` flag it creates is **run-scoped**: one allocation per
/// `channel()` call, i.e. per run, so one run ending can never silence
/// another that started from the same [`AudioStatus`]. That is the whole
/// fix for the restart bug this module's doc describes -- do not be tempted
/// to hoist it into `AudioStatus` to save an `Arc`.
///
/// Cheap, and it touches no audio hardware, so it is safe to call
/// unconditionally whether or not an [`AudioOut`] ever starts.
pub fn channel(status: AudioStatus) -> (RingWriter, RingReader) {
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let primed = Arc::new(AtomicBool::new(false));
    (
        RingWriter {
            queue: Arc::clone(&queue),
            primed: Arc::clone(&primed),
            status: status.clone(),
        },
        RingReader {
            queue,
            primed,
            status,
        },
    )
}

/// Pure drain of ONE stereo pair from an already-locked queue. Returns
/// whether it had to invent silence.
///
/// - `live == false` (not primed, or muted): emits silence, drains nothing,
///   and reports no shortfall -- no matter what is still sitting in the
///   queue. Without this, a muted or just-ended run would count against a
///   queue nothing is feeding, and stale samples would play back as if the
///   run were still producing them.
/// - `live == true`: pops what it can, pads the rest with silence, and
///   reports `true` if it came up short.
///
/// Deliberately does NOT touch the counter: at two elements a call, an
/// increment here is an increment per *source pair*, which is the APU's mix
/// rate per second of dropout. The counting belongs one level up, in
/// [`Resampler::render`], where the unit is a whole device buffer.
///
/// No locking here on purpose -- `queue` is a plain `&mut VecDeque` -- which
/// is what makes the drain decision unit-testable with no device, no
/// thread, and no mutex.
fn fill(out: &mut [i16], queue: &mut VecDeque<i16>, live: bool) -> bool {
    if !live {
        out.fill(0);
        return false;
    }
    let mut short = false;
    for slot in out.iter_mut() {
        *slot = match queue.pop_front() {
            Some(sample) => sample,
            None => {
                short = true;
                0
            }
        };
    }
    short
}

/// Mix-rate -> device-rate resampling state, and the home of the dropout
/// accounting.
///
/// The resampler itself is an integer phase accumulator (repeat when the
/// device runs faster than the mix, skip when slower) -- `ggo-emu/src/
/// audio.rs::build_stream`'s math verbatim. What is NOT inherited from
/// there is where the counter is incremented; see [`Self::render`].
struct Resampler {
    acc: u32,
    cur: [i16; STEREO_PAIR],
    device_rate: u32,
    mix_rate: u32,
}

impl Resampler {
    fn new(device_rate: u32, mix_rate: u32) -> Self {
        Resampler {
            acc: 0,
            cur: [0i16; STEREO_PAIR],
            device_rate,
            mix_rate,
        }
    }

    /// Render one cpal callback: `out_frames` output frames, handed to
    /// `emit` a resampled stereo pair at a time.
    ///
    /// **Increments `underruns` at most once**, however many of the source
    /// pairs consumed along the way came up empty -- one increment is one
    /// device buffer that had a hole in it, which is one audible glitch.
    /// This is the whole reason the drain is split into [`fill`] (per pair,
    /// no counting) and this method (per callback, counts once).
    ///
    /// Pure apart from that single atomic: no locking, no allocation, and
    /// `queue` arrives already locked by the caller, which is what lets a
    /// test drive a realistic 480-frame callback with no device present.
    fn render(
        &mut self,
        out_frames: usize,
        queue: &mut VecDeque<i16>,
        live: bool,
        underruns: &AtomicU32,
        mut emit: impl FnMut([i16; STEREO_PAIR]),
    ) {
        let mut short = false;
        for _ in 0..out_frames {
            // Consume mix_rate/device_rate input pairs per output frame.
            self.acc += self.mix_rate;
            while self.acc >= self.device_rate {
                self.acc -= self.device_rate;
                short |= fill(&mut self.cur, queue, live);
            }
            emit(self.cur);
        }
        if short {
            underruns.fetch_add(1, Ordering::Relaxed);
        }
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
/// session with no sound server), and a run must never fail or hang because
/// of it. `None` means the run is silent and [`AudioStatus::state`] now
/// reports [`AudioState::Unavailable`] with the reason; the ring still
/// exists and [`RingWriter::push`] still bounds itself, so nothing else in
/// the drive loop needs a second code path.
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

/// Build the typed output stream. The ring's mutex and the live flag are
/// read exactly ONCE per callback, however many source pairs
/// [`Resampler::render`] consumes; the callback allocates nothing.
fn build_stream<T: SizedSample + FromSample<i16>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    device_rate: u32,
    mix_rate: u32,
    ring: RingReader,
) -> Option<cpal::Stream> {
    let mut resampler = Resampler::new(device_rate, mix_rate);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let out_frames = data.len() / channels;
                let mut queue = ring.queue.lock().unwrap();
                let live = ring.is_live();
                let mut frames = data.chunks_mut(channels);
                resampler.render(
                    out_frames,
                    &mut queue,
                    live,
                    &ring.status.shared.underruns,
                    |pair| {
                        let Some(frame) = frames.next() else {
                            return;
                        };
                        frame[0] = T::from_sample(pair[0]);
                        if channels >= STEREO_PAIR {
                            frame[1] = T::from_sample(pair[1]);
                        }
                        for extra in frame.iter_mut().skip(STEREO_PAIR) {
                            *extra = T::from_sample(0i16);
                        }
                    },
                );
            },
            |err| log::warn!("ggo emu panel: audio stream error: {err}"),
            None,
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typical cpal buffer: 480 frames is 10 ms at 48 kHz. Used by the
    /// callback-level tests so the numbers they assert are the numbers
    /// production actually produces.
    const CALLBACK_FRAMES: usize = 480;
    const DEVICE_RATE: u32 = 48_000;
    const MIX_RATE: u32 = ggo_emu_core::apu::MIX_RATE;

    /// A ring that has already been fed, i.e. primed and live -- the
    /// steady state most of these tests care about.
    fn primed_channel() -> (RingWriter, RingReader, AudioStatus) {
        let status = AudioStatus::new();
        let (writer, reader) = channel(status.clone());
        writer.push(&[0, 0]);
        assert!(reader.is_live(), "sanity: a fed ring is live");
        (writer, reader, status)
    }

    /// Drive one callback's worth of drain and report how many dropout
    /// events it counted, exactly as `build_stream`'s closure would.
    fn one_callback(reader: &RingReader, resampler: &mut Resampler, status: &AudioStatus) -> u32 {
        let before = status.dropouts();
        let mut queue = reader.queue.lock().unwrap();
        let live = reader.is_live();
        let mut emitted = 0usize;
        resampler.render(
            CALLBACK_FRAMES,
            &mut queue,
            live,
            &status.shared.underruns,
            |_| emitted += 1,
        );
        assert_eq!(
            emitted, CALLBACK_FRAMES,
            "every output frame must be written"
        );
        status.dropouts() - before
    }

    // ------------------------------------------------- fill (per pair)

    #[test]
    fn fill_drains_available_samples_and_reports_no_shortfall() {
        let mut queue: VecDeque<i16> = VecDeque::from(vec![1, 2, 3, 4, 5, 6]);

        let mut out = [0i16; 4];
        assert!(!fill(&mut out, &mut queue, true));
        assert_eq!(
            out,
            [1, 2, 3, 4],
            "live and behind the writer: oldest first"
        );

        let mut out = [0i16; 2];
        assert!(!fill(&mut out, &mut queue, true));
        assert_eq!(out, [5, 6], "the remainder is still queued for next time");
    }

    #[test]
    fn fill_emits_silence_and_reports_a_shortfall_when_live_and_caught_up() {
        let mut queue: VecDeque<i16> = VecDeque::new();
        let mut out = [7i16; 4]; // non-zero sentinel, so silence is provable
        assert!(fill(&mut out, &mut queue, true));
        assert_eq!(out, [0, 0, 0, 0]);
    }

    #[test]
    fn fill_pads_a_partial_drain_with_silence_and_still_reports_a_shortfall() {
        let mut queue: VecDeque<i16> = VecDeque::from(vec![9, 9]); // one pair
        let mut out = [1i16; 6]; // asks for three
        assert!(fill(&mut out, &mut queue, true));
        assert_eq!(out, [9, 9, 0, 0, 0, 0], "what was there, then silence");
    }

    #[test]
    fn fill_emits_silence_and_never_reports_or_drains_while_not_live() {
        let mut queue: VecDeque<i16> = VecDeque::from(vec![9, 9, 9, 9]);
        let mut out = [7i16; 4];
        assert!(!fill(&mut out, &mut queue, false));
        assert_eq!(out, [0, 0, 0, 0], "silence regardless of the leftovers");
        assert_eq!(queue.len(), 4, "the queue is left untouched, not drained");
    }

    // --------------------------------------- render (per callback) unit

    /// **The counter's unit.** One callback that came up short is ONE
    /// dropout, not one per source pair -- at 480 frames of 48 kHz against
    /// a 32020 Hz mix that is 320 empty pairs, and counting those would put
    /// a five-digit number per second in front of the user.
    #[test]
    fn one_short_callback_counts_exactly_one_dropout_however_many_pairs_were_empty() {
        let status = AudioStatus::new();
        let mut queue: VecDeque<i16> = VecDeque::new();
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);

        let mut emitted = 0usize;
        resampler.render(
            CALLBACK_FRAMES,
            &mut queue,
            true,
            &status.shared.underruns,
            |_| emitted += 1,
        );

        assert_eq!(emitted, CALLBACK_FRAMES);
        assert_eq!(
            status.dropouts(),
            1,
            "one buffer with holes in it is one dropout event"
        );
    }

    /// Ten empty callbacks are ten dropouts -- the counter still tracks
    /// severity, it just does so in a unit a human can act on.
    #[test]
    fn each_short_callback_counts_once_so_the_rate_stays_readable() {
        let status = AudioStatus::new();
        let mut queue: VecDeque<i16> = VecDeque::new();
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);

        for _ in 0..10 {
            resampler.render(
                CALLBACK_FRAMES,
                &mut queue,
                true,
                &status.shared.underruns,
                |_| {},
            );
        }
        assert_eq!(status.dropouts(), 10);
    }

    #[test]
    fn a_callback_the_ring_can_satisfy_counts_nothing() {
        let status = AudioStatus::new();
        // Comfortably more than the ~320 pairs a 480-frame callback needs.
        let mut queue: VecDeque<i16> = VecDeque::from(vec![5i16; 4_000]);
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);

        let mut heard_signal = false;
        resampler.render(
            CALLBACK_FRAMES,
            &mut queue,
            true,
            &status.shared.underruns,
            |pair| heard_signal |= pair != [0, 0],
        );
        assert_eq!(status.dropouts(), 0);
        assert!(heard_signal, "a fed ring must produce actual samples");
    }

    /// The steady state that would otherwise spin the counter at
    /// device-callback rate: nothing queued and nothing live.
    #[test]
    fn a_not_live_callback_never_counts_however_many_times_it_runs() {
        let status = AudioStatus::new();
        let mut queue: VecDeque<i16> = VecDeque::new();
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);

        for _ in 0..1_000 {
            let mut all_silent = true;
            resampler.render(
                CALLBACK_FRAMES,
                &mut queue,
                false,
                &status.shared.underruns,
                |pair| all_silent &= pair == [0, 0],
            );
            assert!(all_silent);
        }
        assert_eq!(
            status.dropouts(),
            0,
            "1000 not-live callbacks, zero counted"
        );
    }

    // ------------------------------------------------------- RingWriter

    #[test]
    fn push_is_a_no_op_on_an_empty_slice() {
        let status = AudioStatus::new();
        let (writer, reader) = channel(status);
        writer.push(&[]);
        assert_eq!(reader.queued_len(), 0);
        assert!(!reader.is_live(), "an empty push does not prime the ring");
    }

    #[test]
    fn push_drops_the_oldest_samples_past_the_cap() {
        let status = AudioStatus::new();
        let (writer, reader) = channel(status);
        // Values count up, so the survivors are identifiable.
        let overflow: Vec<i16> = (0..(MAX_QUEUED as i32 + 10)).map(|v| v as i16).collect();
        writer.push(&overflow);

        assert!(
            reader.queued_len() <= MAX_QUEUED,
            "the queue must stay bounded, got {}",
            reader.queued_len()
        );
        let mut first = [0i16; 2];
        let mut queue = reader.queue.lock().unwrap();
        fill(&mut first, &mut queue, true);
        assert!(
            first[0] as i32 > 0,
            "the oldest (lowest-valued) samples were the ones trimmed, got {first:?}"
        );
    }

    // --------------------------------------------------- priming / runs

    /// **Run start.** The device opens before the run loop's first frame,
    /// so a stream can be playing from a ring nothing has written yet.
    /// Counting there would report a dropout on every single cart launch.
    #[test]
    fn a_ring_that_has_never_been_fed_is_not_live_and_counts_nothing() {
        let status = AudioStatus::new();
        let (_writer, reader) = channel(status.clone());
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);

        for _ in 0..20 {
            one_callback(&reader, &mut resampler, &status);
        }
        assert_eq!(
            status.dropouts(),
            0,
            "a cold start is not a dropout -- the device opens before the run \
             loop's first frame, so this would fire on every cart launch"
        );
        assert!(!reader.is_live(), "a ring nobody has fed is not live");
    }

    /// **BLOCKER 1's regression test.** The panel restarts a cart by
    /// stopping run A and immediately starting run B on the SAME
    /// `AudioStatus`; A's teardown lands milliseconds later, after B is
    /// already going. If "is audio flowing" lived on the shared status, A's
    /// drop would silence B for the rest of its life while the pane
    /// rendered "audio on". It is run-scoped for exactly this reason.
    #[test]
    fn ending_one_run_does_not_silence_another_started_from_the_same_status() {
        let status = AudioStatus::new();

        // Run A, under way.
        let (writer_a, reader_a) = channel(status.clone());
        writer_a.push(&[1, 2]);
        assert!(reader_a.is_live());

        // Run B starts on the same panel-scoped status, before A's thread
        // has finished unwinding.
        let (writer_b, reader_b) = channel(status.clone());
        writer_b.push(&[3, 4]);
        assert!(reader_b.is_live());

        // A's emulator thread finally returns and drops its writer.
        drop(writer_a);

        assert!(!reader_a.is_live(), "the ended run goes quiet");
        assert!(
            reader_b.is_live(),
            "run A ending must not silence run B -- this is the restart bug"
        );

        // And B keeps counting honestly rather than being frozen silent.
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);
        assert_eq!(
            one_callback(&reader_b, &mut resampler, &status),
            1,
            "B is live, its ring is nearly empty, so a real dropout is counted"
        );
    }

    /// **Finding 7.** A panicking emulator thread never reaches the end of
    /// the run loop, so the unprime has to ride on `Drop` rather than an
    /// explicit call -- otherwise a dead run counts dropouts forever.
    #[test]
    fn dropping_the_writer_unprimes_the_ring_even_without_a_clean_shutdown() {
        let (writer, reader, status) = primed_channel();
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);

        drop(writer); // an unwind drops locals exactly like this
        assert!(!reader.is_live());
        for _ in 0..50 {
            assert_eq!(one_callback(&reader, &mut resampler, &status), 0);
        }
        assert_eq!(status.dropouts(), 0, "a dead run must not keep counting");
    }

    // ------------------------------------------------------------- mute

    /// Mute is a submission gate: nothing new is queued, and what WAS
    /// queued is dropped so unmuting resumes from live audio.
    #[test]
    fn mute_stops_submitting_frames_and_clears_what_was_already_queued() {
        let (writer, reader, status) = primed_channel();
        writer.push(&[1, 2, 3, 4]);
        assert_eq!(reader.queued_len(), 6, "the priming pair plus these four");

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

    /// **BLOCKER 3's regression test.** The reader goes live the instant
    /// the mute flag flips, but the writer only refills at the next vsync
    /// (up to 16.7 ms later) and `push` cleared the queue on the way IN to
    /// mute. Without priming, every unmute injects a burst of phantom
    /// dropouts from a ring that simply has not been refilled yet.
    #[test]
    fn unmuting_counts_nothing_until_the_ring_has_actually_been_refilled() {
        let (writer, reader, status) = primed_channel();
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);

        // Mute, and let the emulator thread notice at its next frame.
        status.set_muted(true);
        writer.push(&[1, 2, 3, 4]);
        for _ in 0..50 {
            assert_eq!(one_callback(&reader, &mut resampler, &status), 0);
        }

        // Unmute. The writer has NOT run yet -- this is the window.
        status.set_muted(false);
        for _ in 0..50 {
            one_callback(&reader, &mut resampler, &status);
        }
        assert_eq!(
            status.dropouts(),
            0,
            "the refill window between unmuting and the next vsync must not be \
             charged as dropouts"
        );
        assert!(
            !reader.is_live(),
            "unmuted but not yet refilled is not live"
        );

        // The next frame refills it, and normal service (and normal
        // accounting) resumes.
        writer.push(&vec![7i16; 4_000]);
        assert!(reader.is_live());
        assert_eq!(one_callback(&reader, &mut resampler, &status), 0);
    }

    /// The counter must not become meaningless while muted -- a muted run's
    /// empty queue is not a dropout, it is the point.
    #[test]
    fn a_muted_run_is_not_live_so_the_counter_never_moves() {
        let (writer, reader, status) = primed_channel();
        let mut resampler = Resampler::new(DEVICE_RATE, MIX_RATE);
        status.set_muted(true);
        writer.push(&[1, 2, 3, 4]);

        for _ in 0..200 {
            assert_eq!(one_callback(&reader, &mut resampler, &status), 0);
        }
        assert_eq!(status.dropouts(), 0);
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
    fn muting_is_instant_rather_than_waiting_for_the_next_frame() {
        let (_writer, reader, status) = primed_channel();
        assert!(reader.is_live());
        status.set_muted(true);
        assert!(
            !reader.is_live(),
            "the reader reads the mute flag directly, so silence is immediate"
        );
    }

    // ------------------------------------------------------ AudioState

    #[test]
    fn state_is_idle_before_a_run_opens_a_device() {
        let status = AudioStatus::new();
        assert_eq!(status.state(), AudioState::Idle);
        assert_eq!(status.state().label(true), None, "idle says nothing");
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
            status.state().label(true).as_deref(),
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
    fn an_open_device_reports_mute_and_the_dropout_count() {
        let status = AudioStatus::new();
        status.mark_available();
        assert_eq!(
            status.state(),
            AudioState::Live {
                muted: false,
                dropouts: 0
            }
        );
        assert_eq!(
            status.state().label(true).as_deref(),
            Some("audio on · 0 dropouts")
        );
        assert!(status.state().is_toggleable());

        status.set_muted(true);
        status.record_dropout();
        status.record_dropout();
        status.record_dropout();
        assert_eq!(
            status.state().label(true).as_deref(),
            Some("audio muted · 3 dropouts")
        );
    }

    /// **Finding 6.** A run that has ended closed its device with its
    /// thread, so an idle pane must not keep claiming "audio on". The count
    /// is still worth reading; it just has to be labelled as the last
    /// run's.
    #[test]
    fn an_idle_pane_reports_the_last_runs_count_rather_than_a_live_device() {
        let status = AudioStatus::new();
        status.mark_available();
        status.record_dropout();

        assert_eq!(
            status.state().label(true).as_deref(),
            Some("audio on · 1 dropouts"),
            "while the run is going"
        );
        assert_eq!(
            status.state().label(false).as_deref(),
            Some("audio idle · 1 dropouts last run"),
            "and once it is not"
        );
    }

    #[test]
    fn reset_for_run_clears_the_counter_and_the_verdict_but_keeps_the_mute_preference() {
        let status = AudioStatus::new();
        status.set_muted(true);
        status.mark_unavailable("no default output device");
        status.record_dropout();

        status.reset_for_run();

        assert_eq!(
            status.state(),
            AudioState::Idle,
            "last run's excuse is gone"
        );
        assert_eq!(status.dropouts(), 0);
        assert!(status.is_muted(), "mute is a preference, not run state");
    }

    // --------------------------------------------------- the real device

    /// Smoke check, not an invariant: opening the real default output
    /// device must not panic, and must leave the status in one of the two
    /// states the pane knows how to render.
    ///
    /// It deliberately does not assert which one -- that depends on the
    /// machine -- and the arms here restate [`start_output`]'s own match, so
    /// this proves no *logic*. What it does prove is that the whole
    /// device-touching path (host enumeration, config negotiation, stream
    /// build, `play`, and `Drop`) runs to completion on this machine
    /// without unwinding, which is the only part of the audio path that
    /// cannot be exercised without hardware. Forcing the degraded branch
    /// (e.g. `ALSA_CONFIG_PATH` pointing at an empty file) exercises the
    /// other side in ~0 s.
    ///
    /// Not `#[ignore]`d: "a machine with no audio device is normal in CI"
    /// is a claim this task makes, and a test that only runs under
    /// `--ignored` does not check it. Nothing is ever fed to the ring and
    /// it is never primed, so this plays true silence.
    ///
    /// Caveat, untested: nothing here bounds how long `snd_pcm_open` may
    /// take, so a wedged sound server would hang this test rather than
    /// failing it. Not reproducible on demand, so it stays a known gap
    /// rather than a speculative timeout.
    #[test]
    fn opening_the_real_device_does_not_panic_and_leaves_a_renderable_state() {
        let status = AudioStatus::new();
        let (_writer, reader) = channel(status.clone());
        let out = start_output(&status, reader, MIX_RATE);

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
