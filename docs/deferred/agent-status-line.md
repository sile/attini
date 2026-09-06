# Agent command status line (proposal, not implemented)

**Status:** Proposal / design note. Not implemented. This document records the idea
of printing a one-line status when an `attini agent` invocation ends, so the human
can tell which session and model were used and what the invocation did, and how to
implement it.

## Problem

Running `attini agent ...` does not tell you which session was used. The session
name resolves by precedence (from `main.rs`): `-s/--session`, then the
`ATTINI_SESSION_NAME` env var, then the default `main`. Likewise the model name
resolves from `--model`, then the `ATTINI_MODEL_NAME` env var, then the default
`deepseek-v4-flash`. When a wrapper script or `plan run` invokes `attini agent -s
repo-a` then `-s repo-b`, it is easy to lose track of which session advanced, with
which model, and how it ended — the only thing printed at the end is the model's
streamed output.

The transcript / metrics are only inspectable afterwards via `attini session
metrics`, or by `jq`-ing a `--transcript` file. There is no human-readable
end-of-invocation breadcrumb.

## What it solves

- Disambiguates the active session and model at the invocation boundary (the core
  ask).
- Summarises the outcome in one line: end reason, exit code, duration, turns,
  token usage.
- Surfaces **context size** — how close the conversation is to the 64 K window / the
  compaction trigger — so the human knows when to compact or start a fresh session.
- Keeps stdout clean: the line goes to **stderr**, so no model output is interleaved.

## Where it goes (grounded in the code)

`agent_cli::run()` (agent_cli.rs) is the single choke point for an `attini agent`
invocation. At the end it already:

- derives `(reason, exit_code)` from the `Driven` outcome
  (`InvocationEndReason::{Completed, AwaitingApproval, SessionToolCallExhausted,
  Error}` mapped to `EXIT_OK` / `EXIT_AWAITING_APPROVAL` / `EXIT_ERROR`);
- computes `duration_ms = end_ts - start_ts`;
- appends a `MetricsSnapshot` from `counters.to_metrics_entries(duration_ms)` and an
  `InvocationEnd { ts, reason }`;
- returns `RunOutcome::Exit(exit_code)` (and prints `attini: {e}` to stderr on the
  `Err` path).

So the status line needs no new data sources for most fields. Emit it with an
`eprintln!` right before each `return RunOutcome::Exit(...)` — covering the `Ok`
and `Err` branches alike. `cfg.session_name`, `cfg.model`, `counters`, and
`duration_ms` are all in scope there.

### Why stderr, not stdout

stdout is reserved for the streamed model content (`ProgressSinks.content =
&mut stdout`). The agent's own diagnostics already go to stderr
(`attini: ...`, `[cap] ...`, `[compaction] ...`, `[skill_load] ...`), so the status
line belongs there too and never corrupts a `| jq` / redirect on stdout.

## Format proposal

One line, `key=value` so it is greppable:

```
attini: [agent] model=deepseek-v4-flash session=main reason=completed exit=0 turns=3 duration=9.4s prompt=1234 completion=567 ctx=20736/65536
```

- `model=` — `cfg.model` (could differ from the default because of the
  `ATTINI_MODEL_NAME` env fallback / `--model` override).
- `session=` — `cfg.session_name` (the disambiguator; could differ from the
  positional arg because of the env fallback / default).
- `reason=` — `InvocationEndReason::as_str()`: `completed | awaiting_approval |
  error | session_tool_call_exhausted`.
- `exit=` — the process exit code (`EXIT_OK` / `EXIT_AWAITING_APPROVAL` / `EXIT_ERROR`).
- `turns=` — `counters.turns`.
- `duration=` — `duration_ms`, humanised (`9.4s`); the raw `duration_ms` stays in the
  metrics snapshot.
- `prompt=` / `completion=` — the invocation's billed totals
  (`prompt_tokens_billed_total`, `completion_tokens_total`) already in `counters`.
