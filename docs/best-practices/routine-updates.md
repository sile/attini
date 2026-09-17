# Making a routine update both cheap and safe to approve

Practical notes for repositories worked on by an agent (and by people) in
attini, where a single logical update is a *fixed sequence* of small steps.

Two goals pull in opposite directions here, and the point of this document is
that they can be satisfied at the same time:

- **Cheap.** The routine is one action, so it should cost one approval, not one
  per step.
- **Safe.** Widening the permission surface to make that happen is not an
  improvement — it moves the approval somewhere you can no longer see.

The technique below reaches both: it is the cheapest option *and* the narrowest
permission grant, which is why it is worth adopting rather than merely
convenient.

## The problem

Under attini, every `command` call needs approval, and approvals are per call.
A routine that is really one action therefore costs one approval per step:

```
git switch main
git fetch origin
git merge --ff-only origin/main
git mv <item> done/
<edit the item's status field>
git add …
git commit -m "…"
git push
git branch -d <merged-branch>
```

That is many prompts for one mechanical action. The obvious remedy —
auto-approving `git` (or the individual subcommands) — is the wrong fix. It
trades the prompts away by widening the permission surface to *every* `git`
invocation, including the ones you do want to look at: the commit that stages
something unintended, the push to the wrong remote, the `git mv` that lands in
the wrong directory. The steps are not dangerous in isolation; the point of
reviewing them is that they are the last moment before something becomes
permanent.

Widening rules also does not scale in the direction you want. Each new step in
the routine becomes a new rule, so the surface grows with the routine, and the
rules stay as coarse as the commands they cover (`git` is allowed, or it is
not).

A concrete example: a repository keeps design proposals and bug reports in a
tracked directory, and moves each one into a `done/` directory once the change
it describes has landed. The shape is common — after a merge, the same handful
of git operations runs every time, and only the item name and the pull request
number change.

## The fix: one script, one approval

Put the whole routine in a script in the repository, and approve *that*:

```sh
scripts/close-item.sh --pr 42 path/to/item.md < note.txt
```

One approval, because it is one call. The trade is deliberate and easy to
state:

| | per-step approvals | auto-approved `git` | one script |
|---|---|---|---|
| prompts per run | many | none | **one** |
| what is auto-approved | nothing | every `git` call | **only this routine** |
| new rule needed per step added | no | yes | no |
| reviewable before it runs | yes, step by step | no | **yes, as a whole** |

Concentrating the steps into a script is *more* reviewable, not less. Instead of
authorizing eight individual commands whose cumulative effect you reconstruct in
your head, you read one artifact — the script — and know exactly what the
routine will do. The script is committed, so the review happens once when it is
written, and every later approval is "run this thing I already read", not "run
this command I have not seen before".

This is the same reasoning as the refusal to widen `git`, applied in the other
direction: the permission surface should be *narrower* than the alternatives,
not broader. The script is the narrowest grant that covers the routine — it can
only do what it says, and only in the form it says it.

## Writing the script so it is worth approving

The script earns its single approval by being something you would not mind
auto-running. Concretely:

**Refuse to run on a dirty tree.** Check `git status --porcelain` first and
abort if it is not empty. A routine that mixes with unrelated in-progress work
is how a `git mv` or a `git add` picks up a file it did not mean to. Failing
loudly makes the precondition explicit rather than leaving the operator to
remember it.

**Resolve the target from the source of truth, not from arguments.** If the
merge commit and the merged branch name come from the forge (`gh pr view N
--json mergeCommit,headRefName`), they cannot be mistyped. Pass the pull request
number as an argument and derive the rest.

**Never invent content, but never omit it either.** Anything that is prose —
the one-paragraph outcome note, the changelog line, the note explaining *why*
the item is being closed — is read from standard input, not templated. A
placeholder that reads like boilerplate destroys the record's value, and an
optional field is a field that will be left empty. Take the body on stdin and
stop with a clear message when it is empty, so the operator is told "write the
note" rather than discovering a blank section later.

**Quote every variable.** `set -euo pipefail` at the top, and `"$var"`
everywhere. A script that is safe to auto-approve is one whose failure mode is
"stops", never "does something adjacent".

**Bail out instead of guessing on a surprising state.** If the current branch
is neither the default branch nor the branch the pull request says was merged,
stop. Do not switch branches "to be helpful" and do not continue on the current
one. The script knows exactly two valid starting states; anything else is a
condition worth a human look.

**Validate your own edits.** If the script rewrites a line (a status field, a
version number), confirm the anchor matched exactly once and fail if it did not
— a silently skipped substitution is a wrong record that looks successful.

**Print what you did.** End with the resulting commit and `git status --short
--branch`. The operator approves the script *before* seeing its effects, so the
output is the only place they can confirm the effects were the intended ones.
Use `--no-pager` on every `git` call in the script: a pager launched from a
script hangs the agent's tool call with no way to page.

## Failure modes to expect

**An incomplete routine is worse than no routine.** The script is a claim that
"these steps are all of them". If the real routine sometimes needs a ninth step,
somebody has to remember the exception every time, and the script quietly does
the wrong thing on those occasions. Keep the script to a routine that is
actually fixed, and when an exception becomes common, absorb it into the script
(take the exception as a flag) rather than documenting it as a caveat.

**The last step is the tempting one to add last.** Deleting the merged branch,
tagging the release, closing the issue — these feel like cleanup and are easy to
leave out. If a step is part of the routine, it belongs in the script, because
"remembered manually every time" is exactly the cost this is meant to remove.

**Version the script.** It is a tracked file that changes the repository. Review
it as a change like any other, and keep the changes it makes in the same shape
across revisions, so that "what the script does" stays readable at a glance.

## When this does not apply

- **The steps need judgement.** If you would sometimes skip step 4, or run
  steps 5 and 6 in the other order, the sequence is not a routine yet.
- **The steps are not repetitive.** A script is worth writing the third time you
  type the same sequence, not the first.
- **A single call already does it.** If the whole routine is one command (a
  `make release`, a `cargo publish`), approve that command's prefix and stop —
  wrapping it in a script adds a layer without narrowing anything.
