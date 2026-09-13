# Dedicated `attini approve` command and one-shot `--grant`

**Status:** Implemented. `attini approve` exists, the `attini agent --approve` flag was
removed, and `--grant oneshot|session|workspace` folds a persistent rule into the
approval. The sections below are the design record; see the git history / README for the
shipped form. The *rationale* (why a subcommand rather than a flag) is kept because it is
still worth reading before anyone proposes re-adding `--approve`.

The standalone `attini grant` / `attini grant-read` subcommands described below were later
**removed**: with the JSONL format, rules are plain lines edited by hand, and the only
write path is `attini approve --grant` (commands). `--grant` has no `read` scope, so read
rules are always added by hand; that asymmetry is recorded in
`docs/deferred/grant-read-scope.md`.

## The observation

Approving a pending tool call is one of the two ways to advance an `attini agent`
session, yet it is expressed as a *flag* on `agent`:

```sh
attini agent --approve
```

Two problems:

1. **Typos fall through to the prompt.** `--approve` is easy to mistype (`--aprove`,
   `--approve=`). With the current `noargs` setup a mistyped flag is not a hard error:
   because `agent` has an *optional positional* `<PROMPT>`, the unknown token is absorbed
   as the prompt instead of being rejected. Verified live:

   ```
   $ attini agent -s approv-check --approv
   [agent] model=deepseek-flash session=approv-check ctx=0
   ... the model runs a full investigation turn, treating "--approv" as the prompt ...
   ```

   So the typo does not merely fall back to *nothing* — it silently starts a **new model
   turn with the typo as the prompt**, spending tokens and running tools the human never
   asked for. (A second token surfaces only later, as an `unexpected argument` error:
   `attini agent -s s --approv hello` rejects `hello`, because `--approv` was consumed as
   the prompt first.) This is the strongest argument for the change: the confusing path
   only exists because `agent` must keep an optional `<PROMPT>`, and `--approve` rides
   along on that command.
2. **It is conceptually a command, not a modifier.** "Approve the pending call" is a
   self-contained action on a session, in the same family as `attini status` /
   `attini ask`. Modelling it as a subcommand gives it its own `--help`, its own
   name in the top-level command list, and shell-completion/tab-completion ergonomics.
   Decisively: an `approve` subcommand has **no positional**, so `attini approve --approv`
   is a clean unknown-flag error — the fall-through in (1) cannot happen there.

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

- **`attini agent --approve` is removed, not deprecated.** Keeping it would leave the
  fall-through in (1) reachable through the flag the change is meant to retire, so the
  two would contradict each other. The `Continuation::Approve` path in `src/tell_cli.rs`
  stays — the new subcommand just constructs it (see `try_run_approve` in `src/main.rs`).
- `agent` keeps its optional `<PROMPT>` (it is the normal "advance with a new prompt"
  path), so `attini agent --approv` still becomes a weird prompt. That is fine: once
  `--approve` is gone, no one expecting to approve reaches it, so the leftover token is
  just a mistyped prompt, not a mistaken approval.
- `--approve` forbids combining with a prompt. A dedicated command makes that rule
  structural instead of validated: `approve` has no positional, so there is nothing to
  reject. (The old `--skill` combination rule is moot: `--skill` itself has since been
  removed — context now enters only through the prompt or `--stdin`.)
- Consider a matching `attini reject` for symmetry even though `--reject` was removed
  (a fresh prompt already covers "reject + redirect"; see `docs/deferred/reject-flag.md`
  for why `--reject` is redundant). Reject-by-flag is gone, but a read-only
  `approve` command does not force a `reject` command to exist.

## Second idea: one-shot `--grant SCOPE` on approve

Today, avoiding a future approval means finding the argv-prefix and running a *separate*
`attini grant ...` invocation. The agent even prints a suggestion for it
(`emit_suggested_rule` in `src/tell_cli.rs`):

```
[command] approval required: cargo test
suggested rule (persist separately after approve):
  attini grant cargo test                # session-local
  attini grant cargo test --workspace    # workspace-wide
```

That is two commands and a copy-paste. Fold the grant into the approval:

```sh
attini approve --grant <SCOPE>
```

where `SCOPE` is one of:

| `SCOPE` | Meaning | Equivalent to |
|---|---|---|
| `oneshot` | approve this call only, persist nothing | default (current `--approve`) |
| `session` | approve and append the args-prefix rule to the session `permissions.jsonl` | `approve` + `attini grant <prefix>` |
| `workspace` | approve and append to the workspace `permissions.jsonl` | `approve` + `attini grant <prefix> --workspace` |

