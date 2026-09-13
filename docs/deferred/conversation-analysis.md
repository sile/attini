# Interpretation skill for conversation analysis

**Status:** Deferred. The deterministic `attini session analyze` subcommand
(measurement half) is implemented and committed; the interpretation skill
is the remaining, unbuilt half. See `docs/design/conversation-analysis.md`
for the implemented measurement half.

## Deferred half: `-S analyze-conversation` skill

A `SKILL.md` (loaded via the existing explicit `attini tell -S PATH`)
instructing the model how to:

- Run `attini session analyze <NAME>` (and pass `--json` if machine parsing
  helps).
- Read the histogram and identify which record kind / tool result dominates.
- Explain the finding in one or two sentences (e.g. "`cargo test` is 1.40 MB
  across 69 calls; reasoning is 4.29 MB and is still being re-sent on
  resume").
- If something looks unusual, drill into the specific offender with `read`
  / `command` — for example, inspect the biggest single `read` offset or the
  largest `command` stdout, to decide whether the fix is a tool change
  (output cap, reasoning drop) or a workflow change (split the session).
- Report a recommendation rather than a restatement of the numbers.

## Why deferred

The measurement subcommand already exists, so the skill would only need to
chain it with model judgment. It is deferred because the deterministic part
(raw numbers) is the harder half and is done; the interpretation skill is
ergonomic, not a correctness gap.

## How to revive

Add the `analyze-conversation` skill (a `SKILL.md` loaded via the existing
explicit `attini tell -S PATH`). It only needs to chain `session analyze`
output with model judgment.
