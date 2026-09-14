# Patch grant scope (deferred)

**Status:** Deferred. Not implemented. This memo records a future idea that
surfaces while designing `docs/design/approve-command.md` (the dedicated
`attini approve` command with `--grant SCOPE`).

## The idea

`--grant SCOPE` in `attini approve` persists a command **argv-prefix** rule, so
future invocations of the same command can run without a prompt. Patches have no
argv, so today they cannot be granted this way:

- a patch is auto-approved only when it is an `Update` to a **git-tracked** path
  (reversible, so no prompt), and
- everything else (new files, non-tracked paths) always needs approval.

There is no middle ground of the form "let this model keep patching files under
`src/` without asking." A `--grant` analogue for patches would add one.

## Why it is only deferred

1. **It is not needed yet.** Command friction is the observed pain (the current
   `emit_suggested_rule` suggestion is about commands). Patch approval has not
   been reported as tedious.
2. **The scope vocabulary is unclear.** A command grant is naturally a prefix
   (`cargo test`); a patch grant is naturally a **path** scope (`src/`, a single
   file, `tests/`?). The read side resolved the analogous worry: `SCOPE` keeps its
   meaning (lifetime) and the pending call's *kind* decides what is persisted
   (args-prefix vs. path) — see `docs/deferred/grant-read-scope.md`. A patch grant
   could follow the same pattern (a `path` payload), so the vocabulary is no longer
   the blocker. What remains is point 3.
3. **It widens the write boundary.** Auto-approved writes are currently bounded
   by "git-tracked `Update` only", which is a strong, simple guarantee. A path
   grant would let new files / non-tracked edits through without a prompt, so it
   needs its own careful boundary discussion (workspace only, deny rules first,
   etc.) rather than riding in on the approve-command change.

## Related

`docs/design/write-permission-type.md` records the now-implemented declarative-file-rule
version of the same idea (a `write` rule type in `docs/design/permissions-file.md`). The
declarative file rule exists; only the imperative `--grant` analogue is still deferred. A
file rule and a `--grant` form are two shapes of the same thing; before building the
`--grant` form, decide whether it adds anything over hand-editing a `write` rule.

## If revived

- Decide the grant shape: a path prefix, an exact path, or a glob (avoid globs
  if a pure prefix suffices).
- Reuse the same approve-then-grant independence rules from
  `docs/design/approve-command.md`: the grant is best-effort and its failure is
  a warning, not a rollback.
- Keep the command-side `SCOPE` and the patch-side scope as separate options
  (e.g. `--grant-command SCOPE` vs. `--grant-patch SCOPE`) if a shared vocabulary
  does not read well.
