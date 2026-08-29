//! Run a child process while streaming its stdout and stderr to the
//! parent's stderr under a byte-rate limit, and accumulating the full
//! output of both streams for the tool result.
//!
//! The display is a user-facing copy only: bytes skipped by the rate
//! limiter are still accumulated, so callers can hand the full output
//! to the model exactly as before. The parent's stdout is never used
//! here because the CLI's final result is emitted there.

use std::io::{self, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound on bytes of a child stream displayed to the parent's
/// stderr per one-second window. Excess bytes are still accumulated.
pub const STREAM_DISPLAY_BYTES_PER_SECOND: usize = 64 * 1024;

/// Size of each read from a child pipe.
const CHUNK_SIZE: usize = 4 * 1024;

/// Captured result of a streamed child run.
#[derive(Debug)]
pub struct ChildOutput {
    /// Full accumulated stdout (lossy UTF-8, matching the previous
    /// `Command::output` behaviour).
    pub stdout: String,
    /// Full accumulated stderr (lossy UTF-8).
    pub stderr: String,
    pub status: ExitStatus,
    /// Wall time from spawn to both streams drained.
    pub duration: Duration,
}

/// Run `cmd` with piped stdout / stderr, streaming both to the parent
/// stderr under [`STREAM_DISPLAY_BYTES_PER_SECOND`] while accumulating
/// the full output. Blocks until the child exits and both pipes are
/// drained.
pub fn run_streamed(cmd: &mut Command) -> io::Result<ChildOutput> {
    let started = Instant::now();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_thread = stdout
        .map(|reader| thread::spawn(move || pump(reader, DisplayRateLimiter::new_per_second())));
    let stderr_thread = stderr
        .map(|reader| thread::spawn(move || pump(reader, DisplayRateLimiter::new_per_second())));

    let status = child.wait()?;
    let stdout_bytes = join_pump(stdout_thread)?;
    let stderr_bytes = join_pump(stderr_thread)?;
    let duration = started.elapsed();

    Ok(ChildOutput {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        status,
        duration,
    })
}

fn join_pump(handle: Option<thread::JoinHandle<io::Result<Vec<u8>>>>) -> io::Result<Vec<u8>> {
    match handle {
        Some(h) => h
            .join()
            .map_err(|_| io::Error::other("child output reader thread panicked"))?,
        None => Ok(Vec::new()),
    }
}

/// Read a child pipe to EOF, accumulating every byte and displaying
/// the stream to the parent stderr under `limiter`'s rate cap.
fn pump<R: Read>(mut reader: R, mut limiter: DisplayRateLimiter) -> io::Result<Vec<u8>> {
    let mut accumulated = Vec::new();
    let mut chunk = [0u8; CHUNK_SIZE];
    let mut stderr = io::stderr().lock();
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        accumulated.extend_from_slice(&chunk[..n]);
        limiter.write(&mut stderr, &chunk[..n])?;
    }
    Ok(accumulated)
}

/// Per-stream byte-rate limiter for the display copy. Accumulation is
/// unaffected: only the displayed prefix of each chunk is capped.
#[derive(Debug, Clone)]
pub struct DisplayRateLimiter {
    budget: usize,
    window: Duration,
    window_start: Instant,
    used: usize,
}

impl DisplayRateLimiter {
    pub fn new(budget: usize, window: Duration) -> Self {
        Self {
            budget,
            window,
            window_start: Instant::now(),
            used: 0,
        }
    }

    pub fn new_per_second() -> Self {
        Self::new(STREAM_DISPLAY_BYTES_PER_SECOND, Duration::from_secs(1))
    }

    /// Display up to the remaining window budget of `bytes`, writing
    /// to `sink`. Excess bytes are dropped from the display only.
    pub fn write<W: Write>(&mut self, sink: &mut W, bytes: &[u8]) -> io::Result<()> {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= self.window {
            self.window_start = now;
            self.used = 0;
        }
        let to_write = bytes.len().min(self.budget.saturating_sub(self.used));
        if to_write > 0 {
            sink.write_all(&bytes[..to_write])?;
            self.used += to_write;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bytes(limiter: &mut DisplayRateLimiter, sink: &mut Vec<u8>, bytes: &[u8]) {
        limiter.write(sink, bytes).expect("write");
    }

    #[test]
    fn limiter_caps_display_at_window_budget() {
        let mut limiter = DisplayRateLimiter::new(4, Duration::from_secs(1));
        let mut sink = Vec::new();
        let payload = b"abcdefghij";
        write_bytes(&mut limiter, &mut sink, payload);
        assert_eq!(sink.len(), 4, "first window caps at budget");
        // The remainder of the chunk is dropped, not carried over.
        write_bytes(&mut limiter, &mut sink, payload);
        assert_eq!(sink.len(), 4, "same window still exhausted");
    }

    #[test]
    fn limiter_resets_after_window_elapses() {
        let mut limiter = DisplayRateLimiter::new(4, Duration::from_millis(1));
        let mut sink = Vec::new();
        write_bytes(&mut limiter, &mut sink, b"aaaa");
        std::thread::sleep(Duration::from_millis(5));
        write_bytes(&mut limiter, &mut sink, b"bbbb");
        assert_eq!(sink.len(), 8, "a new window restores the budget");
    }

    #[test]
    fn limiter_allows_up_to_budget_across_chunks() {
        let mut limiter = DisplayRateLimiter::new(5, Duration::from_secs(1));
        let mut sink = Vec::new();
        write_bytes(&mut limiter, &mut sink, b"ab");
        write_bytes(&mut limiter, &mut sink, b"cde");
        write_bytes(&mut limiter, &mut sink, b"fgh");
        assert_eq!(sink, b"abcde", "partial chunk fills the remaining budget");
    }

    #[test]
    fn pump_accumulates_full_output_under_small_budget() {
        // A tiny budget must not truncate the accumulated bytes.
        let reader = io::Cursor::new(b"x".repeat(10 * 1024));
        let limiter = DisplayRateLimiter::new(1, Duration::from_secs(1));
        let bytes = pump(reader, limiter).expect("pump");
        assert_eq!(bytes.len(), 10 * 1024, "accumulation is never rate-capped");
    }

    #[test]
    fn run_streamed_captures_full_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'out-line1\\nout-line2\\n'; printf 'err-line\\n' >&2");
        let out = run_streamed(&mut cmd).expect("run");
        assert!(out.status.success());
        assert_eq!(out.stdout, "out-line1\nout-line2\n");
        assert_eq!(out.stderr, "err-line\n");
    }

    #[test]
    fn run_streamed_non_zero_exit_is_not_an_error() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 3");
        let out = run_streamed(&mut cmd).expect("run");
        assert_eq!(out.status.code(), Some(3));
    }
}
