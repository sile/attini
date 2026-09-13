# Cross-repo handoff (deferred)

**Status:** Deferred. Not implemented. This document records the design exploration, the decision, and how to revive it.

## Problem

`attini tell` runs with a single workspace per session. Work that spans multiple git repos must be carried out by separate sessions, and the context produced in repo A is not available to repo B automatically. The gap is *context handoff across repos*.

## What it solves

A way for a session working in repo A to leave structured context (a "handoff") that a session in repo B can discover and consume, without the human manually re-pasting the summary.

## Existing alternatives (zero code)

These are already possible with current features:

1. **Manual copy.** The human carries the summary from repo A's session and pastes it into repo B's prompt.

2. **Scratchpad relay.** repo A's model writes `scratchpad/relay.md` using the existing `patch` tool. The human pre-grants read access in repo B with `attini session grant-read <repoA>/<session>/scratchpad/`, then repo B reads it with the `read` tool. **No new code.**

## Proposed design (if revived)

### Model tool `handoff_send` (preview + approval)

- Approval-gated, like `command`. Writes to `<target>/.attini/handoffs/<uuid>/` containing `body.md` + metadata (source session, title, kind, created_at).
- Address is the **target repo absolute path**, not a session name.
- First `handoff_take` in the target repo consumes it (moves to taken) to avoid double-processing; no broadcast.

### Model tool `handoff_take` (non-gated read + state change)

- Copies `<workspace>/.attini/handoffs/<id>/` into the session, marks it taken.

### Startup injection

- On session start, scan `<workspace>/.attini/handoffs/` and inject pending handoffs (id / title / sender) into the system prompt.

### Human CLI

- `attini handoff list --for <path>` / `show <id>` / `close <id>` to inspect and clean up.

## Implementation cost

- New approval-gated model tool: `PendingToolKind::HandoffSend`, preview, `execute_pending`, session.rs persistence, tests. Touches the approval state machine.
- New non-gated `handoff_take`.
- Cross-workspace write exception + security (allowlist deferred — human approval is the only guard until then).
- Startup injection scan.
- Documentation.

**Medium-high complexity** across multiple files and the approval flow.

## Value vs. cost

**Value (moderate, mostly ergonomic):**

- Structured, discoverable inbox with consistent layout.
- Auto-detection on startup (repo B notices without being told).
- Take semantics (no double-processing).
- Audit trail (who / when / to which repo).

**Cost:** non-trivial; touches the approval state machine and introduces a cross-workspace write exception.

**Zero-code alternative:** already available (scratchpad relay) and delivers ~80% of the benefit.

## Decision

**Deferred.** The value is real but primarily ergonomic; the implementation complexity is not justified while a zero-code fallback exists. Revisit only after the cross-repo handoff workflow is demonstrated to be frequent and the manual/relay path proves awkward.

## How to revive

Use the scratchpad relay (`grant-read`) in practice for a period. If it becomes a frequent, painful workflow, return to this document and implement — either the full model-tool design above, or a lighter middle ground (human CLI subcommands + startup injection only, which avoids the approval state machine).
