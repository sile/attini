# Feedback: One edit per file in a patch call

## Feedback

A patch call that edits several parts of the same file is refused: "more than
one edit targets <path> within the same patch call". The work is to write or
revise a whole markdown file, which means several separate replacements in one
file, so the refusal is hit constantly.

I first met it writing a backlog item section by section, and then writing an
RFC: the title, then each section's body. Every one of those is a replacement
in the same file, so each patch call carried exactly one of them and the file
was rewritten several times over several calls.

What I did instead was read the whole file and write it out in one go where
that was possible, and fall back to one call per edit where it was not. The
second is what I ended up doing most, since the sections are not known until
the earlier ones are written.

## Outcome

Accepted, as two changes rather than one.

The `more than one edit targets <path>` refusal goes entirely: edits to the
same path in one patch call are applied in the order they appear. The reason
the rule existed was that a second edit's `before` would be matched against
the file as it was at the start of the call, so the model could not see what
the first edit had done. With the edits applied in order and each `before`
matched against the result of the one before it, that ambiguity is gone, and
the simplest description of a patch call is that its edits happen left to
right.

Adding a second kind, `replace`, covers the case that started this: writing a
whole file. It writes the entire contents without a `before`, so a file that
is being created or rewritten in one go needs no anchor text at all and cannot
fail on a `before` that does not match. This is cheaper than the `before` and
`after` pair for whole-file work, since the outgoing lines never have to be
repeated, and it leaves `update` for the case it is good at: a small change in
a large file.

The two were kept apart rather than folded into one change because they answer
different halves of the complaint. Repeatable paths fix the section-by-section
editing that was hit constantly; `replace` fixes the rewrite that the tool had
no good shape for. Either one alone would have left the other case as awkward
as it was.

The work is carried by two RFCs, both still open: one for edits to the same
path repeating within a call, and one for `replace` writing a whole file.
