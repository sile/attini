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

<!-- NOKORI_TODO: the one section written later, when `nokori close` moves
this file. Leave it empty until then, comment and all.

The sections above are written before the item moves: by the time `nokori
close` moves this file, they are written, their comments are gone, and the
change is committed.

Those sections may be rewritten as long as the item is open, as often as the
understanding changes. What they may not become is a description of work
already done. `nokori close` needs the change committed, so whatever they say
at that point is what the moving commit records.

Say whether the RFC was accepted or rejected, and why. When it is accepted,
name the change that implements it, or the item that does, if the work happens
somewhere else.

To refer to another item, link it:
[bug-empty-input](../closed/20261114-bug-empty-input.md). Link only an item
that is already closed: never link one that is still open, not even as
background. Run `nokori explain references` for the paths and the rule.

An accepted RFC can still be overturned later. Say so in the later item and
cite this one; do not edit this file afterwards to follow it.

Once this section is written and committed, treat the file as closed: do not
edit any part of it again, and append nothing later. If something turns up
later, leave the file as it is and open a new item that cites this one and
acts on it.

Delete this comment when you write the section. -->
