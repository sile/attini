# `--grant` scope for reads (deferred)

**Status:** Deferred. Not implemented. This memo records the asymmetry left after
removing the standalone `attini grant` / `attini grant-read` subcommands: the only
`--grant` path that remains is `attini approve --grant oneshot|session|workspace`, and
it covers **commands only**. Read access has no `--grant` scope at all, so read rules
are added **only by hand** to `permissions.jsonl`.

## The current asymmetry

With the JSONL permissions file (`docs/design/permissions-file.md`), a rule is a plain
line:

- `type: command`, matched by an `args_prefix` (e.g. `["cargo", "test"]`),
- `type: read`, matched by a `path` (recursive, segment-boundary aware).

`attini approve --grant SCOPE` persists a **command** prefix rule and folds it into the
approval. There is no equivalent for reads:

- `--grant` has no `read` scope (a read grant is a *path*, not an argv prefix, so the
  same `SCOPE` word would be overloaded).
- The standalone `attini grant-read` command that used to exist was removed. It was a
  thin writer of `permissions.jsonl` read rules, and with the file now hand-editable it
  added a code path without adding capability.

So today the intended way to grant a read is: edit `permissions.jsonl` and add a line,
for example

```text
{..."type":"read","allow":true,"path":"../docs/"...}
```

This is deliberate (open, human-editable files) but it is a one-sided story: commands can
be granted from inside the approval flow, reads cannot.

## Why it is only deferred

1. **Reads are rarely discovered past the workspace root.** The observed friction is
   command approvals (`emit_suggested_rule` still suggests `attini approve --grant
   session|workspace`). A pre-added `read` rule is usually enough because the path is
   known before the session starts.
2. **The approval shape is unsolved.** A command grant is a prefix; a read grant is a
   path (file, directory, prefix). Folding both into one `--grant SCOPE` vocabulary would
   overload `SCOPE` and is the exact problem recorded in
   `docs/deferred/patch-grant-scope.md` for patches.
3. **The read approval problems belong to a different memo.** The interesting case — the
   model *asks* to read something outside the workspace (or denied by a `read`
   `allow:false` rule) and the human approves on the spot — needs a read-only suspension
   path and a mutable/rebuildable `ToolExecutor`, which is structural. That is the subject
   of `docs/deferred/read-outside-workspace-approval.md`, not of a `--grant` flag.

So this memo is narrower than `read-outside-workspace-approval.md`: it records only that
*if* a read-approval flow lands, the read side should get a `--grant` scope of the same
family as the command one, instead of only being hand-editable.

## Related

- `docs/design/approve-command.md` — the shipped `attini approve` + `--grant
  oneshot|session|workspace`; notes that standalone `grant` / `grant-read` were removed and
  that `--grant` has no `read` scope.
- `docs/deferred/read-outside-workspace-approval.md` — the fuller "approve a read on the
  spot" design; its open question on approval scope is where this memo's vocabulary would
  be decided.
- `docs/deferred/patch-grant-scope.md` — the same asymmetry for patches (no path-based
  `--grant`). Reads and patches arrive at the same "path scope vs. argv prefix" mismatch.
- `docs/deferred/write-permission-type.md` — the declarative-rule version of the write
  side of the same family (`write` rule type).

## If revived

- Decide the read scope shape: exact path, directory (recursive), or prefix. Reuse the
  recursive segment-boundary matching already implemented for `read` rules so the grant
  and the file rule agree.
- Keep the read scope separate from the command scope in the CLI surface if a shared
  `SCOPE` word does not read well (e.g. distinct `--grant-command` / `--grant-read`
  options), mirroring the patch-side note.
- Only build this **together with** the read-approval flow
  (`docs/deferred/read-outside-workspace-approval.md`); a `--grant` for reads without a way
  for the model to ask is just a second spelling of hand-editing the file.
