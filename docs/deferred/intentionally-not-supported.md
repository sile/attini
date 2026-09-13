# Intentionally not supported

**Status:** Deferred. Discussion note. Not yet asserted anywhere as policy.

## Why this note exists

attini has grown by *removing* whole classes of convenience features rather
than retaining them. That removal is not accidental or unfinished — it is
a policy about how attini wants to behave. This memo records the explicit
list of things attini **deliberately does not support**, so the next person
(or future conversation) does not re-add them out of habit, and so a reader
can distinguish "not implemented yet" from "intentionally not there".

## The list

### 1. Memory / automatic context persistence

- **What:** a three-tier automatic loading of `memories.md` (global
  `~/.attini/`, project `.attini/`, session `.attini/<NAME>/`) injected into
  every system prompt at startup.
- **Status:** removed (`ea845e8`).
- **Why not:** `memory` was the last implicit auto-injection. It violated the
  design philosophy "Context is requested, not discovered". If global
  persistent context is ever needed, the intended path is explicit
  `--reference PATH` or a skill.

### 2. Subagent / delegation

- **What:** a `subagent_run` tool that spawned a child `attini tell` in a
  separate session and blocked until it finished.
- **Status:** removed entirely (`ec12bc5`).
- **Why not:** it was synchronous and serial (no concurrency), its only real
  value was context isolation, and that value is already achieved with a
  separate `attini tell -s 別名` session or `attini ask -s 別名`. The tool
  only added complexity and an extra approval surface.

### 3. AGENTS.md / agent instruction files

- **What:** automatic discovery and injection of repository agent
  instruction files (AGENTS.md or similar) into the system prompt.
- **Status:** not implemented, and intentionally not planned.
- **Why not:** it is implicit context discovery by convention, which attini
  explicitly avoids. Repo-specific instructions can already be supplied
  explicitly with `--reference PATH` (a file) or `--skill PATH` (a SKILL.md).
  There is no need for a magic file-name convention.

### 4. Auto skill load / implicit skill discovery

- **What:** scanning `~/.attini/skills/` and `.attini/skills/` for skills and
  automatically listing / loading them.
- **Status:** removed (`a159db0`).
- **Why not:** skills must now be named explicitly with `--skill PATH`. The
  model has no `skill_load` tool. Context enters only because you asked for
  it at invocation start.

## The common thread

All four are the same shape: **implicit, convention-based context or
delegation that attini cannot see or control.** attini's answer to each is
"be explicit" — name the file, the skill, or run the other session yourself.
"Unsupported" here means "you ask for it explicitly, and then it works
exactly the way you asked", not "the feature is missing and should be
added".

## Why this is not just "the same as the design philosophy"

The README's design philosophy bullets describe *principles* (context is
requested, state changes surfaced, no silent side effects). This note is a
*concrete list of removals and non-goals*. It answers a different question:
"if you look for feature X, is it gone because it was bad, or just not built
yet?" The answer for these four is "bad, deliberately".

## How to revive

Do not re-add any of these by default. If a future need genuinely appears,
the bar is: the mechanism must be **opt-in and explicit**, and it must be
justifiable as an *amplifier* of what the model already does, not a
replacement for it. If such a mechanism is ever seriously proposed, this
document is the place to argue against it from recorded reasons.

This note is deferred because the removal commits already made the decision;
the remaining question is whether to ever *assert* the list publicly (for
example in the README) and in what form.
