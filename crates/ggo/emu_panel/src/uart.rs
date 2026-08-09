//! The run's diagnostic/UART line log -- what the pane's console shows and
//! what [`crate::ingest`] writes into `ggo_ide.db`'s `uart` table.
//!
//! Ported from `ggo-ide`'s `emu/thread.rs::UartAccumulator`/`UartHandle`:
//! same byte-oriented `push`, same line splitting, same trailing-`\r` trim,
//! same lossy UTF-8 decode, same [`UART_LOG_CAP`] rolling window, same
//! non-destructive [`UartLog::peek_tail`] for a live view. The one API
//! difference is [`UartLog::lines`] where ggo-ide has `take`: see that
//! method's doc.
//!
//! # What actually reaches this log in cart mode
//!
//! **`ggo-emu-core` exposes no host-visible guest-UART channel for a
//! `.cart` XIP run.** The `log(ptr, len)` syscall is handled in
//! `ggo-emu-core/src/runtime.rs` (the `Some(Syscall::Log)` arm) by a bare
//! `println!("cart-log: {..}")` straight to the host process's stdout --
//! there is no sink, no buffer and no hook on `Peripherals` to redirect it
//! to. Only the full-system boot path has a real UART
//! (`FullSystemBus::take_uart`), and that path is not what this pane
//! drives.
//!
//! ggo-ide's cart runner has exactly the same hole and documents it the
//! same way (`tools/ggo-ide/src/emu/mod.rs`, `CartStepper::drain_uart`:
//! "A running cart has no host-visible UART channel", returning only its
//! own synthetic `[cart load failed]` line). So this log carries the same
//! thing ggo-ide's does for a cart: the driver's own per-run diagnostics
//! (load failure, exit code, fault trap, the run's start/end markers),
//! which is also exactly what gets ingested. Making the guest's own
//! `log()` lines land here needs a one-line sink on
//! `ggo_emu_core::Peripherals` -- a `ggo`-repo change, out of scope for a
//! fork-side task; see this task's report.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Rolling cap on accumulated lines. `ggo-ide`'s `emu/thread.rs::
/// UART_LOG_CAP` verbatim (itself the Tauri harness's `2000`): the oldest
/// COMPLETED lines drop first once a chatty run exceeds this many, so a
/// long session can never grow the accumulator without bound. Applies to
/// completed lines only -- the in-progress partial is at most one line's
/// worth of text between two [`UartLog::push`] calls.
pub const UART_LOG_CAP: usize = 2000;

/// Completed lines plus whatever has been seen since the last `\n`. One
/// value behind one `Mutex`, so no reader can observe a torn state
/// between the two halves (ggo-ide's `UartAccumulator`, same reason).
#[derive(Default)]
struct Buffer {
    lines: VecDeque<String>,
    partial: String,
}

/// Cross-thread line log. The emulator thread pushes bytes; the panel
/// reads (never blocking on anything but the mutex) from `render` and at
/// end-of-run.
#[derive(Clone, Default)]
pub struct UartLog(Arc<Mutex<Buffer>>);

