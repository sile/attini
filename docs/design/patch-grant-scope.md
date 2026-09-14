# Patch grant scope

**Status:** Implemented. `attini approve --grant session|workspace` persists a
`write` rule when the pending call is a patch, the same shape the pending
call's kind decides for commands (argv prefix) and reads (path). The pending
flow itself and the approve/grant contract live in
`docs/design/approve-command.md`; the declarative rule lives in
`docs/design/permissions-file.md`.

## The idea

`--grant SCOPE` in `attini approve` persists a rule so a later, similar call
runs without a prompt. The payload is decided by the pending call's **kind**:

- **command** -> an `argv_prefix` rule (`cargo test`);
- **read** -> a `read` path rule;
- **patch** -> a `write` path rule.

`SCOPE` keeps its one meaning everywhere: the lifetime (session or workspace).
This mirrors the read side (`docs/deferred/grant-read-scope.md`); the file shape
is `docs/design/write-permission-type.md`.

## Where a patch grant can originate

A `--grant` only acts on a **pending** call. A patch parks only when its
preview succeeds but it is not auto-applied. In practice that is:

- an `Add` of a new, non-gitignored file (the heuristic never auto-approves a
  new file),
- an `Update` on a file not tracked by git (including every non-scratchpad
  write in a non-git workspace), or
- an `Update` that a `write allow:true` rule covers but the dispatch layer still
  chooses to prompt.

The only remaining hard `preview_patch` **error** for a would-be write is an
`Add` into a gitignored region (`IgnoredParent`) or a Layer-1 / deny-rule
protected path; a `--grant` cannot originate from those. A gitignored parent is
lifted by hand-editing a `write` rule (see `docs/design/write-permission-type.md`),
not by `--grant`.

## What is persisted

The granted path is stored **workspace-relative** when the target is inside the
workspace, matching the form `write` rules are evaluated against
(`workspace_relative_write_target`), so a grant and a deny rule compare
like-for-like under last-match-wins. A path that cannot be reduced to a
workspace-relative form (outside the workspace) keeps the raw path.

A single `--grant` persists **one** rule, so a patch touching more than one
distinct path is rejected as ambiguous (`--grant` is per-path); approve one at a
time.

## Grant contract

The approve-then-grant independence rules from `docs/design/approve-command.md`
apply unchanged:

- the grant is best-effort -- the approval already stands, and a grant failure
  is a one-line warning, not a rollback;
- an identical `allow:true` rule already present is a no-op;
- an identical `allow:false` rule is a conflict (`ExistingWriteDenyConflict`),
  reported rather than silently appended.

## How to verify

1. `attini tell -s S "... add a new file pg-smoke/new.txt ..."` -> parks
   (exit 10) with a patch pending.
2. `attini approve -s S --grant session` -> appends
   `{"type":"write","allow":true,"path":"pg-smoke/new.txt"}` to the session
   `permissions.jsonl`.
3. A subsequent patch on the same path prints
   `[patch] auto-approved: ... (write rule)` with no prompt.

## Not in scope

- A directory-wide patch grant (a `write` rule already covers a directory
  recursively; grant the directory by hand-editing, or grant the exact file).
- Globs (a pure path prefix suffices).
- Distinguishing `Add` from `Update` in the persisted rule.