- `ctx=` — the **current** context size. See the note below: this is NOT
  `prompt_tokens_billed_total`.

### About "context size"

`prompt_tokens_billed_total` is a **cumulative** sum of every call's input tokens in
this invocation, not the current conversation size. The real measure of "how big is
the context right now" is the **last** per-call `prompt_tokens`. Two ways to get it:

1. Add `prompt_tokens_last: u64` to `Counters`, set it in `drive()` where `usage` is
   read (`usage.prompt_tokens.unwrap_or(0)`). Cheap and available in `run()`.
2. Read the last `SessionRecord::TokenUsage` from the session (heavier, re-opens the
   transcript).

Recommend **option 1** (a new counter). The model window is 64 K
(`COMPACTION_TRIGGER_TOKENS = 16_384` is deliberately 1/4 of it), so expose
`ctx=<last_prompt_tokens>/65536`. That tells the human how close to compaction the
session is.

## Scriptability / gating

- Default: **on, to stderr**. stderr is diagnostic and not normally machine-consumed;
  `2>/dev/null` callers already drop every stderr diagnostic.
- Offer an opt-out env gate to mirror the project's existing env-gated knobs
  (`ATTINI_SESSION_NAME`, `ATTINI_IS_SUBAGENT`): `ATTINI_STATUS_LINE=0` disables it.
  (If default-ON proves too noisy for scripts that mix stderr, flip the default to
  off and gate on `ATTINI_STATUS_LINE=1`.)

## Scope

- `attini agent` only (`agent_cli::run`). Not `tui` (interactive, already shows
  state) and not `chat` (one-shot, no session loop).
- Resume paths (`-s NAME --approve` / `--reject`) also flow through `run()` and will
  print the line — which is the right behaviour: it confirms the previously parked
  session resumed to `completed`.

## Options to settle

1. **Stream:** stderr (recommended) vs stdout.
2. **Default on/off:** env-gated off (recommended) vs always-on vs flag.
3. **"Context size":** cumulative bills vs last-call `prompt_tokens`. Recommend
   last-call `prompt_tokens` (a new `prompt_tokens_last` counter) so `ctx=` is a real
   context indicator.
4. **Format:** single line `key=value` (recommended, greppable) vs a multi-line block
   like `print_session_metrics_human`.
5. **Duration:** humanised (`9.4s`) in the line, raw `duration_ms` in the snapshot.
6. **Model field:** include `model=` (recommended; the model also has an env/flag
   fallback, so confirming it matches the session is useful).

## Implementation cost

**Small.** Confined to `agent_cli.rs`. All needed values (`model`, `session_name`,
`reason`, `exit_code`, `duration_ms`, `counters`) are already in `run()`.

- Add one `eprintln!` in `run()` (or a thin wrapper) covering `Ok` and `Err` exits.
- If `ctx=` is wanted, add `prompt_tokens_last: u64` to `Counters`, update it in
  `drive()`, and (optionally) include it in `to_metrics_entries`.
- Extract the formatting into a pure helper (e.g. `render_agent_status_line(...) ->
  String`) and unit-test the format; keep the `eprintln!` thin. This avoids an IO
  test asserting on stderr.
- Tests: helper format test; a test that `prompt_tokens_last` is updated; and a test
  that the line is produced on the `AwaitingApproval` (parked) path too.

No change to the Sans I/O core, the approval state machine, or the tool surface. It
is purely CLI-surface output.

## Relation to other notes

Unrelated to `command-tool-timeout.md`; it does not touch `child_output` or the
command tool. Same location, separate concern.

## Decision

**Proposal / not implemented.** Recorded so the idea is not lost. Implement when a
single-line end-of-invocation diagnostic is wanted; the MVP is the stderr line with
`model=` + `session=` + `reason=` + `duration=`, plus `ctx=` via a
`prompt_tokens_last` counter.
