# Command tool timeout (deferred, under re-consideration)

**Status:** Deferred (under active reconsideration). Not implemented. This document
records the design exploration for re-adding a real-clock cap to the `command` tool,
why the previous incarnation was removed, and how to revive it.

## Problem

The `command` tool runs `argv[0] argv[1..]` in the workspace via
`child_output::run_streamed`, which blocks on `child.wait()` with no cap. A command
that hangs (network stall, waiting on a lock, `bash -c "sleep ..."`, an interactive
prompt, `cargo build` blocked on a registry) stalls the entire session turn
indefinitely.

The only escape today is the human pressing Ctrl+C, which requires a human to be
watching. If the agent is run unattended (a long `attini agent` batch), a single hung
`command` call loses the whole turn and can leave the session wedged in a running
state.

This is also the only agent tool with unbounded runtime: `read` caps at 1 MiB,
`search` caps results, and the command output *display* is rate-limited — but the
tool has no wall-clock bound.

## What it solves

- Bounds the worst case: a hung command is torn down after a human-configured cap,
  the partial output is handed back to the model with `termination_reason: "timeout"`,
  and the session continues.
- Keeps the agent recoverable when nobody is watching.
- Aligns with the existing bounded-tool philosophy (byte caps for `read` / `search`).

## History: why the previous incarnation was removed

The earlier `timeout_seconds` implementation (commits `af73311` / `9884fc6`, removed
in `70cfb02`) had:

- A model-supplied `timeout_seconds` field (default 60, max 300, with
  `CommandError::TimeoutOutOfRange` for 0 / >300).
- A killer thread that slept `timeout`, sent SIGTERM, slept
  `COMMAND_KILL_GRACE_MS` (500ms), then SIGKILL.
- `termination_reason: "timeout"` inferred from `elapsed >= timeout ||
  status.code().is_none()`.

The removal commit's rationale (verbatim):

> Model no longer specifies a runtime cap; attini exec's the command with no timeout
> kill (Ctrl+C remains the interrupt path).

Weaknesses of that design that motivated the drop:

1. **The model chose its own cap.** The thing being protected is also the entity
   choosing the cap, so it can always ask for the maximum (300s). The bound is soft
   and self-defeating for runaway cases. Authority should live with the human.
2. **Direct-pid kill only.** `libc::kill(child_id, ...)` kills just the immediate child.
   A `bash -c "..."` wrapper or a command that spawns descendants (build tools,
   servers) leaves orphans running and the workspace in a bad state. Should kill the
   whole process **group**.
3. **`termination_reason` inference was fragile.** It conflated "signaled" and
   "timeout" via `status.code().is_none()`, so a child killed by an unrelated signal
   was mis-reported as a timeout.
4. **Hard-coded and non-configurable.** No human surface to tune the default / max.

The tool description still says: *"Runtime is not capped by attini; the user can
interrupt a long-running command with Ctrl+C."* — that sentence would be updated by
this proposal.

## Proposed design (if revived)

### Configuration authority lives with the human

Do **not** let the model pick the cap. Introduce a process-wide default cap, with an
optional per-call override **clamped to a hard ceiling**:

