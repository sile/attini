# `try_auto_compact` record-based trigger

**Status:** Implemented. A second, record-size-based auto-compaction trigger
was added alongside the token threshold, closing the stale-`token_usage` hole.

## Problem

`try_auto_compact` (`src/agent_cli.rs`, around line 1002) decides whether to
summarise before the next invocation using **only** `session.latest_prompt_tokens()`:

```rust
let Some(latest) = session.latest_prompt_tokens()? else { return Ok(()); };
if latest < COMPACTION_TRIGGER_TOKENS {
    return Ok(());
}
```

`latest_prompt_tokens` (`src/session.rs`, around line 157) scans `token_usage`
records and returns the **last successfully recorded** `prompt_tokens` value.
That value exists only when a model turn completed and its usage row was appended.

**The gap:** if the previous turn did *not* complete normally — for example it
was suspended awaiting approval right after a very large tool result was
appended — the `token_usage` row may be absent or stale. The real conversation
records (which include that huge raw tool result) can be far larger than the
last recorded value implies. In that case `latest < COMPACTION_TRIGGER_TOKENS`
returns early and **no compaction runs**, even though the next main call reads
those real records and would overflow.

This is the same class of overflow the `render_summary_transcript` +
`compaction_cutoff` work fixed, but only when compaction actually fires. Here
compaction never fires because the trigger threshold is judged from stale data.

## Why deferred

* The observed failure (the "kyodai" session) was fixed by the prose-summariser
  + retained-tail cap, because in that case the last successful turn *did*
  record a large token count, so `try_auto_compact` fired and the new code
  handled it.
* The remaining hole is more pathological: the last record is stale/small while
  the history is huge. Fixing it requires a *record-size-based* trigger, which
  is a different heuristic and carries its own risk:
  * it could re-summarise on **every** resume;
  * it could trigger when the user intentionally kept a large context;
  * it needs a guard so it fires at most once per resume / on the first turn.
* Not observed in practice after the prior fix; deferred until it is.

## Implementation

`try_auto_compact` (`src/agent_cli.rs`) now judges the need to compact from two
independent signals via the pure helper `should_auto_compact(latest, total_chars)`:

```rust
fn should_auto_compact(latest: u64, total_chars: usize) -> bool {
    latest >= COMPACTION_TRIGGER_TOKENS || total_chars > RECORDS_TOTAL_MAX_CHARS
}
```

* `latest` is the last successful turn's `prompt_tokens` (unchanged).
* `total_chars` is the raw character size of the real records since the last
  summary, computed only when `latest < COMPACTION_TRIGGER_TOKENS` (so a
  normal-size history does not pay the extra scan cost).
* A new constant `RECORDS_TOTAL_MAX_CHARS = 250_000` mirrors
  `RETAINED_TAIL_MAX_CHARS`; exceeding it forces compaction even when the token
  count is stale/small.

`compaction_cutoff` was also relaxed: it no longer early-returns `None` when
`records.len() <= target_keep`. Instead it starts at `safe_tail_start` (which
returns `0` for a short history) and walks forward while the whole history
exceeds the budget, so a few records dominated by one huge tool result are
folded into a summary rather than dropped.

Guards:
* `try_auto_compact` still returns early when `pending.json` exists, so it fires
  only on a fresh `Continuation::Prompt` (never on `--approve`).
* It fires on the first turn of a resumed session only; after compaction the
  summary replaces the huge history so a second pass sees a small total.

## Decision

**Implemented** (see "Implementation" above). The record-size trigger closes
the stale-`token_usage` hole and, combined with the pre-existing
`compaction_cutoff` fold-all path, ensures a huge tool result appended just
before a suspend is summarised instead of overflowing the next main call.
