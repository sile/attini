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
  design philosophy "Context is requested, not discovered". If persistent
  context is ever needed, put it in the prompt explicitly (or paste via
  `--stdin`).

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
  explicitly avoids. Repo-specific instructions are just text — put them in
  the prompt when you start a session. There is no need for a magic
  file-name convention.

### 4. Auto skill load / implicit skill discovery

- **What:** scanning `~/.attini/skills/` and `.attini/skills/` for skills and
  automatically listing / loading them.
- **Status:** removed (`a159db0`).
- **Why not:** the model has no `skill_load` tool; context enters only
  because you asked for it at invocation start. (The explicit `--skill PATH`
  flag that replaced discovery has itself since been removed — see item 5.)

### 5. `--skill` and `--reference` flags

- **What:** `--skill PATH` / `-S` (prepend a `SKILL.md` body to the system
  prompt) and `--reference PATH` / `-r` (inline a file, or grant it as a read
  root when oversized).
- **Status:** both removed.
- **Why not:** the only thing they provided over the prompt was "this text
  goes into the system prompt instead of the user message" — not a guarantee
  the model will obey (there is none, by LLM nature). If you want context in
  a session, type it in the prompt; for larger blobs paste via `--stdin`.
  The remaining real capability — granting read permission outside the
  workspace — is a separate concern handled by `read` rules in
  `permissions.jsonl` (and later, an approval flow), not by a "context" flag. Removing them also keeps "context is requested, not discovered"
  honest: there is no longer a special flag that silently lands text in the
  system prompt.

### 6. Plan mode (`--plan=on|off`)

- **What:** a persistent per-session flag that made every patch — including
  edits on git-tracked files that would otherwise be auto-applied — require
  explicit human approval.
- **Status:** removed.
- **Why not:** attini already gates the consequential writes; a separate mode
  added a second, redundant notion of "how much approval" a session needs and
  a persistent state file to keep in sync. The human is the final gate by
  default, and `attini ask` is the read-only way to inspect or query a
  session without touching it. There is no whitelisted "semi-approval" state
  to toggle into.

### 7. `--local-only` mode and rule attributes (`readonly` / `network`)

- **What:** a startup flag `--local-only` that switched the evaluator into a
  mode where commands matched by a `network: false` (attribute-only) rule
  were auto-approved, and the `readonly` / `network` rule attributes that
  existed only to feed that mode.
- **Status:** removed. The `Mode` enum, `evaluate`'s `mode` argument, and the
  `Rule.readonly` / `Rule.network` fields are gone.
- **Why not:** it added a second approval axis (a per-invocation "how much
  should run unattended" knob) on top of the per-rule `decision`. Managing
  it meant the human had to reason about modes *and* rules, and attribute-only
  rules that carry no `decision` were a confusing middle state. Approval is
  now a single, flat thing: a rule either says `approve`, `deny`, or is absent
  (which falls through to a pending approval). Rules carry no attributes that
  silently change behavior under some mode.

## The common thread

All are the same shape: **implicit, convention-based context or delegation
that attini cannot see or control.** attini's answer to each is "be
explicit" — put it in the prompt, or run the other session yourself.
"Unsupported" here means "you ask for it explicitly, and then it works
exactly the way you asked", not "the feature is missing and should be
added". (Item 5 is the degenerate case: even an *explicit* flag was
unnecessary, because the prompt already does the same thing.)

## Why this is not just "the same as the design philosophy"

The README's design philosophy bullets describe *principles* (context is
requested, state changes surfaced, no silent side effects). This note is a
*concrete list of removals and non-goals*. It answers a different question:
"if you look for feature X, is it gone because it was bad, or just not built
yet?" The answer for these seven is "bad, deliberately".

## How to revive

Do not re-add any of these by default. If a future need genuinely appears,
the bar is: the mechanism must be **opt-in and explicit**, and it must be
justifiable as an *amplifier* of what the model already does, not a
replacement for it. If such a mechanism is ever seriously proposed, this
document is the place to argue against it from recorded reasons.

This note is deferred because the removal commits already made the decision;
the remaining question is whether to ever *assert* the list publicly (for
example in the README) and in what form.
