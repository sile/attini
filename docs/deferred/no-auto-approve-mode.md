# Plan mode (persistent patch guard)

**Status:** Proposed. Not implemented. This document records the design and how to build it.

## Problem

Normally `attini agent` auto-approves safe, git-tracked `Update` edits: they are
revertible, so no human prompt is shown. The user wants a mode where *every*
patch — including tracked-file edits — requires explicit human approval, so that
unstaged changes can be reviewed in a separate step or session.

`command` is intentionally out of scope for now; this mode is **patch-only**.

## Design

### Flag: `--plan=on|off` (persistent)

```sh
attini agent -s main --plan=on  "..."   # plan mode ON (persisted)
attini agent -s main "..."              # unspecified -> keep current session state
attini agent -s main --plan=off "..."   # plan mode OFF (persisted)
```

- Values are `on` / `off` only; anything else errors.
- Unspecified leaves the session's current state unchanged (first run defaults
  to off / normal mode).
- State is persisted under `.attini/<session>/`, so it survives across
  `attini agent` invocations.
- `--approve` / `--reject` remain usable inside plan mode; a human explicitly
  approving a pending call is always allowed (it is not a machine authorization
  bypass, it is the human gate itself).

Rationale for `--plan=on|off` over `--enter-plan` / `--leave-plan`: a single
flag keeps one concept at one knob, is short, symmetric, and matches the
project's existing value-taking options (`--session`, `--max-tokens`, ...).

### Effect (patch only)

The core change is in `dispatch_patch_unapproved`
(`src/agent_cli.rs:1669`):

```rust
if preview.auto_approve && !plan_mode { ... }
```

That single condition makes the auto-approve branch never fire when plan mode
is on, so every patch enters the `PatchDispatch::Awaiting` path and requires
human approval. `preview_patch` / `tools.rs` is left untouched — plan mode is a
session property, not an executor property, so it belongs in the caller.

- Adds (`Add`) and non-tracked `Update` already require approval; plan mode only
  additionally covers git-tracked `Update`, which is the only case that is
  currently silent.
- `command` auto-approval (from `permissions.json` approve rules) is **not**
  affected. Those are explicit human-written rules, and the user scoped this
  mode to patches only.
- Deny rules (`AutoDeny`) are unaffected and still hard-reject.

### Persistence

Add `plan_mode: bool` to `Session` (`src/session.rs`) and include it in
`save` / `load`. Display it in `attini session show`.

### UI (all three on stderr)

1. **Status line at invocation start:** append `plan=on` (or `plan=off`) to the
   existing `[agent] model=... session=... ctx=...` line. If `--plan` was
   specified this invocation but the value differs from the session state, show
   the change (e.g. `plan=on` after having been off).
2. **System prompt injection:** in `build_initial_messages`, when plan mode is
   on, add a short block telling the model that every patch requires explicit
   human approval and that it should not batch many edits assuming they will go
   through silently.
3. **Approval marker:** prefix the approval message with `(plan)` so a human
   sees at a glance that the prompt is due to plan mode, not a normal
   tracked-file auto-approve being blocked.

## Files to touch

- `src/agent_cli.rs` — `dispatch_patch_unapproved` condition; status line;
  `build_initial_messages` injection.
- `src/session.rs` — `Session.plan_mode` field; save/load; `session show`.
- `src/main.rs` — `--plan=on|off` option parsing + validation.
- `tests/` — plan-mode patch flow tests.
- `README.md` — usage line + plan mode paragraph.

## Verification notes

- Will not touch `command` auto-approval; verify that a permissions approve rule
  still auto-runs when plan mode is on.
- Verify `--plan=on` then `--plan=off` round-trips the persisted state.
- Verify a tracked-file `Update` requires approval under plan mode, and that
  approval → apply works (patch text is unchanged; only the auto/await branch
  is gated).
