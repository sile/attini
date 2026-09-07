# Conversation analysis tooling (deferred)

**Status:** Deferred. Not implemented. This document records the design
exploration, the decision, and how to revive it.

## Problem

Inspecting a session's `conversation.jsonl` for bloat and composition
currently requires ad-hoc shell work. The motivating case was
`.attini/main/conversation.jsonl`:

- **12.6 MB / 8403 lines.**
- **Record-kind distribution:** 2592 `tool`, 1871 `token_usage`, 1871
  `assistant`, 654 `tool_approval`, 398 `invocation_start`, 386
  `metrics_snapshot`, 86 `summary`.
- **Assistant payload split:** `reasoning` ≈ 4.29 MB across 1469 records
  (max 73,050 bytes); `content` only ≈ 0.21 MB across 1132 records.
- **Tool-result consumers:** `read` = 744 calls / ≈ 2.93 MB (max 119,018);
  `command` = 579 calls / ≈ 1.97 MB (max 43,703). Within `command`,
  `cargo test` = 69 calls / ≈ 1.40 MB.

Getting these numbers needed a chain of `grep -c` / `sort` / byte-counting
commands. That is:

- **Repetitive** — the same kind of inspection is needed whenever a session
  grows large or compaction behaves oddly.
- **Permission-gated** — exactly which helpers are allowed varies by machine
  and each step needs approval (`python3` was blocked here; `sort` was not).
- **Not reproducible** — the analysis lives in a conversation, not in
  checkable source, and can't be rerun with one command.

## What it solves

A single deterministic command that answers, for one session (or all
sessions): which record kinds dominate, which tool results are largest, which
files are read repeatedly, which external commands produce the most output,
and how much of the assistant payload is `reasoning` vs `content`.

## Current building blocks (zero code)

Existing read-only commands already cover part of this:

- `attini session show <NAME>` — invocation / approval / pending summaries.
- `attini session metrics <NAME>` — token and tool-count aggregates.
- `attini session tail <NAME>` — raw JSONL tail.
- `attini ask -s <NAME> "..."` — model summarisation with prose rendering
  (drops `reasoning`, abbreviates tool calls/results).

None of them produces a **byte-size / record-kind distribution**, which is
what the bloat investigation actually needed.

## Proposed design (if revived)

### Recommended: deterministic subcommand + interpretation skill

The two approaches are complementary, not rivals. Use a deterministic
subcommand to **measure**, and a skill to **interpret and drill down**. The
measurement is mechanical and must be reproducible; what counts as "abnormal"
is a judgment best left to the model.

### 1. `attini session analyze <NAME>` (the measurement)

A deterministic Rust subcommand that reads `conversation.jsonl` (no LOCK
required, read-only) and prints:

- **Record-kind histogram** — count + total bytes per kind (`assistant`,
  `tool`, `token_usage`, `metrics_snapshot`, `summary`, ...).
- **Assistant payload split** — sum of `content` vs `reasoning` (bytes and
  records), including the max single record.
- **Tool-result consumers** — top `read` targets by bytes and by distinct
  ranges; top `command` argv[0] / subcommands by bytes and call count; max
  single stdout.
- **Aggregate totals** — total bytes, record count, and (from `token_usage`)
  prompt / completion tokens.

Output is a human-readable table by default, with `--json` for a machine
-consumable form. Reuses `parse_conversation_line` (`src/session.rs`); no new
model call. The analysis is written to stdout only, never appended to
`conversation.jsonl`.

Crucially, this collapses the ad-hoc chain of `grep -c` / `sort` / `wc -c`
into **one approved command**, so the skill layer doesn't need shell permission
for the measurement step.

Cost: **medium** — a new subcommand under `session`, aggregation logic,
optional `--json`, tests. Much of the plumbing is shared with `run_metrics`
(`src/session_cmd.rs`).

### 2. `-S analyze-conversation` (the interpretation)

A `SKILL.md` (loaded via the existing explicit `attini agent -S PATH`)
instructing the model how to:

- Run `attini session analyze <NAME>` (and pass `--json` if machine parsing
  helps).
- Read the histogram and identify which record kind / tool result dominates.
- Explain the finding in one or two sentences (e.g. "`cargo test` is 1.40 MB
  across 69 calls; reasoning is 4.29 MB and is still being re-sent on
  resume").
- If something looks unusual, drill into the specific offender with `read`
  / `command` — for example, inspect the biggest single `read` offset or the
  largest `command` stdout, to decide whether the fix is a tool change
  (output cap, reasoning drop) or a workflow change (split the session).
- Report a recommendation rather than a restatement of the numbers.

Cost: **near zero** — just a markdown doc. But unlike a standalone skill that
runs shell analysis, it relies on the deterministic subcommand for the raw
numbers, so the measurements stay stable.

## Why the combination is better than either alone

- **Subcommand alone** is reproducible but dumb: it prints numbers, it does
  not tell the human whether the numbers are a problem.
- **Skill alone** is flexible but non-deterministic: the model assembles the
  data with permission-gated shell commands, so results vary and the raw
  numbers are not a stable artifact.
- **Together**: deterministic baseline + model judgment. One approved call
  gets the numbers; the skill interprets them and only drills further where
  the data warrants it.

## Implementation cost

- `session analyze`: new subcommand + parse/aggregate + optional `--json` +
  tests. Reuses existing record parsing; no changes to the approval state
  machine, no model call. **Medium** complexity.
- Skill: one markdown file (and an optional README pointer). **Low** cost.

## Value vs. cost

**Value (moderate):**

- One command turns a multi-step ad-hoc investigation into a one-step recap.
- Deterministic, reproducible, diff-able (`--json`) baseline.
- The skill adds interpretation, so the human gets a diagnosis, not a table.
- Directly supports future bloat / compaction diagnostics.

**Cost (medium):** the subcommand is the bulk of the work; it partially
overlaps `session metrics` but adds byte-level composition that command does
not provide.

## Decision

**Deferred.** The bloat investigation was a one-off; the real fixes (stop
re-sending `reasoning`, cap `command` output) are already decided. The
analysis tooling is ergonomic, not critical, and the next opportunity to use
it is uncertain. If revived, implement **both halves in the order above**: the
subcommand first (deterministic baseline), then the interpretation skill.

## How to revive

When the next session grows large, first write the ad-hoc analysis once more;
if it takes more than a few minutes, implement `session analyze` with
`--json`. Once the subcommand exists, add the `analyze-conversation` skill so
the model can interpret the output and launch targeted follow-up. The
byte-sized record-kind histogram is the highest-value piece of the
subcommand; the interpretation skill is what makes it actionable.