Settled semantics:

- **`approve` and `grant` are independent (option B).** Approving is a one-shot action
  on the current pending call; granting is a persistent state change. They are kept
  separate so each has a single, clear outcome. The grant runs *after* the approve; it
  is best-effort.
- **Grant failure is a warning, not a rollback.** If the grant cannot be written —
  `AlreadyGranted`, `ExistingDenyConflict`, `ExistingAttributeOnlyConflict`, or any I/O
  error — the approval still stands and the command still runs. The failure is reported
  as a one-line warning to stderr afterward. The human asked to approve; that succeeded;
  the convenience grant is a bonus that may or may not apply.
- **A grant that cannot be formed is an error.** If the pending call is not a `command`,
  or the argv cannot be truncated to a prefix at all, `--grant` is rejected up front
  (before approving) rather than silently degrading to a plain approve. Silent no-ops
  are against attini's philosophy.
- **Applies to commands only.** Patches have no argv-prefix; a patch approval cannot be
  auto-granted this way (patches are git-tracked-`Update`-only auto-approve, and
  everything else always needs approval). Introducing a patch scope is a separate,
  future idea — see the memo `docs/deferred/patch-grant-scope.md`.
- **Which prefix gets granted.** Reuse the same truncation `emit_suggested_rule` uses
  (first two argv elements, e.g. `cargo test`) so `--grant session` does not bake in
  every flag. The human should see the exact prefix that will be written, exactly as the
  existing suggestion does today (verified live: `ls -la .` suggests `attini
  grant ls -la`). Because the prefix comes from the same helper as the suggestion, the
  two can never disagree.
- **Multiple pending calls.** `pending.json` is an array and approve handles all of them.
  For `--grant`, if more than one command is pending, `--grant` is an error (ambiguous
  which prefix to persist); `--grant oneshot` is unaffected because it persists nothing.
  This keeps `--grant` a deliberate, single-target action.
- **Reuse the existing machinery.** `permissions::grant(scope, args_prefix)` and
  `GrantScope::{Session, Workspace}` already exist (`src/permissions.rs`); the command
  just calls them after a successful approve, inheriting the existing
  `AlreadyGranted` / `ExistingDenyConflict` handling.

## Why it is only *deferred*

1. **A dedicated subcommand is a small UX win but a visible CLI change.** It adds a
   top-level command, changes help output, and needs a migration note for anyone using
   `--approve` in scripts. The typo-safety benefit is real but modest.
2. **`--grant` has unresolved corners** (multi-pending ambiguity, patch vs. command,
   approving-all while granting-one). Each is solvable, but the design should be settled
   deliberately rather than smuggled in.
3. **No observed pain yet.** The current `--approve` + printed `attini grant ...`
   suggestion works; the friction is an ergonomic annoyance, not a bug. Per attini's
   philosophy (add surface only when the need is demonstrated), this can wait until the
   two-command shuffle is actually felt.

## If revived (implementation sketch)

1. `src/main.rs`: add `try_run_approve` (parse `-s/--session`, `--grant <SCOPE>`, reject
   unknown scopes), dispatch it before/alongside `agent`. Build `Continuation::Approve`
   and call `tell_cli::run` exactly as `try_run_tell` does today.
2. `src/main.rs`: remove the `--approve` flag from `try_run_tell` (and its prompt
   validation, which becomes unnecessary).
3. `src/tell_cli.rs`: after the `Continuation::Approve` branch completes, if a grant was
   requested, read the approved command's argv from the pending record, run it through
   the same truncation `emit_suggested_rule` uses, and call `permissions::grant`. Report
   a grant failure as a warning and keep the approval.
4. Update `README.md` (usage lines, the approve paragraph, the grant paragraph) and the
   `emit_suggested_rule` output to mention the shorter `attini approve --grant session`.
5. Tests: `attini approve --approv` fails as an unknown flag (no positional to absorb it);
   `--grant oneshot` persists nothing; `--grant session` appends the truncated prefix;
   `--grant` on a pending patch errors up front; `--grant` with multiple pending commands
   errors; a grant conflict warns but the approved command still ran.

## Alternative: keep the flag, add the sugar

If a new top-level command feels like too much surface, the minimal half is to keep
`attini agent --approve` and only add `--grant SCOPE` to it. That removes the
copy-paste of the `attini grant` suggestion while leaving the typo-safety and
conceptual-cleanliness arguments unaddressed.