- Env var `ATTINI_COMMAND_TIMEOUT_SECONDS` (default, say, `300`). `0` disables the
  watchdog entirely (restores today's Ctrl+C-only behaviour) as an opt-out.
- Optional per-call `timeout_seconds` in the `command` invocation, clamped to
  `[1, ATTINI_COMMAND_TIMEOUT_SECONDS]`. Ask for more → clamp to the ceiling (don't
  reject). Omit → the env/default cap. A shorter explicit value is honoured so the
  model can request a fast cap for known-finite commands (e.g. `git` commands).

Alternative, simpler surface: keep the schema strictly human-controlled (read the cap
only from env / CLI flag / config) and drop `timeout_seconds` from the model JSON.
**Recommended MVP:** human-configured default cap only, no model field. Add the
clamped per-call override only if the model genuinely needs to shorten individual
commands.

### Kill mechanism: process group, SIGTERM then SIGKILL

- Start the child in a **new process group** via `CommandExt::pre_exec` calling
  `setpgid(0, 0)` (this requires `std::os::unix::process::CommandExt`). Without this,
  the child stays in the parent's group, and `killpg` would kill `attini` itself.
- On timeout, signal the whole group: `killpg(-pgid, SIGTERM)`, then after a grace
  period `killpg(-pgid, SIGKILL)`.
- Grace period constant (the removed `COMMAND_KILL_GRACE_MS = 500`; consider 1000ms;
  constant or configurable).
- This path is Unix-only. `attini` already depends on `libc` and has used
  `libc::kill` historically, so Unix is the target. Decide a non-Unix behaviour at
  compile time: either leave the watchdog a no-op (Ctrl+C-only) or refuse spawn.

### Watchdog placement

- Add an optional `timeout: Option<Duration>` to `child_output::run_streamed` (and
  mirror it on `run_streaming_stdout` for the subagent, or leave subagent out of scope
  initially).
- `run_streamed` already spawns pump threads and blocks on `child.wait()`. Add a
  watchdog thread owning the computed `pgid` and a shared `AtomicBool done`:
  - On expiry, if not `done`: `killpg(SIGTERM)`, sleep grace, if still not `done`
    `killpg(SIGKILL)`.
  - Main thread does `child.wait()` as today, sets `done`, joins pump threads, returns.
  - Use a separate `timed_out: AtomicBool` set only when the watchdog actually fired, so
    the caller reports `termination_reason: "timeout"` rather than inferring it from
    `status.code().is_none()`.
- Acknowledge the wrap-around race (child exits naturally just as the watchdog fires)
  by checking `done` before each signal and by setting `timed_out` only if a signal
  was actually sent. The reused-pgid race is a best-effort limitation; note it in a
  code comment.

### Reporting

- Extend `termination_reason` to `timeout` (alongside `exited` / `signaled`), driven
  by the watchdog's `timed_out` flag, not by `status.code()`.
- `command_result_json` already emits `exit_code`, `termination_reason`, `duration_ms`,
  `stdout`, `stderr`; no new fields are strictly needed. Optionally append a
  human-readable note to stderr (e.g. `[attini] command timed out after Ns and was
  killed`).
- Update `CommandInvocation::definition()` (drop the "Runtime is not capped" sentence)
  and `COMMAND_PARAMS_SCHEMA` if a model field is re-added. If the model field is not
  re-added, keep the existing "ignore stray `timeout_seconds`" parse test as-is.

### Where the cap flows in the code

- The cap flows into `run_command_sync` (agent_cli.rs), which today calls
  `run_streamed(&mut cmd)`; pass `timeout` there.
- The subagent path (`subagent.rs` → `run_streaming_stdout`) is a separate, optional
  follow-up — a hung subagent also stalls the parent.

## Implementation cost

- Add a watchdog thread + process-group creation in `child_output.rs`, and thread a
  `timeout` through `run_streamed` / `run_streaming_stdout`.
- Re-add a config knob (env var + CLI flag) and plumb it from `agent` / `tui` / `chat`
  entry points into `run_command_sync`.
- Wire `termination_reason: "timeout"` (new atomic flag) and update the JSON + schema /
  description.
- Update `CommandInvocation::parse` if the model field is re-added (flip the
  "ignores stray timeout" test back to a real, clamped field).
- Tests:
  - Short-timeout test that a `sleep` child is killed with
    `termination_reason == "timeout"` (use a 1s cap on a 30s sleep; must not hang CI).
  - A guard that a naturally-exited child is **not** reported as a timeout.
  - `command_result_json` round-trip for `termination_reason: "timeout"`.
  - Process-group reaping test: spawn `bash -c "sleep 100 & sleep 100"`, time out, and
    assert no orphan remains.
  - Env-var / clamp tests.
- Be careful with flaky signal timing in CI; use a generous grace vs. the kill assert
  and avoid over-asserting on wall-clock boundaries.

**Medium complexity.** Confined mostly to `child_output.rs` + `agent_cli.rs` + a config
knob. It does **not** touch the Sans I/O core or the approval state machine — the
timeout is a runtime property of shell-side execution, not a core state change.

## Value vs. cost

**Value (high):** closes the only unbounded-runtime tool; prevents a single hung command
from stalling an unattended session; aligns with the bounded-tool philosophy of
`read` / `search`; the model gets a clear `termination_reason: "timeout"` so it can
adapt.

**Cost (medium):** a watchdog thread and signal/process-group handling, plus config
plumbing and tests. No change to the core state machine. The main risk is signal /
process-group timing complexity and CI flakiness.

**Trade-off to settle first:** human-only cap (simplest, safest) vs. model-supplied
per-call cap clamped to a human ceiling (more ergonomic, more surface).
Recommendation: start with a **human-configured default cap, no model field**, as the
MVP; add the clamped per-call override only if the model needs to shorten individual
commands.

## Related observation (adjacent, not blocking)

`COMMAND_MAX_STREAM_BYTES` (256 KiB) is declared but currently unused in
`child_output.rs`, so the tool's "Output byte totals are capped" claim is not actually
enforced — output is accumulated unboundedly (the display copy is rate-limited, but
accumulation is not). Since bounding the command tool is the theme here, this is a
natural companion fix if the tool is being hardened.

## Decision

**Deferred (under active reconsideration).** The value is clear, and the previous
removal was driven by design flaws (model-chosen cap, direct-pid kill,
non-configurable), not by the value of bounding runtime. Revisit by implementing the
MVP: a human-configured default cap, process-group SIGTERM→SIGKILL, and
`termination_reason: "timeout"`. Design note is tracked as a repo document rather than
a GitHub issue; revisit the issue form once implementation starts.

## How to revive

1. Add `ATTINI_COMMAND_TIMEOUT_SECONDS` (default 300, `0` = off) and thread it into
   `run_command_sync`.
2. Add a watchdog + process-group kill in `child_output::run_streamed`; report
   `termination_reason: "timeout"` via an atomic flag.
3. Update the `command` tool description / schema (drop "Runtime is not capped").
4. Add the timeout unit tests (short sleep, no false positive, process-group reaping).
5. Optionally extend the same watchdog to the subagent child (`run_streaming_stdout`)
   in a later increment.

Update this document once the MVP lands, or once the field decision (human-only vs.
clamped model override) is made.
