# `--grant` scope for reads

**Status:** Implemented. `attini approve --grant session|workspace` persists a
`read` rule when the single pending call is a read that reached outside the
workspace, mirroring the command path. `--grant oneshot` stays approve-only (no
persistence). Read rules can also still be added by hand to `permissions.jsonl`.

This memo began as the record of the asymmetry left after removing the standalone
`attini grant` / `attini grant-read` subcommands: the only `--grant` path that
remained was `attini approve --grant oneshot|session|workspace`, and it covered
**commands only**. That gap is now closed.

## The current asymmetry

With the JSONL permissions file (`docs/design/permissions-file.md`), a rule is a
plain line:

- `type: command`, matched by an `args_prefix` (e.g. `["cargo", "test"]`),
- `type: read`, matched by a `path` (recursive, segment-boundary aware), and
- `type: write`, matched by a `path` (same matching as `read`).

`attini approve --grant SCOPE` persists a rule and folds it into the approval. It
now handles every kind:

- For a **command** pending, `--grant` persists an argv prefix
  (`{"type":"command","allow":true,"args_prefix":[...]}`).
- For a **read** pending (a read that reached outside the workspace), `--grant`
  persists a `read` rule (`{"type":"read","path":"..."}` — read rules are
  allow-only, so `allow` is omitted). Inside the workspace the path is stored
  **workspace-relative** (the shape a human writes rules in); outside it, the
  **absolute canonical path** is stored (which the executor accepts as a root).
  This reuses the same recursive segment-boundary matching as a hand-written
  `read` rule.
- For a **patch** pending, `--grant` persists a `write` rule
  (`docs/design/patch-grant-scope.md`).

The read path is derived from the same `read_extra_root` helper the one-shot
execution uses.

Reads can still be granted by hand — edit `permissions.jsonl` and add a line,
for example

```text
{"type":"read","path":"../docs/"}
```

but the approval flow is no longer command-only.

## Resolution

The grant vocabulary was not overloaded: `SCOPE` (`oneshot|session|workspace`)
keeps its meaning (how long the grant lives), and the **pending call's kind**
decides what gets persisted (argv prefix vs. path). So a single `--grant SCOPE`
reads fine and no separate `--grant-read` option was needed. The patch side
(`docs/design/patch-grant-scope.md`) now follows the same pattern: a patch pending
persists a `write` path rule.

> The earlier "case 2" (a `read` `allow:false` rule denying an in-workspace path)
> was removed when read deny was dropped: read rules are allow-only, so there is
> no read deny to turn into an approval. See `docs/deferred/read-outside-workspace-approval.md`.

## Related

- `docs/design/approve-command.md` — the shipped `attini approve` +
  `--grant oneshot|session|workspace`; covers commands, reads, and patches.
- `docs/deferred/read-outside-workspace-approval.md` — the "approve a read on the
  spot" design (outside-the-workspace reads park for approval and feed this grant).
- `docs/design/patch-grant-scope.md` — the same pattern for patches (a `write` path
  rule); implemented.
- `docs/design/write-permission-type.md` — the declarative-rule version of the write
  side of the same family (`write` rule type); implemented.

## If extended

- A read grant persists a canonical **directory** path (whatever `read_extra_root`
  resolved the requested target to). A finer shape (exact file only) would need a new
  matcher; not needed so far.
- The same "kind decides the payload, `SCOPE` decides the lifetime" pattern is the model
  the patch-side `--grant` followed (`docs/design/patch-grant-scope.md`).
