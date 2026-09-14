# Read outside the workspace with on-the-spot approval (implemented)

**Status:** Implemented. Case 1 (read outside the workspace) and case 2 (a `read`
`allow:false` rule) are both implemented as a **one-shot, non-persisted** approval: a
read-only call that resolves outside every root, or that is denied by a winning `read`
rule, is parked as a `Pending` (`PendingToolKind::Read`) instead of erroring, and
`attini approve` re-runs that single call. For an outside path the executor runs with a
one-shot extra read root derived from the requested path; for a denied path it runs the
same call (the deny rule is only enforced by the pre-dispatch check, not by the
executor). Nothing is written to `permissions.jsonl` unless `--grant session|workspace`
is given, so the default does not outlive the call (no resume persistence needed). This
memo records the design related to `docs/design/approve-command.md` (the dedicated
`attini approve` command + `--grant`) and to `docs/design/permissions-file.md` (the
`read` rule type).

## Two related cases

This memo covers two shapes of the same itch:

1. **Read outside the workspace.** The path is under no granted root, so it used to be
   rejected outright. The model could not ask to look at it *now*.
2. **Read under a `read` `allow:false` rule.** The path is inside the workspace, but a
   permission rule denies it. This used to be *silently* allowed, since the deny rule was
   never consulted during read resolution.

Both want the same mechanism: turn a read denial into an approval request instead of a
flat error. Both are now implemented this way.

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
  `ToolExecutionError::OutsideWorkspace`; `run_read_only` turns that into an approval
  request. (Historically it was surfaced as a plain tool error with no approval hook.)
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

## Still open

- **Scope of approval.** Done: `attini approve --grant session|workspace` now persists
  an allow `read` rule for a `read` pending, alongside the one-shot default. An
  in-workspace target is stored workspace-relative (the same shape the deny check
  evaluates), an out-of-workspace target is stored as an absolute canonical path (which
  the executor accepts as a root). See `docs/design/approve-command.md` and
  `docs/deferred/grant-read-scope.md`.
- **`search` without a prefix.** A `search` over the whole workspace (`path_prefix`
  omitted) has no single target, so the case-2 deny check is skipped for it; a file
  denied by a rule can still be read indirectly through such a search.
- **Symmetry.** `list` (directory) vs `read`/`search` (file/prefix) may want different
  approval granularity; the one-shot root currently uses the requested path as-is.

## Resolved open questions

- **Mutability of `ToolExecutor`.** Not needed. The one-shot root is passed per call to
  `execute_with_extra_read_root`; the executor stays immutable and there is no
  `RefCell`.
- **Persistence key.** Not needed for one-shot. A session/workspace read grant would
  need one (see `grant-read-scope.md`).
- **Model signal.** Done: the `list` / `read` / `search` tool descriptions state that an
  outside path, or one denied by a read rule, requires one-shot human approval.

## Decision

**Both cases implemented.** A read outside the workspace, or one denied by a winning
`read` rule, is approved one-shot by default; `--grant session|workspace` persists an
allow `read` rule when the human wants it to stick. The deny check runs before the
executor in `run_read_only`, so an in-workspace `read` `allow:false` rule parks the call
instead of silently succeeding.

## Connection to the permissions file

With the JSONL permissions file (`docs/design/permissions-file.md`), a denied read is no
longer only "outside every root"; it can also be a `read` rule with `allow:false` that
wins the override chain. That gives the approval flow a cleaner thing to ask about: the
model requested a specific path, a rule denied it, so approve/deny for *that path*. Both
cases (outside the workspace, and denied by a rule) now surface the same "approve this
read?" prompt.

## How case 2 is implemented

`run_read_only` (`src/tell_cli.rs`) consults the permission layers before running the
read. `workspace_relative_read_target` reduces an in-workspace target to its
workspace-relative form (the shape rule paths are written in, e.g. `secret_dir`), and
`evaluate_read` runs last-match-wins over `[workspace] ++ [session]`. A winning
`allow:false` rule yields `ReadOnlyDispatch::NeedsApproval`, parking the same
`PendingToolKind::Read` as case 1. The deny check lives in the dispatch layer, not in the
executor, so on approval the call runs normally (no one-shot root is needed for an
in-workspace path).

## Remaining work

- `search` with no `path_prefix` is not covered (no single target; see "Still open").
- Nothing else: the read `--grant` scope persists a workspace-relative allow rule for an
  in-workspace target so it wins over a broader deny rule under last-match-wins.
