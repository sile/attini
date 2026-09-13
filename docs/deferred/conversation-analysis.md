# Interpretation guidance for conversation analysis

**Status:** Deferred. The deterministic `attini analyze` subcommand
(measurement half) is implemented and committed; interpretation guidance
is the remaining, unbuilt half. See `docs/design/conversation-analysis.md`
for the implemented measurement half.

> Note: this was originally framed as a `-S analyze-conversation` skill, but
> the `--skill` flag has since been removed. There is no skill-loading
> mechanism now; interpretation guidance can only be supplied by typing it
> into the prompt (or pasting it via `--stdin`).

## Deferred half: interpretation instructions

A short instruction block (typed into the prompt, since `--skill` is gone)
instructing the model how to:

- Run `attini analyze <NAME>` (and pass `--json` if machine parsing
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

The measurement subcommand already exists, so the guidance would only need
to chain it with model judgment. It is deferred because the deterministic
part (raw numbers) is the harder half and is done; the interpretation side
is ergonomic, not a correctness gap.

## How to revive

Write the interpretation instructions and pass them in the prompt when you
want an analysis (there is no skill flag anymore). It only needs to chain
`attini analyze` output with model judgment.
