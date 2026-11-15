# RFC: apply edits to one path in the order they are written

## Summary

A `patch` call may name the same path in more than one edit, and the edits
targeting one path are applied in the order they appear in the `edits` array.
The first edit on a path is matched against the file as it was at preview
time; each later one is matched against the result of the edit before it, and
the file is written once, with the folded result. The rule that a path must be
unique within a call goes away, and with it the parse error that told the
model to split a file's changes into several calls.

## Motivation

A file that is written in sections -- a markdown document, an RFC, a plan --
is revised one section at a time, and each revision is a replacement of a
small span. Today a `patch` call can only touch a path once, so those
revisions must be spread over several `patch` tool calls in the same turn.

That split has costs beyond the extra call. Every one of the sibling calls
repeats the same surrounding text in its `before`, so the cost of revising one
file grows with the number of revisions times the length of the context they
need to match. The repeated text is model output, so it is paid twice: once
to generate the call and once to carry it in the next request.

The split also feeds the transcript-corruption case fixed for orphaned tool
calls. A turn that carries several `patch` calls for one file can park on the
first approval-requiring call, leaving its siblings unanswered until the
resume repair cancels them and the model re-issues them. The repair makes that
survivable, but the situation is avoidable: the same work fits in one call if
a path may repeat.

## Design

`PatchInvocation::parse` keeps collecting edits in array order and no longer
rejects a repeated `path`. The `MultipleEditsSamePath` error variant and its
hint are removed. `PatchTool` and the parameter schema do not change, so a
model that already writes one edit per path keeps working unchanged.

`preview_patch` groups edits by their target path and keeps the groups in
first-appearance order. For each group it reads the file once, then folds the
edits in order: an `update` whose `before` matches exactly once in the current
buffer replaces that span and the buffer moves on, so the next edit is matched
against the folded text. A `before` that matches zero times reports `NoMatch`,
and one that matches more than once reports `AmbiguousMatch`, using the text as
it stands at that point in the fold. The snapshot stored for the path is the
text before the first edit, so a conflict check still means "the file did not
change under us"; the preview's line counts are taken from the folded result,
so the human sees the net change rather than one edit's worth of it.

The approval preview is rendered per path: one header, then the edits of that
path in order. The confirmation the human gives is for the whole call, which
is what it already is.

`apply_patch` re-checks each path's snapshot exactly as today, re-folds that
path's edits against the verified bytes, writes one temporary file per path,
and renames once per path. Atomicity is therefore per path, not per edit: a
failure in the fold or in phase one leaves the workspace untouched, and a
failure during the renames leaves the paths already renamed in place. That is
the same guarantee a several-file patch has today, and git makes both
recoverable, which is what the tool's auto-approval rests on.

Executing an approved batch and the plan-run path go through the same
functions, so neither needs a change.

## Alternatives

Requiring one call per path, as today: it keeps the applier trivially simple
and makes every edit independent. It is rejected because the cost is paid on
every multi-section document, and because the call-splitting is what puts
several sibling `patch` calls in one turn.

Matching every edit on a path against the original file and requiring them not
to overlap: it needs no fold and keeps each edit's meaning local. It is
rejected because it makes non-overlap a property the model must reason about,
and an edit whose `before` covers text another edit changed becomes a failure
the model can only fix by rewriting the whole call.

Applying a path's edits one re-read at a time, with a preview per edit: it
never has to report `AmbiguousMatch` against intermediate text. It is rejected
because it reads the file as many times as there are edits and because a
failure halfway makes the earlier edits visible, which the preview the human
approved never showed.

## Open questions

None. The order is the array order, the snapshot is the pre-edit text, and the
removals are limited to the uniqueness rule and its error variant.

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
