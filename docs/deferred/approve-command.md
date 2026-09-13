# Dedicated `attini approve` command and one-shot `--grant` (deferred)

**Status:** Deferred. Not implemented. Records the case for replacing the `--approve` flag
with a dedicated subcommand, and for adding a `--grant SCOPE` sugar to reduce grant
friction.

## The observation

Approving a pending tool call is one of the two ways to advance an `attini agent`
session, yet it is expressed as a *flag* on `agent`:

```sh
attini agent --approve
```

Two problems:

1. **Typos are silent.** `--approve` is easy to mistype (`--aprove`, `--approve=` ,
   `--approve ` with a stray space/arg). With the current `noargs` setup, a mistyped flag
   is not a hard error — it is simply not recognized, so the invocation falls back to the
   fresh-prompt path (`PROMPT is required unless --approve is given` or, worse, treats the
   leftover token as a prompt). The human thinks they approved; nothing was approved.
2. **It is conceptually a command, not a modifier.** "Approve the pending call" is a
   self-contained action on a session, in the same family as `attini session show` /
   `attini ask`. Modelling it as a subcommand gives it its own `--help`, its own
   name in the top-level command list, and shell-completion/tab-completion ergonomics.

## Proposed shape

Promote approval to a first-class subcommand:

```sh
attini approve [-s NAME]        # approve the session's pending tool call(s)
```

- Symmetric with the rest of the CLI: the session is named with the same `-s` /
  `ATTINI_SESSION_NAME` fallback used everywhere else.
- It is a *separate token*, so a typo (`attini aproove`) fails as an unknown subcommand
  instead of silently doing something else.
- Keeps `agent` for "advance with a new prompt" only, which sharpens both commands.

### Interaction with `agent`

- `attini agent --approve` would be removed (or kept as a deprecated alias for one
  release). The `Continuation::Approve` path in `src/agent_cli.rs` stays — the new
  subcommand just constructs it, exactly as `--approve` does today (see
  `try_run_agent` in `src/main.rs`).
- `--approve` currently forbids combining with `--skill` and forbids a prompt.
  A dedicated command makes those rules structural instead of validated: `approve` has
  no `--skill` and no positional, so there is nothing to reject.
- Consider a matching `attini reject` for symmetry even though `--reject` was removed
  (a fresh prompt already covers "reject + redirect"; see `docs/deferred/reject-flag.md`
  for why `--reject` is redundant). Reject-by-flag is gone, but a read-only
  `approve` command does not force a `reject` command to exist.

## Second idea: one-shot `--grant SCOPE` on approve

Today, avoiding a future approval means finding the argv-prefix and running a *separate*
`attini session grant ...` invocation. The agent even prints a suggestion for it
(`emit_suggested_rule` in `src/agent_cli.rs`):

```
[command] approval required: cargo test
suggested rule (persist separately after approve):
  attini session grant cargo test                # session-local
  attini session grant cargo test --workspace    # workspace-wide
```

That is two commands and a copy-paste. Fold the grant into the approval:

```sh
attini approve --grant <SCOPE>
```

where `SCOPE` is one of:

| `SCOPE` | Meaning | Equivalent to |
|---|---|---|
| `oneshot` | approve this call only, persist nothing | default (current `--approve`) |
| `session` | approve and append the argv-prefix rule to the session `permissions.json` | `approve` + `attini session grant <prefix>` |
| `workspace` | approve and append to the workspace `permissions.json` | `approve` + `attini session grant <prefix> --workspace` |

Details to settle:

- **Which prefix gets granted.** Reuse the same truncation `emit_suggested_rule` uses
  (first two argv elements, e.g. `cargo test`) so `--grant session` does not bake in
  every flag. The human should see the exact prefix that will be written.
- **Applies to commands only.** Patches have no argv-prefix; a patch approval cannot be
  auto-granted this way (it is git-tracked-only auto-approve, or `--plan=on` to gate all
  patches). `--grant` on a pending patch should be an error, not a silent no-op.
- **Multiple pending calls.** `pending.json` is an array and `--approve` approves all of
  them. If several commands are pending, "which argv-prefix to grant?" is ambiguous —
  either grant each pending command's prefix, or require exactly one pending command
  when `--grant` is non-`oneshot`.
- **Reuse the existing machinery.** `permissions::grant(scope, argv_prefix)` and
  `GrantScope::{Session, Workspace}` already exist (`src/permissions.rs`); the command
  just calls them after a successful approve, inheriting the existing
  `AlreadyGranted` / `ExistingDenyConflict` / `ExistingAttributeOnlyConflict` handling.
- **Semantics of "approve then grant".** The approval is this-call; the grant is
  future-calls. They should be independent: if the grant fails (deny conflict), the
  approval should still stand, and the error should be reported after the call ran.

## Why it is only *deferred*

1. **A dedicated subcommand is a small UX win but a visible CLI change.** It adds a
   top-level command, changes help output, and needs a migration note for anyone using
   `--approve` in scripts. The typo-safety benefit is real but modest.
2. **`--grant` has unresolved corners** (multi-pending ambiguity, patch vs. command,
   approving-all while granting-one). Each is solvable, but the design should be settled
   deliberately rather than smuggled in.
3. **No observed pain yet.** The current `--approve` + printed `attini session grant ...`
   suggestion works; the friction is an ergonomic annoyance, not a bug. Per attini's
   philosophy (add surface only when the need is demonstrated), this can wait until the
   two-command shuffle is actually felt.

## If revived (implementation sketch)

1. `src/main.rs`: add `try_run_approve` (parse `-s/--session`, `--grant <SCOPE>`, reject
   unknown scopes), dispatch it before/alongside `agent`. Build `Continuation::Approve`
   and call `agent_cli::run` exactly as `try_run_agent` does today.
2. `src/agent_cli.rs`: after the `Continuation::Approve` branch completes, if a grant was
   requested, read the approved command's argv from the pending record and call
   `permissions::grant`.
3. Decide `--approve` fate: remove it, or keep a thin deprecated alias that prints a
   pointer to `attini approve`.
4. Update `README.md` (usage lines, the approve paragraph, the grant paragraph) and the
   `emit_suggested_rule` output to mention the shorter `attini approve --grant session`.
5. Tests: unknown-subcommand typo fails; `--grant oneshot` persists nothing;
   `--grant session` appends the truncated prefix; `--grant` on a pending patch errors;
   multi-pending behavior matches the chosen policy.

## Alternative: keep the flag, add the sugar

If a new top-level command feels like too much surface, the minimal half is to keep
`attini agent --approve` and only add `--grant SCOPE` to it. That removes the
copy-paste of the `attini session grant` suggestion while leaving the typo-safety and
conceptual-cleanliness arguments unaddressed.
