# A `write` permission type (deferred)

**Status:** Deferred. Not implemented. This memo records a future addition to
the permissions file described in `docs/design/permissions-file.md`, where
only `command` and `read` types exist today.

## The idea

Add a third rule type so patch/write access can be expressed the same way as
reads:

```jsonl
# let the model write under src/ without prompting
{'type':'write','allow':true,'path':'src/'}
# never touch generated output
{'type':'write','allow':false,'path':'dist/'}
```

`write` would reuse the same shape as `read`: `type` + `allow` +
`path`, with the same **recursive, segment-boundary** matching.

## Why it is only deferred

The write boundary is currently a strong, simple guarantee: a patch is
auto-approved only when it is an `Update` to a **git-tracked** path
(reversible, so no prompt); everything else always needs approval. Moving this
to a permission file means the boundary can be widened by editing a file, which
needs its own careful review:

- **Scope shape.** Command rules are argv prefixes; read rules are paths. Write
  rules would be paths, but possibly need to distinguish create/update/delete.
- **Interaction with the git-tracked rule.** Does `allow:true` under `src/` let
  a *new* file under `src/` be written without a prompt? That is exactly the
  "middle ground" `docs/deferred/patch-grant-scope.md` describes.
- **Deny semantics.** A `write` `allow` must not silently override the
  workspace-boundary guard; the file layer is about *which* allowed paths, not
  about escaping the workspace.

## Relationship to other memos

This overlaps with `docs/deferred/patch-grant-scope.md`: that memo asks for a
`--grant` analogue scoped to patches, this one asks for a declarative file rule.
They are two shapes of the same idea. If either is revived, decide whether the
file rule makes the `--grant` form redundant, or vice versa, before building
both.

## If revived

- Decide the exact match unit (path prefix vs. exact path) and whether a `write`
  rule can authorise a *new* file or only an update.
- Keep the workspace-boundary guard as a separate, non-overridable layer; a
  `write` rule narrows or widens *within* the workspace, it does not escape it.
- Reuse the recursive, segment-boundary matching defined for `read` in
  `docs/design/permissions-file.md`.
