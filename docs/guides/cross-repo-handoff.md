# Working across multiple repositories

**Status:** Guidance. There is no special feature here — this is how to use
the existing commands when a task spans more than one git repo.

## The situation

`attini tell` runs with a **single workspace per session**. A session in repo
B cannot see the context a session in repo A produced; the gap is *context
handoff across repos*. attini deliberately has no tool for this (see the
"Cross-repo handoff" entry in
[`docs/design/intentionally-not-supported.md`](../design/intentionally-not-supported.md)).
What follows is the zero-code recipe that covers the need.

## The recipe: scratchpad relay

1. **In repo A**, have the model write a relay note to its scratchpad, e.g.
   `patch` a file at `scratchpad/relay.md`. The scratchpad is non-tracked, so
   the write parks for one human approval; the note is a normal readable file
   afterwards.
2. **Carry it to repo B.** Two ways:
   - **Manual copy** — copy `.attini/<session>/scratchpad/relay.md` into repo
     B (or just paste its contents into the prompt).
   - **Grant read access** — add a `read` rule to repo **B**'s
     `.attini/permissions.jsonl` (or a session-scoped one):

     ```jsonl
     {"type":"read","path":"<repoA>/.attini/<session>/scratchpad/"}
     ```

     `read` rules are allow-only; the path matches recursively
     (segment-boundary aware). Then repo B's session can `read` the relay file
     directly.
3. **In repo B**, `read` the relay file (or just use the pasted text). Done.

## Why there is no dedicated feature

A first-class design was explored and set aside: a model tool that writes a
structured handoff into the target repo's `.attini/handoffs/`, auto-detected on
startup, with take-once semantics and a human `list`/`show`/`close` CLI.

It was not built because:

- The value is **primarily ergonomic** — a structured inbox, auto-detection,
  take semantics, an audit trail. The recipe above already delivers most of
  the benefit.
- The cost is real: it touches the **approval state machine** (a new
  approval-gated model tool, preview, `pending.json` persistence) and
  introduces a **cross-workspace write exception**, where human approval would
  be the only guard until an allowlist exists.

If cross-repo work later proves frequent and this recipe proves awkward, the
full design (model tools + startup scan + human CLI) is recorded in the git
history (the former `docs/deferred/cross-repo-handoff.md`) as a starting point. The
gate to revisit is the same as everywhere else in attini: the mechanism would
have to be **opt-in and explicit**, and an *amplifier* of what the model
already does, not a replacement for it.