impl UartLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly-produced bytes, splitting completed lines off (each
    /// with a trailing `\r` trimmed, so CRLF output doesn't carry a stray
    /// carriage return into the ingested `uart.text` column) and enforcing
    /// [`UART_LOG_CAP`] as each new line lands. Decodes lossily rather
    /// than rejecting invalid UTF-8: `uart.text` is a plain string column
    /// with no encoding guarantee from the guest.
    ///
    /// A no-op for an empty slice, so a caller in a per-frame loop never
    /// takes the lock needlessly.
    pub fn push(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut buf = self.0.lock().unwrap();
        buf.partial.push_str(&String::from_utf8_lossy(bytes));
        while let Some(pos) = buf.partial.find('\n') {
            let line = buf.partial[..pos].trim_end_matches('\r').to_string();
            buf.partial.drain(..=pos);
            buf.lines.push_back(line);
            if buf.lines.len() > UART_LOG_CAP {
                buf.lines.pop_front();
            }
        }
    }

    /// Convenience for the driver's own diagnostics, which are always
    /// whole lines: `push(format!("{line}\n").as_bytes())`.
    pub fn push_line(&self, line: impl AsRef<str>) {
        self.push(format!("{}\n", line.as_ref()).as_bytes());
    }

    /// Every line accumulated so far (completed lines oldest first, plus
    /// the current unterminated partial if any) -- what gets ingested.
    ///
    /// ggo-ide's equivalent is `UartHandle::take`, which DRAINS. It has to:
    /// its emu thread is persistent and reused across runs, so `take` is
    /// also that page's per-run reset. Here the log is owned by the
    /// per-run [`crate::drive::Session`] and a new run gets a new one, so
    /// draining would buy nothing and would blank the console the instant
    /// a run ended -- which, in cart mode, is when the only interesting
    /// lines have just been written.
    pub fn lines(&self) -> Vec<String> {
        let buf = self.0.lock().unwrap();
        let mut out: Vec<String> = buf.lines.iter().cloned().collect();
        if !buf.partial.is_empty() {
            out.push(buf.partial.clone());
        }
        out
    }

    /// The newest `n` COMPLETED lines, non-destructively -- safe to call
    /// from `render`, i.e. up to 60 times a second.
    ///
    /// The in-progress partial is deliberately excluded (unlike
    /// [`Self::lines`]): showing it on every poll would make the last
    /// console line visibly rewrite itself, which reads as flicker rather
    /// than a growing log. It appears once its terminating `\n` lands.
    /// ggo-ide's `peek_tail` makes the same call for the same reason.
    pub fn peek_tail(&self, n: usize) -> Vec<String> {
        let buf = self.0.lock().unwrap();
        let start = buf.lines.len().saturating_sub(n);
        buf.lines.iter().skip(start).cloned().collect()
    }

    /// Number of completed lines currently held (the console header's
    /// count).
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_splits_on_newlines_and_holds_the_partial_back() {
        let log = UartLog::new();
        log.push(b"first\nsecond\nthi");
        assert_eq!(log.peek_tail(10), vec!["first", "second"]);
        assert_eq!(
            log.lines(),
            vec!["first", "second", "thi"],
            "lines() includes the unterminated tail; peek_tail does not"
        );
        log.push(b"rd\n");
        assert_eq!(log.peek_tail(10), vec!["first", "second", "third"]);
    }

    #[test]
    fn push_trims_a_trailing_carriage_return() {
        let log = UartLog::new();
        log.push(b"crlf line\r\nlf line\n");
        assert_eq!(log.peek_tail(10), vec!["crlf line", "lf line"]);
    }

    #[test]
    fn push_decodes_invalid_utf8_lossily_rather_than_dropping_it() {
        let log = UartLog::new();
        log.push(&[b'o', b'k', 0xFF, b'\n']);
        let lines = log.peek_tail(1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("ok"), "{lines:?}");
        assert!(lines[0].contains('\u{FFFD}'), "{lines:?}");
    }

    #[test]
    fn push_of_nothing_is_a_noop() {
        let log = UartLog::new();
        log.push(b"");
        assert!(log.is_empty());
        assert!(log.lines().is_empty());
    }

    /// THE cap: a run that logs forever must not grow the buffer forever.
    /// The oldest lines go first and the newest are what survive.
    #[test]
    fn completed_lines_are_capped_at_the_rolling_window_oldest_first() {
        let log = UartLog::new();
        for i in 0..(UART_LOG_CAP + 50) {
            log.push_line(format!("line {i}"));
        }
        assert_eq!(log.len(), UART_LOG_CAP, "the cap is a hard ceiling");
        let all = log.lines();
        assert_eq!(all.len(), UART_LOG_CAP);
        assert_eq!(
            all.first().map(String::as_str),
            Some("line 50"),
            "the 50 oldest lines were evicted, not the newest"
        );
        assert_eq!(
            all.last().map(String::as_str),
            Some(&format!("line {}", UART_LOG_CAP + 49)[..])
        );
    }

    /// The cap counts COMPLETED lines only: one gigantic unterminated
    /// partial is exempt (it is at most one line's worth between pushes),
    /// exactly as ggo-ide's `UART_LOG_CAP` doc says.
    #[test]
    fn the_cap_applies_to_completed_lines_not_the_partial() {
        let log = UartLog::new();
        for i in 0..UART_LOG_CAP {
            log.push_line(format!("l{i}"));
        }
        log.push(b"a very long unterminated tail");
        assert_eq!(log.len(), UART_LOG_CAP);
        assert_eq!(log.lines().len(), UART_LOG_CAP + 1);
    }

    #[test]
    fn peek_tail_returns_the_newest_n_and_never_more_than_there_are() {
        let log = UartLog::new();
        for i in 0..5 {
            log.push_line(format!("l{i}"));
        }
        assert_eq!(log.peek_tail(2), vec!["l3", "l4"]);
        assert_eq!(log.peek_tail(99).len(), 5);
        assert!(log.peek_tail(0).is_empty());
    }

    /// Non-destructive: the console reads it on every render, and that
    /// must not consume what the end-of-run ingest still needs.
    #[test]
    fn reads_do_not_consume() {
        let log = UartLog::new();
        log.push_line("only line");
        assert_eq!(log.peek_tail(10).len(), 1);
        assert_eq!(log.peek_tail(10).len(), 1);
        assert_eq!(log.lines().len(), 1);
        assert_eq!(log.lines().len(), 1);
    }

    /// Clones share one buffer -- the emulator thread writes, the panel
    /// reads.
    #[test]
    fn clones_share_the_same_buffer_across_threads() {
        let log = UartLog::new();
        let writer = log.clone();
        std::thread::spawn(move || writer.push_line("from the emu thread"))
            .join()
            .unwrap();
        assert_eq!(log.peek_tail(1), vec!["from the emu thread"]);
    }
}
