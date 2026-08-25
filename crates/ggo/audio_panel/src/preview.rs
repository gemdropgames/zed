//! Playback of one file on its own OS thread, through the emulator pane's
//! cpal ring -- so a preview shares the emulator's device handling (no
//! device is `Unavailable(reason)`, mute is a submission gate, dropouts are
//! counted) instead of growing a second one.
//!
//! Two sources, one sink:
//!
//! - **Source**: the decoded PCM pushed in as-is, one 60 Hz frame's worth
//!   per turn, played at the file's own rate.
//! - **Baked**: the `.adp` blocks uploaded into a standalone
//!   [`ggo_emu_core::apu::Apu`] and played on sample channel 0 exactly as
//!   a cart would (`queue_samples` + `play_sample`), mixed a frame at a
//!   time and tapped with `copy_since` -- the same loop `drive.rs` runs,
//!   minus the CPU. What comes out is what the hardware will produce:
//!   4-bit ADPCM, the 4.12 phase step, the 32 kHz mix.
//!
//! The loop is paced to 60 Hz like the emulator's, and the ring's cap
//! (~256 ms) is what bounds latency when Stop is pressed. A one-shot keeps
//! the thread alive for [`DRAIN_FRAMES`] after the last push so the tail
//! in the ring is heard before the writer's drop silences it.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ggo_audio::Decoded;
use ggo_emu_core::apu::Apu;
use ggo_emu_panel::audio::{self, AudioStatus, RingWriter};

/// What to play.
pub enum Spec {
    Source(Arc<Decoded>),
    /// A whole `.adp` (header + blocks).
    Baked(Arc<Vec<u8>>),
}

/// Progress is published as a fraction scaled by this.
const PROGRESS_SCALE: u32 = 10_000;
const FRAME_TIME: Duration = Duration::from_micros(16_667);
/// Frames to keep the thread alive after a one-shot's last sample, so the
/// ring (≈256 ms) drains before the writer's drop silences it.
const DRAIN_FRAMES: u64 = 20;
/// The nominal mix rate the SDK's `step_for_rate_hz` divides by -- the
/// same arithmetic emerald uses for a cart, so the preview's pitch is the
/// cart's pitch.
const MIX_RATE_NOMINAL: u64 = 32_000;
/// Contract §4: `loop_off == 0xFFFF_FFFF` plays once.
const LOOP_NONE: u32 = ggo_emu_core::apu::ONE_SHOT;
const SAMPLES_PER_BLOCK: u64 = 120;
const BLOCK_BYTES: usize = 64;

/// A running preview. Dropping it stops the thread.
pub struct Preview {
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    done: Arc<AtomicBool>,
}

