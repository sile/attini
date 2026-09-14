# `--grant` scope for reads

**Status:** Implemented. `attini approve --grant session|workspace` now persists a
`read` rule (a canonical path) when the single pending call is a read that reached
outside the workspace, mirroring the command path. `--grant oneshot` stays
approve-only (no persistence). Read rules can also still be added by hand to
`permissions.jsonl`.

This memo began as the record of the asymmetry left after removing the standalone
`attini grant` / `attini grant-read` subcommands: the only `--grant` path that remained
was `attini approve --grant oneshot|session|workspace`, and it covered **commands only**.
That gap is now closed.

## The current asymmetry

With the JSONL permissions file (`docs/design/permissions-file.md`), a rule is a plain
line:

- `type: command`, matched by an `args_prefix` (e.g. `["cargo", "test"]`),
- `type: read`, matched by a `path` (recursive, segment-boundary aware).

`attini approve --grant SCOPE` persists a rule and folds it into the approval. It now
handles both kinds:

- For a **command** pending, `--grant` persists an argv prefix
  (`{"type":"command","allow":true,"args_prefix":[...]}`).
- For a **read** pending (a read that reached outside the workspace and was parked),
  `--grant` persists the **canonical target path**
  (`{"type":"read","allow":true,"path":"..."}`). This reuses the same recursive
  segment-boundary matching as a hand-written `read` rule, so the grant and a manual
  line agree exactly.

The read path is derived from the same `read_extra_root` helper the one-shot execution
uses, so the granted path and the executed one-shot root are identical.

Reads can still be granted by hand — edit `permissions.jsonl` and add a line, for
example

```text
{..."type":"read","allow":true,"path":"../docs/"...}
```

but the approval flow is no longer command-only.

## Resolution

The grant vocabulary was not overloaded: `SCOPE` (`oneshot|session|workspace`) keeps its
meaning (how long the grant lives), and the **pending call's kind** decides what gets
persisted (argv prefix vs. path). So a single `--grant SCOPE` reads fine and no separate
`--grant-read` option was needed. This is exactly the resolution the patch-side memo
(`docs/deferred/patch-grant-scope.md`) is still waiting for; patches remain out of scope
because a patch has no argv prefix and widening the write boundary is a bigger decision.

Still unimplemented: **case 2** of `read-outside-workspace-approval.md` — a `read`
`allow:false` rule that denies a path *inside* the workspace. That is a different flow
(evaluation already decided deny; it does not currently park for approval), and it is not
what this `--grant` scope addresses.

## Related

- `docs/design/approve-command.md` — the shipped `attini approve` +
  `--grant oneshot|session|workspace`; now covers commands and reads.
- `docs/deferred/read-outside-workspace-approval.md` — the "approve a read on the spot"
  design; case 1 (outside-the-workspace reads) is the flow this grant extends, case 2
  (a `read` `allow:false` rule) is still open.
- `docs/deferred/patch-grant-scope.md` — the same asymmetry for patches (no path-based
  `--grant`); still deferred.
- `docs/deferred/write-permission-type.md` — the declarative-rule version of the write
  side of the same family (`write` rule type).

## If extended

- A read grant persists a canonical **directory** path (whatever `read_extra_root`
  resolved the requested target to). A finer shape (exact file only) would need a new
  matcher; not needed so far.
- The same "kind decides the payload, `SCOPE` decides the lifetime" pattern is the model
  for the patch-side `--grant` if it ever lands (`docs/deferred/patch-grant-scope.md`).
