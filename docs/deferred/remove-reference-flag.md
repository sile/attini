# Remove `--reference` (deferred)

**Status:** Deferred. Not implemented. Records the case for deleting `--reference` and what must be checked first.

## The observation

`--reference PATH` loads a file into the system prompt before the first turn. But in
practice the model can simply ignore it (observed: a `--reference`d file was present in
the prompt, yet the model did not act on it). When the goal is "make the model honor this
context," putting the text directly in the prompt — or using `--skill`, which is also
prepended to the system prompt — is more reliable than a side-channel file flag.

That raises the question: is `--reference` earning its keep at all, or should it be
removed?

## What `--reference` actually does today

Defined in `src/agent_cli.rs` (`resolve_references`, `REFERENCE_MAX_BYTES`, `ResolvedReference`),
wired from `src/main.rs` (`-r`, repeatable). Behavior:

- File ≤ 32 KiB: its body is inlined verbatim into a `# Reference files` system block.
- File > 32 KiB: the body is **not** inlined; the absolute path is granted as an extra
  read root and mentioned as readable via the `read` tool.
- Missing path: hard startup error.
- Relative paths resolve against the workspace root. It inspects nothing implicitly — a
  good fit for the "context is requested, not discovered" principle.

## Why it is a deletion candidate

Each role is already covered by an existing mechanism:

| `--reference` role | Already provided by |
|---|---|
| Inline small text into the prompt | Typing it into the prompt, or `--skill PATH` |
| Grant read access to a big file, no inlining | `--read-path PATH` (absolute path) |
| Avoid shell-quoting a long blob | `--stdin` (`-I`, up to 1 MiB, pipe or terminal) |
| Reliable "the model must honor this" | `--skill` (also prepended to the system prompt) |

So `--reference` is roughly `--stdin` + `--read-path` + `--skill` with a 32 KiB inlining
twist. It adds a flag, a constant, a resolver, tests, README surface, and a
questionable interaction (inline vs. path-only) for behavior that is otherwise reachable.

Alignment argument: attini removes redundant surface rather than accumulating flags
(cf. removal of `skill_load` discovery, `subagent`, memory, `--reject`). `--reference`
has the same shape as those — a convenience that duplicates explicit paths.

## Why it is only *deferred*, not done

A few things argue for caution before deleting:

1. **`--stdin` is the nearest replacement but has a different input model.** `--reference`
   takes a *path*; `--stdin` takes a *stream*. Piping a file (`-I < msg.md` or `cat msg.md |`)
   works, but on a terminal you must remember Ctrl+D. There is a small ergonomic gap for
   "inline this existing file."
2. **The 32 KiB auto-inline/auto-grant split is genuinely useful.** It means the user does
   not have to know or care which side of the threshold a file falls on. `--stdin` has no
   such auto-switch (a >1 MiB `--stdin` just errors).
3. **`--skill` is semantically different** (it is "instructions", loaded from a path and
   prepended) — using it to carry *data* would blur the skill concept.
4. **It may simply be underused, not harmful.** If nobody reaches for it, removing it is
   pure cleanup; if some workflows rely on the path form, removal is a small regression.

## If revived (deletion plan)

1. Confirm usage is effectively nil (no external scripts/docs depend on `-r`).
2. Delete in `src/agent_cli.rs`: `ResolvedReference`, `resolve_references`,
   `REFERENCE_MAX_BYTES`, the `reference_paths` field, the `# Reference files` block in
   `build_initial_messages`, the `resolve_references_*` tests.
3. Delete in `src/main.rs`: the `--reference`/`-r` option loop and the `reference_paths`
   wiring; fix the `--stdin` size-limit error message that currently says
   *"use --reference PATH or paste a smaller fragment"*.
4. Update `README.md` (usage line, the `--reference` paragraph, and the design-philosophy
   bullet that names it as the example of explicit loading).
5. Provide a migration note: use `--stdin` (pipe a file) or `--read-path` + `read`.

## Alternative: keep but reframe

If deletion feels too aggressive, the lighter option is to keep `--reference` but stop
advertising it as a way to "make the model use context" and document it as a *file-inlining
convenience* only, pointing users at `--skill` when they want the model to actually act on
the content.
