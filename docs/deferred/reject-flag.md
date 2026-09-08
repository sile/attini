# `--reject` flag (candidate removal)

**Status:** Deferred. Under discussion. Not yet decided whether to remove.

## Problem

`attini agent` resumes a suspended session in three ways: a fresh `PROMPT`,
`--approve`, or `--reject`. The intuition behind `--reject` is "let the model
rethink after a rejection." This document questions whether that workflow
actually exists, and whether the flag earns its keep.

## How a rejection is delivered today

`--reject` (`src/main.rs:231`, wired to `Continuation::Reject` in
`src/agent_cli.rs:475`) does the following, with **no new user message**:

1. Appends a `ToolApproval { decision: Reject, auto_decided_by: None }`
   record — the audit log marks this as a *human explicit rejection*.
2. Appends a synthetic tool result:
   `{"error":"rejected","message":"user rejected this tool call"}`.
3. Clears pending.json.

The model then sees: `[assistant tool_call]` -> `[tool result: rejected]`.
Nothing tells it *why* it was rejected or *what* to do differently.

## The user's point (confirmed)

A bare rejection is a **pure negative signal**. The model cannot distinguish:

- "this patch has a bug" (direction: fix this)
- "this patch is the wrong approach" (direction: try something else)
- "this command is unnecessary" (direction: don't run it)

With no feedback, the model retries and will likely produce the same thing
again — the human then has to type a directed prompt anyway. So the
"reject without instruction, let the model rethink alone" case is largely
fictitious in practice.

## The dominant real workflow is already covered

"Reject + give direction" is the common case. That is exactly what
`attini agent "<new instruction>"` does:

- `repair_orphaned_tool_calls` (`src/agent_cli.rs:1919`) auto-rejects the
  unanswered `tool_call` when the loop previously suspended, appending
  `ToolApproval { decision: Reject, auto_decided_by: Some(repair) }`.
- The fresh `PROMPT` is appended as a `user` message after it.

So the natural reject-and-redirect flow needs **no `--reject` flag**; the
fresh prompt handles it, and the auto-cancel is recorded correctly.

## The only residual value of `--reject`

Audit semantics. The two paths produce different `auto_decided_by` records:

| Path          | auto_decided_by              | Meaning                          |
|---------------|------------------------------|----------------------------------|
| `--reject`    | `None`                       | human explicit rejection          |
| fresh prompt  | `Some(AutoDecidedBy::repair)` | auto-cancel due to new prompt     |

So `--reject` is the only way to record "the human deliberately rejected this
call and chose not to redirect" in the audit log. This is a metadata
distinction, not a workflow one.

## Recommendation

`--reject` is **nearly redundant**: the real "reject + redirect" workflow is
covered by a fresh prompt, and a bare rejection without direction is not a
useful standalone operation. Its only unique value is the audit label
`auto_decided_by: None`.

Options:

1. **Keep** — the audit distinction (human rejection vs auto-cancel) aligns
   with attini's "human is the final gate" philosophy.
2. **Remove** — simplify the CLI; accept that all pending-call cancellations
   become `auto_decided_by: Some(repair)` even when caused by a human.
3. **Defer** (current) — record the analysis, decide only if a real need
   appears.

## How to revive

If removing, also update `docs/deferred/no-auto-approve-mode.md` which says
"`--approve` / `--reject` remain usable inside plan mode". Revisit if a
workflow emerges where the human wants to reject and let the model retry with
no direction *and* wants the audit to record the human's explicit role.
