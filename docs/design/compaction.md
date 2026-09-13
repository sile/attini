# Conversation compaction

**Status:** Implemented and in use. This document records the design and
behavior of attini's conversation compaction: when it fires, what it keeps,
and how it avoids overflowing the context window.

## Overview

A long session eventually exceeds the model's context window. attini handles
this by summarising older conversation records into a compact summary and
keeping only a recent tail, so the next model call fits within the window.

Two independent pieces work together:

1. **Trigger** — decide *when* to compact (`try_auto_compact`).
2. **Cutoff** — decide *what* to keep vs. fold into the summary
   (`compaction_cutoff`), and how the summary is rendered
   (`render_summary_transcript`).

## Trigger: when compaction runs

`try_auto_compact` (`src/tell_cli.rs`) judges the need to compact from two
independent signals via the pure helper
`should_auto_compact(latest, total_chars)`:

```rust
fn should_auto_compact(latest: u64, total_chars: usize) -> bool {
    latest >= COMPACTION_TRIGGER_TOKENS || total_chars > RECORDS_TOTAL_MAX_CHARS
}
```

- `latest` is the last successful turn's `prompt_tokens`
  (`session.latest_prompt_tokens()`), compared against
  `COMPACTION_TRIGGER_TOKENS` (`16_384`).
- `total_chars` is the raw character size of the real records since the last
  summary, compared against `RECORDS_TOTAL_MAX_CHARS` (`250_000`). It is
  computed only when `latest < COMPACTION_TRIGGER_TOKENS` (so a normal-size
  history does not pay the extra scan cost).

### Why two signals (the stale-token hole)

`latest_prompt_tokens` (`src/session.rs`) scans `token_usage` records and
returns the **last successfully recorded** `prompt_tokens` value. That value
is absent or stale if the previous turn did not complete normally — for
example it was suspended awaiting approval right after a very large tool
result was appended. In that case the real records can be far larger than the
last recorded value implies, and the token-only trigger would early-return
without compacting.

The record-size signal closes that hole, at the cost of a lazy extra scan
only when the token signal is not already met.

## Cutoff: what is folded into the summary

`compaction_cutoff` (`src/tell_cli.rs`) chooses the oldest record to keep.
It walks from a safe boundary near the tail toward the start, folding more
into the summary while the **whole retained tail** exceeds the budget:

- `KEEP_RECENT_RECORDS_TARGET` (`10`) — keep at least this many recent
  records as raw history.
- `RETAINED_TAIL_MAX_CHARS` (`250_000`) — if the kept tail exceeds this
  budget, fold more into the summary.
- A **safe boundary** is a record that can be cut at without breaking a
  `assistant`/`tool` pair (the `safe_tail_start` helper avoids splitting a
  `tool_call` from its matching `tool` result).

The function no longer early-returns `None` when `records.len() <= target_keep`:
for a short history it starts at `safe_tail_start` (which returns `0`) and
walks forward while the whole history exceeds the budget, so a few records
dominated by one huge tool result are folded into a summary rather than
kept raw.

`None` is returned only when the retained tail stays within budget.

## Summary rendering

The summariser sends a **bounded prose transcript** rather than raw
`ChatMessage` records. `render_summary_transcript` collapses each record to a
short line, capping tool-call args (`90` chars), tool results (`200` chars),
and assistant/user text (`16k` chars each); the newest side is kept within
`SUMMARY_MAX_CHARS` (`200_000`), with a note when old parts are dropped.
This avoids feeding a huge `reasoning_content` or tool-result JSON back to
the model during compaction.

## Guards

- `try_auto_compact` returns early when `pending.json` exists, so it fires
  only on a fresh `Continuation::Prompt` (never on `--approve`).
- It fires on the first turn of a resumed session only; after compaction the
  summary replaces the huge history so a second pass sees a small total.
- `attini session compact` is the manual counterpart, using the same
  summariser path.

## Related

- `docs/bug/conversation-log-bloat.md` — the investigation that motivated the
  bounded prose summariser and the retained-tail cap.
- `docs/deferred/deepseek-reasoning-resend.md` — why `reasoning_content`
  resend is kept as a watch item.