impl Preview {
    pub fn start(spec: Spec, looping: bool, status: AudioStatus) -> Preview {
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let spawned = std::thread::Builder::new()
            .name("ggo-audio-preview".into())
            .spawn({
                let (stop, progress, done) = (stop.clone(), progress.clone(), done.clone());
                move || {
                    let (mut writer, reader) = audio::channel(status.clone());
                    let mix_rate = match &spec {
                        Spec::Source(decoded) => decoded.rate_hz,
                        Spec::Baked(_) => ggo_emu_core::apu::MIX_RATE,
                    };
                    // Held for the thread's life: dropping it closes the
                    // device. `None` (no device) plays silently, which is
                    // the emulator pane's rule too.
                    let _out = audio::start_output(&status, reader, mix_rate);
                    run(&spec, looping, &mut writer, &stop, &progress, true);
                    done.store(true, Ordering::Release);
                }
            });
        if let Err(e) = spawned {
            log::error!("ggo-audio-preview thread: {e}");
            done.store(true, Ordering::Release);
        }
        Preview {
            stop,
            progress,
            done,
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// 0.0 ..= 1.0 through the clip (wrapping when looping).
    pub fn progress(&self) -> f32 {
        self.progress.load(Ordering::Relaxed) as f32 / PROGRESS_SCALE as f32
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Where frames go. The ring in production; a `Vec` in tests, which is
/// what keeps the whole loop testable with no device and no thread.
pub(crate) trait Sink {
    fn push(&mut self, samples: &[i16]);
}

impl Sink for RingWriter {
    fn push(&mut self, samples: &[i16]) {
        RingWriter::push(self, samples)
    }
}

impl Sink for Vec<i16> {
    fn push(&mut self, samples: &[i16]) {
        self.extend_from_slice(samples)
    }
}

/// The whole playback, synchronous. `pace` false runs flat out (tests).
pub(crate) fn run(
    spec: &Spec,
    looping: bool,
    sink: &mut dyn Sink,
    stop: &AtomicBool,
    progress: &AtomicU32,
    pace: bool,
) {
    match spec {
        Spec::Source(decoded) => run_source(decoded, looping, sink, stop, progress, pace),
        Spec::Baked(blob) => run_baked(blob, looping, sink, stop, progress, pace),
    }
}

struct Pacer {
    last: Instant,
    on: bool,
}

impl Pacer {
    fn hold(&mut self) {
        if self.on {
            if let Some(hold) = FRAME_TIME.checked_sub(self.last.elapsed()) {
                std::thread::sleep(hold);
            }
            self.last = Instant::now();
        }
    }
}

fn run_source(
    decoded: &Decoded,
    looping: bool,
    sink: &mut dyn Sink,
    stop: &AtomicBool,
    progress: &AtomicU32,
    pace: bool,
) {
    let samples = &decoded.samples;
    if samples.is_empty() {
        return;
    }
    let per_frame = (decoded.rate_hz / 60).max(1) as usize;
    let mut frame = Vec::with_capacity(per_frame * 2);
    let mut pacer = Pacer {
        last: Instant::now(),
        on: pace,
    };
    let mut pos = 0usize;
    let mut drained = 0u64;
    while !stop.load(Ordering::Acquire) {
        if pos >= samples.len() {
            if looping {
                pos = 0;
            } else {
                drained += 1;
                if drained > DRAIN_FRAMES {
                    break;
                }
                pacer.hold();
                continue;
            }
        }
        let end = (pos + per_frame).min(samples.len());
        frame.clear();
        for &s in &samples[pos..end] {
            frame.push(s);
            frame.push(s);
        }
        sink.push(&frame);
        pos = end;
        progress.store(
            (pos as u64 * PROGRESS_SCALE as u64 / samples.len() as u64) as u32,
            Ordering::Relaxed,
        );
        pacer.hold();
    }
}

fn run_baked(
    blob: &[u8],
    looping: bool,
    sink: &mut dyn Sink,
    stop: &AtomicBool,
    progress: &AtomicU32,
    pace: bool,
) {
    let Some((header, blocks)) = ggo_asset_formats::parse_adp(blob) else {
        return;
    };
    let len = header.block_count as usize * BLOCK_BYTES;
    if len == 0 || header.rate_hz == 0 {
        return;
    }
    let mut apu = Apu::new();
    // `queue_samples` clamps to the region and reports what landed; a clip
    // longer than 384 KiB previews as the truncated upload a cart would
    // get, which is the honest preview of it.
    let uploaded = apu.queue_samples(0, &blocks[..len]);
    if uploaded <= 0 {
        return;
    }
    let step = ((header.rate_hz as u64) << 12) / MIX_RATE_NOMINAL;
    let step = step.min(0xFFFF) as u32;
    let step_vol = step | (0xFF << 16) | (0xFF << 24);
    let loop_off = if looping { 0 } else { LOOP_NONE };
    if apu.play_sample(0, 0, uploaded as u32, loop_off, step_vol, 0) < 0 {
        return;
    }
    let total_samples = uploaded as u64 / BLOCK_BYTES as u64 * SAMPLES_PER_BLOCK;
    let frames_total = (total_samples * 60).div_ceil(header.rate_hz as u64).max(1);
    let mut pacer = Pacer {
        last: Instant::now(),
        on: pace,
    };
    let mut cursor = 0u64;
    let mut scratch = Vec::new();
    let mut frames = 0u64;
    while !stop.load(Ordering::Acquire) {
        apu.run_frame();
        scratch.clear();
        cursor = apu.copy_since(cursor, &mut scratch);
        sink.push(&scratch);
        frames += 1;
        let played = if looping {
            frames % frames_total
        } else {
            frames.min(frames_total)
        };
        progress.store(
            (played * PROGRESS_SCALE as u64 / frames_total) as u32,
            Ordering::Relaxed,
        );
        if !looping && frames >= frames_total + DRAIN_FRAMES {
            break;
        }
        pacer.hold();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize, rate: u32) -> Decoded {
        // 8000-amplitude integer triangle at rate/40 Hz.
        let period = 40usize;
        let samples = (0..len)
            .map(|i| {
                let p = i % period;
                let half = period / 2;
                let v = if p < half {
                    (p as i32 * 16_000 / half as i32) - 8000
                } else {
                    8000 - ((p - half) as i32 * 16_000 / half as i32)
                };
                v as i16
            })
            .collect();
        Decoded {
            samples,
            rate_hz: rate,
            source_channels: 1,
        }
    }

    /// A sink that trips the stop flag after `limit` pushes -- the test's
    /// stand-in for the Stop button.
    struct StopAfter {
        out: Vec<i16>,
        pushes: usize,
        limit: usize,
        stop: Arc<AtomicBool>,
    }

    impl Sink for StopAfter {
        fn push(&mut self, samples: &[i16]) {
            self.out.extend_from_slice(samples);
            self.pushes += 1;
            if self.pushes >= self.limit {
                self.stop.store(true, Ordering::Release);
            }
        }
    }

    fn peak(samples: &[i16]) -> u16 {
        samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0)
    }

    #[test]
    fn a_baked_one_shot_is_audible_and_ends_on_its_own() {
        let decoded = tone(16_000, 16_000);
        let blob = Arc::new(ggo_audio::bake(&decoded, 16_000));
        let mut out = Vec::new();
        let stop = AtomicBool::new(false);
        let progress = AtomicU32::new(0);
        run(&Spec::Baked(blob), false, &mut out, &stop, &progress, false);
        // One second at the 32 kHz mix = ~64k interleaved samples, plus
        // the drain frames of silence.
        assert!(out.len() > 60_000, "{} samples", out.len());
        assert!(peak(&out) > 2000, "peak {}", peak(&out));
        assert_eq!(progress.load(Ordering::Relaxed), PROGRESS_SCALE);
        // The drain tail is silence: the sample really did stop.
        let tail = &out[out.len() - 2000..];
        assert_eq!(peak(tail), 0, "tail must be silent after the one-shot ends");
    }

    #[test]
    fn a_baked_loop_keeps_playing_until_stopped() {
        let decoded = tone(1_600, 16_000); // 0.1 s
        let blob = Arc::new(ggo_audio::bake(&decoded, 16_000));
        let stop = Arc::new(AtomicBool::new(false));
        let mut sink = StopAfter {
            out: Vec::new(),
            pushes: 0,
            limit: 120, // two seconds of frames: 20 passes of the clip
            stop: stop.clone(),
        };
        let progress = AtomicU32::new(0);
        run(&Spec::Baked(blob), true, &mut sink, &stop, &progress, false);
        assert_eq!(sink.pushes, 120, "the loop ran until Stop, not until the clip ended");
        // Still audible at the end: the loop point kept feeding samples.
        let tail = &sink.out[sink.out.len() - 4000..];
        assert!(peak(tail) > 2000, "loop went silent, peak {}", peak(tail));
    }

    #[test]
    fn a_source_preview_duplicates_mono_into_stereo_and_reports_progress() {
        let decoded = Arc::new(tone(600, 6_000)); // 0.1 s, 100 samples/frame
        let mut out = Vec::new();
        let stop = AtomicBool::new(false);
        let progress = AtomicU32::new(0);
        run(&Spec::Source(decoded.clone()), false, &mut out, &stop, &progress, false);
        assert_eq!(out.len(), 1200, "every mono sample becomes an L/R pair");
        assert_eq!(&out[0..2], &[decoded.samples[0], decoded.samples[0]]);
        assert_eq!(progress.load(Ordering::Relaxed), PROGRESS_SCALE);
    }

    #[test]
    fn junk_and_empty_inputs_return_without_pushing() {
        let mut out = Vec::new();
        let stop = AtomicBool::new(false);
        let progress = AtomicU32::new(0);
        run(&Spec::Baked(Arc::new(b"nope".to_vec())), false, &mut out, &stop, &progress, false);
        let empty = Arc::new(Decoded {
            samples: vec![],
            rate_hz: 16_000,
            source_channels: 1,
        });
        run(&Spec::Source(empty), true, &mut out, &stop, &progress, false);
        assert!(out.is_empty());
    }
}
