# Read outside the workspace with on-the-spot approval (deferred)

**Status:** Deferred. Not implemented. This memo records a design idea related to
`docs/deferred/approve-command.md` (the dedicated `attini approve` command + `--grant`)
and to `docs/design/permissions-file.md` (the `read` rule type).

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
with `attini grant-read`. That is fine when the path is known up front and
awkward when it is discovered mid-task.

## Current mechanics (grounding)

- Read-only resolution goes through `resolve_within_any(workspace_root, extra_read_roots, input)`
  (`src/tools.rs:859`). It accepts a path under `workspace_root` or any of
  `extra_read_roots`. A path outside all roots returns
  `ToolExecutionError::OutsideWorkspace`, surfaced to the model as a plain tool error.
  There is no approval hook.
- `extra_read_roots` is built **once** in `run()` (`src/tell_cli.rs:289`) from the
  persistent `extra_read_paths` in `permissions.json`.
  `canonicalise_extra_read_roots` dedupes and canonicalises. The `ToolExecutor` is then
  constructed once (`src/tell_cli.rs:291`) and never mutated for the rest of the session.
- Read-only dispatch (`ToolKind::ReadOnly`, `src/tell_cli.rs:644`) executes **inline**:
  `run_read_only` → `append_tool`. It has no `Awaiting(...)` variant and never pushes to
  `parked`. Contrast with `ToolKind::Patch` / `ToolKind::Command`, which can return
  `Awaiting(pending)`, which gets pushed to `parked`, saved via `save_pending`, and
  returned as `Driven::AwaitingApproval`.

So the two obstacles are: (a) read-only has no suspension path, and (b) the read
boundary is a property of the executor fixed at startup, not decided per call.

## Why this is different from `patch` / `command` approval

`patch` and `command` decide *per call* whether to run or to suspend; the executor's
capability set does not change. Read access is the opposite: approving a read means
**widening `extra_read_roots` for the rest of the session** (or for one call). That makes
it closer to a grant than to a one-shot approval, and it means the `ToolExecutor` must
become mutable during the loop, or be rebuilt when a new root is granted.

## Sketch

1. When `resolve_within_any` fails with `OutsideWorkspace`, instead of returning the error
   immediately, park a `Pending` describing the requested path and suspend the invocation
   (same shape as `CommandDispatch::Awaiting`).\*
2. The human approves or denies. On approve, add the requested path as a read root.
3. On resume, the new root must be reconstructed **before** the model reissues the call,
   so it has to be persisted (e.g. into `permissions.json` as an `extra_read_paths`
   entry, or a session-scoped list) and re-read in `run()` the way `grant-read` entries
   already are.

\* Because of the tool-batching rule (approval-gated calls go last, and anything after
one is cancelled), a parked read behaves like any other approval-gated call.

## Open questions

- **Scope of approval.** One path? The containing directory? A session grant? This is
  where `docs/deferred/approve-command.md`'s `--grant SCOPE<oneshot|session|workspace>`
  directly connects: the read case wants the same vocabulary, and `oneshot` is awkward
  here because a read root is inherently session-lived once added.
- **Mutability of `ToolExecutor`.** Either `extra_read_roots` becomes interior-mutable
  (`RefCell`/`RwLock`) so the loop can push a root on approval, or the executor is
  rebuilt. Both are structural; neither is needed by `patch`/`command`.
- **Persistence key.** Adding to `permissions.json.extra_read_paths` blends one-shot
  approvals into the persistent grant list (it would outlive the session unless tagged).
  A session-scoped list avoids that but adds a new state file.
- **Model signal.** The model currently learns the boundary only by hitting
  `OutsideWorkspace`. For it to *ask*, the tool description or the error must tell it
  that an approval request is possible.
- **Symmetry.** `list` (directory) vs `read`/`search` (file/prefix) may want different
  approval granularity.

## Decision

**Deferred.** The itch is real (workspace-only reads are awkward when the path is found
mid-task), but the fix is structural (mutable/rebuildable executor + a new persistence
path + a suspension path for read-only) and worth doing only together with the
`approve`/`--grant` work in `docs/deferred/approve-command.md`, so the two share one
approval vocabulary rather than growing two.

## Connection to the permissions file

With the JSONL permissions file (`docs/design/permissions-file.md`), a denied read is no
longer only "outside every root"; it can also be a `read` rule with `allow:false` that
wins the override chain. That gives the approval flow a cleaner thing to ask about: the
model requested a specific path, a rule denied it, so approve/deny for *that path*. The
ideal end state is that both cases (outside the workspace, and denied by a rule) surface
the same "approve this read?" prompt. For now the file design returns a plain error for
a denied read; this memo records the promotion into an approval request.

## How to revive

Land `docs/deferred/approve-command.md` first (dedicated `attini approve` + `--grant
SCOPE`). Then extend the same scope vocabulary to read roots: on `OutsideWorkspace` **or a
`read` `allow:false` match**, park a pending read request, and on approval add the path as
a read root under the chosen scope, persisting it so a resumed invocation rebuilds it in
`run()`.
