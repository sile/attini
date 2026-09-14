# Command tool timeout

**Status:** Implemented. `attini tell` and `attini approve` accept
`--command-timeout N` (env `ATTINI_COMMAND_TIMEOUT_SECONDS`), default 180 seconds.
A `command` tool call that runs longer is killed by process group (SIGTERM, then
SIGKILL after a one-second grace) and the tool result reports
`termination_reason: "timeout"`.

## What it does

The `command` tool runs `argv[0] argv[1..]` in the workspace via
`child_output::run_streamed`. Before this feature that function blocked on
`child.wait()` with no cap, so a hung command (network stall, waiting on a lock,
`bash -c "sleep ..."`, an interactive prompt, a build blocked on a registry)
stalled the entire session turn indefinitely; the only escape was a human
pressing Ctrl+C.

The timeout bounds the worst case: the child is torn down after the cap, its
partial output is handed back to the model with `termination_reason: "timeout"`,
and the session continues. This is the last agent tool that had unbounded
runtime — `read` caps at 1 MiB, `search` caps results, and command output is
both byte-capped (see [`docs/bug/command-max-stream-bytes.md`](../bug/command-max-stream-bytes.md))
and display-rate-limited.

## Configuration

Authority lives with the human, not the model — the `command` invocation schema
is unchanged, so the model cannot pick (and therefore cannot maximise) its own
cap. This mirrors `read`/`search`, whose caps are fixed constants.

| Surface | Value |
|---|---|
| CLI flag | `--command-timeout N` (on `attini tell` and `attini approve`) |
| Env var | `ATTINI_COMMAND_TIMEOUT_SECONDS` |
| Default | 180 seconds |
| Disable | `0` |

Precedence is the same as every other knob: CLI flag, then env var, then the
default. The resolved value is stored on `TellConfig.command_timeout_seconds`
and converted to `Option<Duration>` by `tell_cli::command_timeout` (`Some(0)` and
`None` both mean "no cap").

## Kill mechanism: process group, SIGTERM then SIGKILL

- The child is started in its own process group via `CommandExt::pre_exec`
calling `setpgid(0, 0)`. Without this the child would stay in attini's own
group and `killpg` would kill attini itself.
- On expiry the watchdog signals the whole group: `killpg(pgid, SIGTERM)`, then
after `KILL_GRACE` (one second) `killpg(pgid, SIGKILL)`. Signalling the group
(not just the immediate pid) reaps `bash -c` wrappers and their descendants, so
a timed-out server or build does not leave orphans or keep pipes open.
- The path is Unix-only; attini already targets Unix and depends on `libc`.

## Watchdog placement

`child_output::run_streamed(cmd, timeout: Option<Duration>)` runs a small polling
loop in `wait_with_timeout` instead of a plain `child.wait()`:

- With `timeout = None` it does the original blocking `wait()`.
- Otherwise it polls `try_wait()` every `WATCHDOG_POLL_INTERVAL` (50 ms), checks
  the deadline, and on expiry kills the group. Polling (not a separate killer
  thread) keeps the child handle and the pgid in one place; the 50 ms granularity
  is far below any useful cap.
- `timed_out` is reported via a plain `bool` on `ChildOutput` set only when the
  watchdog actually fired, so the caller distinguishes a real timeout from a
  child that exited on its own just before the deadline. This avoids the fragile
  `status.code().is_none()` inference the earlier incarnation used (which
  conflated "signaled" with "timeout").

Pipes are always drained to EOF by the pump threads, so accumulation stays
bounded by `COMMAND_MAX_STREAM_BYTES` regardless of the timeout, and a killed
child releases the pipes promptly.

## Reporting

`run_command_sync` derives `termination_reason` in this order:

1. `output.timed_out` -> `"timeout"`
2. `status.code().is_some()` -> `"exited"`
3. otherwise -> `"signaled"`

`command_result_json` needs no new fields; it already emits `termination_reason`.
The `command` tool description now says the runtime is capped (default 180 s) and
that a killed command reports `termination_reason: "timeout"`.

## Tests

- `run_streamed_kills_child_on_timeout` — a `sleep 30` child with a 200 ms cap is
  killed and marked `timed_out`, well before 30 s.
- `run_streamed_timeout_disabled_when_none` — no cap means a fast child completes
  normally and is not marked timed out.
- `run_streamed_timeout_kills_process_group_descendants` — `sh -c 'sleep 30' &
  wait` is killed at the group level so the open pipe does not block `join_pump`.
- `command_timeout_zero_and_none_disable_the_cap` and
  `command_timeout_seconds_becomes_a_duration` — the config-to-`Duration`
  conversion.

## Why the previous incarnation was removed

An earlier `timeout_seconds` design (commits `af73311` / `9884fc6`, removed in
`70cfb02`) was dropped for flaws this design avoids: the model chose its own cap
(and could always request the maximum); it killed only the immediate pid
(orphaning descendants); `termination_reason` was inferred from
`status.code().is_none()` (mis-reporting unrelated signals as timeouts); and it
was hard-coded with no human surface to tune it.
