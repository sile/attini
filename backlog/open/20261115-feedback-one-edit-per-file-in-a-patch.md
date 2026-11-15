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

<!-- NOKORI_TODO: the one section written later, when `nokori close` moves
this file. Leave it empty until then, comment and all.

Two things are recorded here, and both are needed: what was decided, and what
came of it. The decision is written when it is made, even though the file is
not closed until the result is known, because the reason for a decision is hard
to recover later and is exactly what a reader of a closed file asks for.

Accepted, reshaped, declined, and held -- say which, and why. A declined
request needs its reason as much as an accepted one: the next person with the
same complaint will look for this file, and "no" without a reason sends them to
file it again.

Then say what came of it. A new item usually carries the work, so name that
item and let it stand for the change; whether it is a bug that reports the
program misbehaving, an RFC that argues the new shape, or a plan that says how
the work runs is decided then, by what the response turned out to be. When the
response was to do nothing, say that instead: an `Outcome` that says nothing
about the result is read as work nobody got to, not as a decision to leave it.

Once this section is written and committed, treat the file as closed: do not
edit any part of it again, and append nothing later. If something turns up
later, leave the file as it is and open a new item that cites this one and
acts on it.

To refer to another item, link it:
[bug-empty-input](../closed/20261114-bug-empty-input.md). Link only an item
that is already closed: never link one that is still open, not even as
background. Run `nokori explain references` for the paths and the rule.

Delete this comment when you write the section. -->
