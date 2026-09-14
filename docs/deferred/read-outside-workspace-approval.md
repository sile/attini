# Read outside the workspace with on-the-spot approval (implemented)

**Status:** Implemented. A read-only call (`read`/`list`/`search`) that resolves
outside every root is parked as a `Pending` (`PendingToolKind::Read`) instead of
erroring, and `attini approve` re-runs that single call. This memo records the
design related to `docs/design/approve-command.md` (the dedicated `attini
approve` command + `--grant`) and to `docs/design/permissions-file.md` (the
`read` rule type).

> The earlier "case 2" (promoting a `read` `allow:false` rule to an approval
> request) was **removed**: read rules are now allow-only. See "Why read has no
> deny" below and in `docs/design/permissions-file.md`.

## Problem

Read-only tools (`read` / `list` / `search`) can only touch paths under the
workspace root or under a root granted **before the invocation starts**. If the
model wants to look at something outside the workspace *now*, there is no way for
it to ask and have the human approve that specific read. The human must already
know the path and pre-grant it by adding a `read` rule to `permissions.jsonl`.
That is fine when the path is known up front and awkward when it is discovered
mid-task.

## Implemented shape

1. `run_read_only` (`src/tell_cli.rs`) turns an `OutsideWorkspace` executor
   result into `ReadOnlyDispatch::NeedsApproval`; every other error stays a plain
   tool error.
2. The loop's `ToolKind::ReadOnly` arm parks a `Pending` (`PendingToolKind::Read`)
   and sets `suspending` (same shape as `CommandDispatch::Awaiting`).*
3. On `attini approve`, `execute_pending` re-parses the read call and runs it via
   `ToolExecutor::execute_with_extra_read_root(inv, extra)`, where `extra` is the
   canonicalised requested path (`read_extra_root`). The root lives only for that
   call; the executor stays immutable (no `RefCell`).

* Because of the tool-batching rule (approval-gated calls go last, and anything
after one is cancelled), a parked read behaves like any other approval-gated
call.

Nothing is written to `permissions.jsonl` unless `--grant session|workspace` is
given, so the default does not outlive the call (no resume persistence needed).

## Persisting a grant

`attini approve --grant session|workspace` persists a `read` rule for a `read`
pending. An in-workspace target is stored workspace-relative (e.g.
`secret_dir/secret.txt`), the shape a human writes by hand; an out-of-workspace
target is stored as an absolute canonical path (which the executor accepts as a
root). See `docs/design/approve-command.md` and `docs/deferred/grant-read-scope.md`.

## Scope of approval

A `read` rule is allow-only and is not a gate: it only **widens** the roots a
read may reach (`extra_read_roots`). There is no `read` deny, so there is no
"denied by a read rule" case. See "Why read has no deny".

## Why read has no deny

A read deny cannot be enforced. The model can read any file it can name through
the `command` tool (`cat`, `grep`, ...); the workspace boundary limits only
`read`/`list`/`search`, not `command`. Closing every such bypass would mean
parsing command lines — open-ended and not worth it. A read deny would be a
promise the tool cannot keep, which is worse than no promise. If a file must not
be read, keep it out of the workspace (or out of reach) instead.

`write` is different: a write is destructive and hard to undo, so it is worth
gating even though the model could also route around it. Precision pays off
there; for reads it does not.

## History

The first cut of this feature also had a "case 2": a workspace-internal `read`
`allow:false` rule parked the call. That was removed along with read deny (see
`docs/design/permissions-file.md`, "Why read has no deny"). `evaluate_read` and
the pre-dispatch deny check in `run_read_only` are gone; only case 1 (outside the
workspace) remains.

## Related

- `docs/design/permissions-file.md` -- the JSONL format; read rules are
  allow-only.
- `docs/design/approve-command.md` -- the `attini approve` command and `--grant`.
- `docs/deferred/grant-read-scope.md` -- the grant-target asymmetry history.
