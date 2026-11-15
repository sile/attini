# Plan: implement repeatable paths and a whole-file replace in `patch`

## Goal

A patch call may name any path more than once, and the edits that name it are
applied in the order they appear. A patch call may also write a whole file
with `kind: "replace"`, which carries `path` and `content` and no `before`.
The crate's own test suite passes, and some test in it exercises each of the
two: one patch call whose edits both name the same path and whose second
`before` only matches after the first edit lands, and one `replace` that
overwrites an existing file.

The two changes are carried by
[rfc-patch-multiple-edits-same-path](20261115-rfc-patch-multiple-edits-same-path.md)
and
[rfc-patch-replace-whole-file](20261115-rfc-patch-replace-whole-file.md).
This plan assumes both are accepted as written; where the plan departs from
them, the departure is argued in the step that makes it.

## Steps

- [ ] Drop the unique-path rule from `PatchInvocation::parse`; a call whose
      two edits name the same path parses. The rule's error variant, code,
      message, and hint go with it.
- [ ] Make `preview_patch` fold one path's edits in order: reads the file
      once, matches each `before` against the result of the edits before it,
      and reports the file's net line change rather than one edit's worth.
- [ ] Make `apply_patch` verify and write per path rather than per edit: one
      snapshot check, one fold, one temporary file, one rename. A failure in
      one path no longer undoes the paths already renamed, which is what a
      several-file call already does.
- [ ] Add `PatchTool::Replace { path, content }` and its `"replace"` parse
      arm; a replace that also carries `before` or `after` is a parse error,
      and its `content` obeys the existing file-size limit.
- [ ] Guard and preview a replace exactly as an update to the same path: rule
      first, then git tracking, then approval, and the file's existing bytes
      as the snapshot.
- [ ] Update the tool description and the parameters schema: a path may
      repeat, `replace` is one of the kinds, and each kind says which members
      it needs.
- [ ] Add the tests the goal names -- one for a repeated path whose second
      `before` depends on the first edit, one for a replace that overwrites an
      existing file -- and fix the existing tests that assert the removed
      behaviour.
- [ ] Run `cargo fmt`, `cargo clippy`, and the test suite; the failures that
      are about `git` not finding a repository in this sandbox are not this
      plan's.

## Notes

The two RFCs touch the same three functions, so they go in together rather
than one after the other: `PatchTool` is a single enum, and adding a third
kind while also folding paths means one pass over the `match` arms instead of
two.

The step to cut if this turns out too big is the per-path `apply_patch`. The
current applier already re-checks every snapshot before writing anything, so
a version that folds a path's edits but still prepares one temporary file per
edit compiles and is correct -- it is just less careful about what a partial
rename leaves behind. Cutting it keeps both features and gives up only the
failure granularity.

The sandbox this runs in has no working `git` repository, so
`tests/test_patch.rs` and `tests/prop_patch_write_guards.rs` fail with
`fatal: not in a git directory` before any of this code runs. Those two are
not a signal about this change.

## Outcome

<!-- NOKORI_TODO: the one section written later, when `nokori close` moves
this file. It stays empty while the plan is open, this comment and all, and
the comment is deleted in the same edit that writes the section -- which is
the close, since `close` refuses a file whose outcome is still empty.

The sections above are written before the item moves: by the time `nokori
close` moves this file, they are written, their comments are gone (this one
with them), and the change is committed.

Those sections may be rewritten as long as the item is open, as often as the
understanding changes. What they may not become is a description of work
already done. `nokori close` needs the change committed, so whatever they say
at that point is what the moving commit records.

Say which steps ran, which did not, and why the ones that did not were left
out. Name the change that carried the plan out, or the item that did, when
the work happened somewhere else.

To refer to another item, link it:
[bug-empty-input](../closed/20261114-bug-empty-input.md). Link only an item
that is already closed: never link one that is still open, not even as
background. The rule is about this section and not the file: a closed item
may not cite an open one, while an open item may cite either, so a section
above this one may link an open item. Run `nokori explain references` for the
paths and the rule.

An unchecked box in a closed plan means the step was decided against, not
forgotten: `close` does not refuse a plan with steps left, so this section is
where the decision is recorded. A plan that stops partway through and should
be picked up again is not closed -- it stays in `backlog/` with the unchecked
boxes showing where it stopped.

Once this section is written and committed, treat the file as closed: do not
edit any part of it again, and append nothing later. If something turns up
later, leave the file as it is and open a new item that cites this one and
acts on it.

Delete this comment in the same edit that writes the section, and commit both
together before the move. -->
