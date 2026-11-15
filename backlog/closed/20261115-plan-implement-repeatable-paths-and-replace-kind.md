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

- [x] Drop the unique-path rule from `PatchInvocation::parse`; a call whose
      two edits name the same path parses. The rule's error variant, code,
      message, and hint go with it.
- [x] Make `preview_patch` fold one path's edits in order: reads the file
      once, matches each `before` against the result of the edits before it,
      and reports the file's net line change rather than one edit's worth.
- [x] Make `apply_patch` verify and write per path rather than per edit: one
      snapshot check, one fold, one temporary file, one rename. A failure in
      one path no longer undoes the paths already renamed, which is what a
      several-file call already does.
- [x] Add `PatchTool::Replace { path, content }` and its `"replace"` parse
      arm; a replace that also carries `before` or `after` is a parse error,
      and its `content` obeys the existing file-size limit.
- [x] Guard and preview a replace exactly as an update to the same path: rule
      first, then git tracking, then approval, and the file's existing bytes
      as the snapshot.
- [x] Update the tool description and the parameters schema: a path may
      repeat, `replace` is one of the kinds, and each kind says which members
      it needs.
- [x] Add the tests the goal names -- one for a repeated path whose second
      `before` depends on the first edit, one for a replace that overwrites an
      existing file -- and fix the existing tests that assert the removed
      behaviour.
- [x] Run `cargo fmt`, `cargo clippy`, and the test suite; the failures that
      are about `git` not finding a repository in this sandbox are not this
      plan's. No such failure occurred: `git` works here.

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

All eight steps ran; none was left out. They landed in one commit,
`cdb10bf Allow repeatable paths and add a whole-file replace to patch`
(6 files, +509/-156), which carries both RFCs at once as the Notes argue it
should:
[rfc-patch-multiple-edits-same-path](../closed/20261115-rfc-patch-multiple-edits-same-path.md)
and
[rfc-patch-replace-whole-file](../closed/20261115-rfc-patch-replace-whole-file.md).
Both are now closed with their outcomes written.

The per-path `apply_patch`, the step the Notes offered as the one to cut, was
kept: `apply_patch` re-checks each path's snapshot, folds that path's edits
against the verified bytes, writes one temporary file per path, and renames
once per path.

Two departures, both recorded in Notes. `plan_grant` in `src/tell_cli.rs`
had to change along with the schema step -- it required exactly one edit to
choose the gated path, and now requires exactly one distinct path, so a
single-path multi-edit patch can still be granted. And the plan's premise
that `git` does not work in this sandbox is false: `tests/test_patch.rs` ran,
so the tests that asserted the removed rule were rewritten and the new ones
were seen to pass. Checked with `cargo test` (302 lib, 39 in `test_patch`,
26 in `test_tools`), `cargo clippy --all-targets -D warnings`, and
`cargo fmt --check`.
