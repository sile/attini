# Tool calls: dispatch, batching, and the stranded read-only gap

**Status:** Implemented. This document records how attini handles a single
assistant turn that emits multiple tool calls — how each call is dispatched,
what happens when one needs approval, and how unanswered calls are repaired
on resume. It also records the system-prompt note added to steer the model
away from a known corner of the design.

## Overview

Within one assistant turn the model may emit several tool calls at once (an
`assistant` message carrying a `tool_calls` array). Every call must be
answered with a matching `tool` result or the conversation is malformed, so
the driver walks the calls in order and makes sure each one ends up either
executed or explicitly cancelled.

Tool calls fall into two classes:

- **Read-only** — `read`, `search`, `list`. No side effects; safe to run
automatically.
- **Approval-requiring** — `command`, and `patch` writes that are not
auto-approved. These need a human decision before they run.

## Dispatch order and the `suspending` flag

The driver (`src/tell_cli.rs`) iterates the turn's `tool_calls` in order and
handles each one:

- **Approval-requiring call.** It cannot execute now, so it is built into a
  `Pending` and pushed onto a `parked` list. Setting `parked` also raises a
  `suspending` flag: once an approval-pending call has been seen, later calls
  in the *same* turn are not auto-executed either, so the side-effect order is
  not silently rearranged.
- **Read-only call, before any `suspending`.** Runs immediately and its result
  is appended.
- **Read-only call, after `suspending`.** Neither executed nor parked — it is
  simply left with no `tool` result.

At the end of the scan, the `parked` calls are written to `pending.json` and
the invocation suspends with `awaiting_approval`. Multiple approval-requiring
calls in one turn are all parked and all saved; `attini approve` then
approves them together. ("Why does the model sometimes ask for several
commands?" is a separate concern from the gap below.)

## The stranded read-only gap

The `suspending` flag was meant only to stop *side-effecting* tools from
running after an approval-pending one. But because it also suppresses
trailing read-only calls, a read-only call that appears *after* an
approval-requiring call in the same turn is neither executed nor parked. It
has no `tool` result, so on the next resume `repair_orphaned_tool_calls`
synthesizes a cancellation (`reason: unanswered_tool_call_repair`) and the
model re-issues the call on a later turn.

Behavior by shape of the turn:

| Turn contains | Outcome |
|---|---|
| command + command | both parked, both approved |
| command + read-only | command parked; **read-only stranded → repaired** |
| read-only + command | read-only runs, then command parked |
| read-only + read-only | both run |

This is not data loss — the repair pass guarantees every `tool_call` gets an
answer — but it costs an extra cancel/re-issue round trip whenever the model
mixes a read-only call into a turn that also contains an approval-requiring
call.

## `repair_orphaned_tool_calls`

On resume (a fresh prompt, or `attini approve`), the driver reconciles the
assistant turns against the tool results that actually exist. Any assistant
`tool_call` without a matching `tool` result is given a synthesized result
that marks it cancelled (`reason: unanswered_tool_call_repair`,
`auto_decided_by.scope = repair`). This keeps the message history valid —
every call is answered — and lets the model re-issue whatever it still needs.
It is a safety net for the gap above, not a substitute for fixing the
dispatch order.

## Mitigation: the tool-batching note

Rather than reorder dispatch (which risks breaking the assistant→tool
pairing), attini nudges the model with a system-prompt note,
`render_tool_batching_note`, injected once in `build_initial_messages`
alongside the scratchpad note. The header is `# Tool call batching` and the
gist is:

- If a turn contains an approval-requiring call (`command`, or a non-auto
  `patch`), put it **last or alone** in the turn.
- Do not append read-only calls (`read` / `search` / `list`) after an
  approval-requiring call — they will be stranded and cancelled.

The note is guidance only; the orphan-repair pass remains the hard guarantee.

## Why not the structural fix

Making dispatch order-independent (so a trailing read-only call could still
run after a pending one) would break the assistant→tool continuity: a `tool`
result must immediately follow the assistant `tool_calls` it answers, and a
suspended turn has not produced its approval-requiring result yet. Running
and appending a read-only result first would interleave the answers. The
system-prompt note avoids that without touching the message-pairing
invariants.

## How to revisit

If the note proves insufficient and the cancel/re-issue round trip is
observed frequently, reconsider:

1. Allowing pending to carry the trailing read-only calls and executing them
   only *after* approval completes, preserving order.
2. True batch approval where the turn is resolved as a unit.

Both need care around the assistant→tool pairing rule described above.
