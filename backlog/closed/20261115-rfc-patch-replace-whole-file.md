# RFC: add a patch edit kind that replaces a whole file

## Summary

A `patch` edit may be of kind `replace`, which carries only `path` and
`content` and replaces the file's entire contents. It carries no `before`, so
nothing is matched, and it is guarded the way an update to the same path is:
an explicit `write` rule decides, and otherwise a git-tracked file is applied
without asking while any other target is parked for approval. A replace on a
path that does not exist is the existing missing-file failure, since creating
a file is what `add` is for.

## Motivation

This answers [feedback-one-edit-per-file-in-a-patch](../closed/20261115-feedback-one-edit-per-file-in-a-patch.md),
which is the whole-file rewrite that had no good shape before this kind existed.

Updating a file today means quoting the text being replaced verbatim in
`before`. That is right for a one-line fix. It is the wrong shape for a
document being restructured: a title that moves, a section that is rewritten
and renumbered, a file whose headings all change. In those cases the model
quotes far more text than the change is worth, and every quoted byte is model
output that must then match the file exactly or the call fails with `NoMatch`
or `AmbiguousMatch`.

A frequent version of this is the first write to a file the call has just
created earlier in the turn: the model knows the whole content, because it
wrote it, so it can restate it, but it has to restate the part it keeps as
`before` as well as the part it changes as `after`.

The second reason is what the model does without such a kind. Faced with
restructuring, it either quotes the whole file in `before` and the whole file
in `after` -- doubling the cost and adding a match that can fail -- or it
falls back to `command` and a shell heredoc, which circumvents the preview and
the write guard the patch tool exists to provide.

## Design

`PatchTool` gains one arm:

```rust
Replace { path: String, content: String }
```

`parse` accepts `"kind":"replace"` with `path` and `content`, rejects a
replace that also carries `before` or `after`, and applies the existing
`PATCH_MAX_FILE_BYTES` limit to `content`. The schema lists `replace` in the
`kind` enum and says which members each kind requires.

`check_patch_write` gains no new rule. A replace is classified exactly as an
update to the same path: rule first, then git tracking, then approval. That
keeps the human-facing question -- may this path be written, and can it be
undone -- the same for both kinds.

A replace requires the file to exist, the way an update does; creating a file
is what `add` is for, so a replace on a missing path reports the existing
`UpdateOnMissingFile` failure. `preview_patch` records the file's bytes as the
snapshot for conflict detection, exactly as for an update, so the two-phase
applier is unchanged.

The preview and the diff renderer show a replace as the whole file's old text
removed and its new text added, capped by the existing line limit. The line
counts are the file's, so the human sees that they are approving a full
overwrite.

The tool description gains one sentence naming the new kind and saying that it
is for a whole-file rewrite; the `update` description is unchanged.

## Alternatives

Making `before` optional on `update`, with its absence meaning a whole-file
replacement: it needs no new kind, and an existing call that omits `before` by
mistake becomes a full overwrite rather than an error. That is the reason to
reject it -- a misplaced optional member should cost one failed call, not the
file's contents. A distinct kind makes the intent something the model states
rather than something inferred from a missing key.

A dedicated `write` tool that is not part of `patch`: it would need its own
approval path, its own guards, and its own preview, duplicating rules that
have to stay in step. Keeping it a kind means the rules and the approval flow
are shared by construction.

Removing the file with `command` and re-adding it with `add`: it avoids the
new kind. It is rejected because the delete is neither previewed nor guarded
-- the model would be asked to do the dangerous half through the unguarded
tool -- and because two steps where one would do is two chances to stop
halfway.

## Open questions

Does a write rule that allows `update` on a path also allow `replace` on it?
Yes: both write the whole path, and a rule that named the path meant the path.
No rule is added to distinguish them, since an allow narrower than the thing
it guards is the failure mode a reader would not predict.

Should a replace be allowed on a file the invocation created earlier in the
same turn with `add`? Yes. The path is recorded as tracked after a successful
add, so a replace on it is guarded like any other update to it, which is what
the first-write-after-add case needs.

## Outcome

Accepted as written, and implemented by commit
`cdb10bf Allow repeatable paths and add a whole-file replace to patch`,
which carried out the plan for both RFCs and added this kind in the same
change as the repeatable paths.

`PatchTool` gained `Replace { path, content }`, and `parse` accepts
`"kind":"replace"` with those two members, rejects one that also carries
`before` or `after`, and applies `PATCH_MAX_FILE_BYTES` to `content`. The
schema lists the kind and says which members each kind needs.

`check_patch_write` gained no rule: a replace is classified as an update to
the same path, so a rule first, then git tracking, then approval. A replace on
a missing path reports the existing `UpdateOnMissingFile` failure, through the
same `resolve_update_target` an update uses. The snapshot is the file's
existing bytes, identical to an update's. The preview and the diff renderer
show the whole file's old text removed and its new text added, and the line
counts are the file's.

Both open questions were answered in the design and needed nothing more: a
write rule that allows `update` on a path allows `replace` on it, and a
replace on a file the same invocation created with `add` is guarded like any
other update to it.
