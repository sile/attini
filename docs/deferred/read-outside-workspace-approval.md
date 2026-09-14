# Read outside the workspace with on-the-spot approval (partially implemented)

**Status:** Partially implemented. Case 1 (read outside the workspace) is implemented
as a **one-shot, non-persisted** approval: a read-only call that resolves outside every
root is parked as a `Pending` (`PendingToolKind::Read`) instead of erroring, and
`attini approve` re-runs that single call with a one-shot extra read root derived from
the requested path. Nothing is written to `permissions.jsonl`, so the grant does not
outlive the call (no resume persistence needed). Case 2 (a `read` `allow:false` rule
becoming an approval request) is **not** implemented. This memo records the design idea
related to `docs/design/approve-command.md` (the dedicated `attini approve` command +
`--grant`) and to `docs/design/permissions-file.md` (the `read` rule type).

## Two related cases

This memo covers two shapes of the same itch:

1. **Read outside the workspace.** The path is under no granted root, so today it is
   rejected outright. The model cannot ask to look at it *now*.
2. **Read under a `read` `allow:false` rule.** The path is inside the workspace, but a
   permission rule denies it. `docs/design/permissions-file.md` makes this a *silent*
   error today; ideally the model could ask and the human could approve a one-off read.

Both want the same mechanism: turn a read denial into an approval request instead of a
flat error. For now (case 2) the design keeps the error, and this memo records the
ideal.

## Problem

Read-only tools (`read` / `list` / `search`) can only touch paths under the workspace
root or under a root granted **before the invocation starts**. If the model wants to look
at something outside the workspace *now*, there is no way for it to ask and have the
human approve that specific read. The human must already know the path and pre-grant it
by adding a `read` rule to `permissions.jsonl`. That is fine when the path is known up front and
awkward when it is discovered mid-task.

## Current mechanics (grounding)

- Read-only resolution goes through `resolve_within_any(workspace_root, extra_read_roots, input)`
  (`src/tools.rs:859`). It accepts a path under `workspace_root` or any of
  `extra_read_roots`. A path outside all roots returns
  `ToolExecutionError::OutsideWorkspace`, surfaced to the model as a plain tool error.
  There is no approval hook.
- `extra_read_roots` is built **once** in `run()` (`src/tell_cli.rs:289`) from the
  allow `read` rules in `permissions.jsonl`.
  `canonicalise_extra_read_roots` dedupes and canonicalises. The `ToolExecutor` is then
  constructed once (`src/tell_cli.rs:291`) and never mutated for the rest of the session.
- Read-only dispatch (`ToolKind::ReadOnly`, `src/tell_cli.rs:644`) executes **inline**:
  `run_read_only` → `append_tool`. It has no `Awaiting(...)` variant and never pushes to
  `parked`. Contrast with `ToolKind::Patch` / `ToolKind::Command`, which can return
  `Awaiting(pending)`, which gets pushed to `parked`, saved via `save_pending`, and
  returned as `Driven::AwaitingApproval`.

So the two obstacles were: (a) read-only has no suspension path, and (b) the read
boundary is a property of the executor fixed at startup, not decided per call.

**What changed (case 1):** read-only now has a suspension path. `run_read_only` returns a
`ReadOnlyDispatch::NeedsApproval` when the executor answers `OutsideWorkspace`, and the
loop parks a `PendingToolKind::Read`. On approval, `execute_pending` re-runs the single
call through `ToolExecutor::execute_with_extra_read_root`, which appends a one-shot root
for that call only. Because the root is derived fresh from the pending's own arguments at
execution time, **no persistence and no executor mutation are needed**: the executor
stays immutable and no cross-resume state exists. The tool descriptions for `list` /
`read` / `search` now say an outside path requires one-shot human approval, so the model
knows it can ask.

## Why this is different from `patch` / `command` approval

`patch` and `command` decide *per call* whether to run or to suspend; the executor's
capability set does not change. Read access is the opposite: approving a read means
**widening `extra_read_roots` for the rest of the session** (or for one call). That makes
it closer to a grant than to a one-shot approval, and it means the `ToolExecutor` must
become mutable during the loop, or be rebuilt when a new root is granted.

## Implemented shape (case 1)

1. `run_read_only` (`src/tell_cli.rs`) turns an `OutsideWorkspace` executor result into
   `ReadOnlyDispatch::NeedsApproval`; every other error stays a plain tool error.
2. The loop's `ToolKind::ReadOnly` arm parks a `Pending` (`PendingToolKind::Read`) and
   sets `suspending` (same shape as `CommandDispatch::Awaiting`).\*
3. On `attini approve`, `execute_pending` re-parses the read call and runs it via
   `ToolExecutor::execute_with_extra_read_root(inv, extra)`, where `extra` is the
   canonicalised requested path (`read_extra_root`). The root lives only for that call.

\* Because of the tool-batching rule (approval-gated calls go last, and anything after
one is cancelled), a parked read behaves like any other approval-gated call.

## Still open (case 2 and beyond)

- **Case 2: `read` `allow:false`.** A path inside the workspace denied by a `read` rule
  still returns a silent error; promoting it to an approval request is not done.
- **Scope of approval.** Done: `attini approve --grant session|workspace` now persists
  an allow `read` rule (a canonical path) for a `read` pending, alongside the one-shot
  default. See `docs/design/approve-command.md` and `docs/deferred/grant-read-scope.md`.
- **Symmetry.** `list` (directory) vs `read`/`search` (file/prefix) may want different
  approval granularity; the one-shot root currently uses the requested path as-is.

## Resolved open questions

- **Mutability of `ToolExecutor`.** Not needed. The one-shot root is passed per call to
  `execute_with_extra_read_root`; the executor stays immutable and there is no
  `RefCell`.
- **Persistence key.** Not needed for one-shot. A session/workspace read grant would
  need one (see `grant-read-scope.md`).
- **Model signal.** Done: the `list` / `read` / `search` tool descriptions state that an
  outside path requires one-shot human approval.

## Decision

**Case 1 implemented.** A read outside the workspace is approved one-shot by default;
`--grant session|workspace` persists an allow `read` rule when the human wants it to
stick. **Case 2 is deferred** and still needs the `read` `allow:false` evaluation to park
instead of erroring (see "How to revive").

## Connection to the permissions file

With the JSONL permissions file (`docs/design/permissions-file.md`), a denied read is no
longer only "outside every root"; it can also be a `read` rule with `allow:false` that
wins the override chain. That gives the approval flow a cleaner thing to ask about: the
model requested a specific path, a rule denied it, so approve/deny for *that path*. The
ideal end state is that both cases (outside the workspace, and denied by a rule) surface
the same "approve this read?" prompt. For now the file design returns a plain error for
a denied read; this memo records the promotion into an approval request.

## How to revive (remaining work)

Case 1 (outside-the-workspace) is implemented, including the `--grant session|workspace`
read scope. The one remaining piece is case 2:

1. **Case 2.** Return `NeedsApproval` (not a plain error) when a `read` rule with
   `allow:false` wins the override chain during read-only resolution. `resolve_within_any`
   lives in `src/tools.rs`; the deny match happens in the permission layer, so the
   executor needs to consult `read` rules the way `dispatch_command` consults `command`
   rules. Park the same `PendingToolKind::Read`.

(Done: the read `--grant` scope. `plan_grant` / `apply_grant` in `src/tell_cli.rs` now
accept a `read` pending and write an allow `read` rule via `permissions::grant_read`, a
path-shaped entry rather than an argv prefix.)
