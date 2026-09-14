# Reading outside the workspace, with on-the-spot approval

**Status:** Implemented. A read-only call (`read`/`list`/`search`) that resolves
outside every root is parked as a `Pending` (`PendingToolKind::Read`) instead of
erroring, and `attini approve` re-runs that single call. `attini approve --grant
session|workspace` can persist a `read` rule so a later read of the same path
runs without a prompt.

This is the read side of the approve/grant family; the command side and the
approve contract live in `docs/design/approve-command.md`, the rule format in
`docs/design/permissions-file.md`, and the patch side in
`docs/design/patch-grant-scope.md`.

## Problem

Read-only tools (`read` / `list` / `search`) can only touch paths under the
workspace root or under a root granted **before the invocation starts** (a
`read` rule in `permissions.jsonl`). If the model wants to look at something
outside the workspace *now*, there is no way for it to ask and have the human
approve that specific read. The human must already know the path and pre-grant
it. That is fine when the path is known up front and awkward when it is
discovered mid-task.

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
root).

The read path is derived from the same `read_extra_root` helper the one-shot
execution uses. Reads can also still be granted by hand -- edit
`permissions.jsonl` and add a line:

```text
{"type":"read","path":"../docs/"}
```

## Scope: a read rule is allow-only

A `read` rule is not a gate: it only **widens** the roots a read may reach
(`extra_read_roots`). There is no read deny, and the `allow` field may be
omitted (it is always allow). See "Why read has no deny".

### Grant vocabulary

The grant vocabulary is not overloaded. `SCOPE` (`oneshot|session|workspace`)
keeps its one meaning -- the lifetime -- and the **pending call's kind** decides
what gets persisted (argv prefix for a command, a path for a read or patch). So
a single `--grant SCOPE` reads fine and no separate `--grant-read` option is
needed. The patch side follows the same shape
(`docs/design/patch-grant-scope.md`).

## Why read has no deny

A read deny cannot be enforced. The model can read any file it can name through
the `command` tool (`cat`, `grep`, ...); the workspace boundary limits only
`read`/`list`/`search`, not `command`. Closing every such bypass would mean
parsing command lines -- open-ended and not worth it. A read deny would be a
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
workspace) remains. The standalone `attini grant` / `attini grant-read`
subcommands were removed earlier; the `--grant` flag on `attini approve` is the
only grant entry point.

## How to verify

1. `attini tell -s S "... read ../outside/out.txt ..."` -> parks (exit 10) with
   a read pending.
2. `attini approve -s S` -> runs the read once, prints the content.
3. `attini approve -s S --grant session` on a read pending -> appends a `read`
   rule (workspace-relative inside, absolute outside) to the session
   `permissions.jsonl`; a later read of that path needs no prompt.

## Related

- `docs/design/approve-command.md` -- the `attini approve` command and `--grant`.
- `docs/design/permissions-file.md` -- the JSONL format; read rules are
  allow-only.
- `docs/design/patch-grant-scope.md` -- the same pattern for patches (`write`).
- `docs/design/write-permission-type.md` -- the declarative `write` rule type.
