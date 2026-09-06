# `try_auto_compact` record-based trigger (deferred)

**Status:** Deferred. Not implemented. This document records the remaining
trigger hole in automatic compaction, why it is not urgent, and how to revive it.

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

## How to revive

* Add a size-based guard next to the token threshold, e.g. compute the total
  byte length of `load_records_since_last_summary()` (or of the retained tail)
  and if it exceeds a budget, force compaction even when
  `latest_prompt_tokens() < COMPACTION_TRIGGER_TOKENS`.
* Or record an actual history-size indicator alongside `token_usage` so
  `latest_prompt_tokens` reflects the real history even when the turn did not
  complete.
* Guard against over-eager compaction: fire at most once per resume, and only
  on the first turn of a resumed session.

## Decision

**Deferred.** The trigger hole is real but requires a new heuristic with
repeated-compaction risk. It was not the cause of the reported failure and has
not been observed since. Revisit only if a session is found to stay locked in
the overflow state with `latest_prompt_tokens` below the threshold.
