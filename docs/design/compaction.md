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
`should_auto_compact(latest, total_chars, threshold)`:

```rust
fn should_auto_compact(latest: u64, total_chars: usize, threshold: u64) -> bool {
    latest >= threshold || total_chars > RECORDS_TOTAL_MAX_CHARS
}
```

- `latest` is the last successful turn's `prompt_tokens`
  (`session.latest_prompt_tokens()`), compared against `threshold`, the
  resolved trigger (default `COMPACTION_TRIGGER_TOKENS`, `16_384`).
- `total_chars` is the raw character size of the real records since the last
  summary, compared against `RECORDS_TOTAL_MAX_CHARS` (`250_000`). It is
  computed only when `latest < threshold` (so a normal-size
  history does not pay the extra scan cost).

### Tuning the trigger threshold

The token threshold is configurable per invocation. `try_auto_compact` takes
the resolved `threshold` as an argument, resolved once by
`resolve_compaction_trigger_tokens(&cfg)`:

- CLI flag `--compaction-trigger-kb N` (carried on
  `TellConfig::compaction_trigger_kb`) wins, and is validated at parse time
  (a non-integer is a usage error).
- otherwise the env var `ATTINI_COMPACTION_TRIGGER_TOKENS_KB`;
- otherwise the built-in default `COMPACTION_TRIGGER_TOKENS_KB` (`16`).

Values are in **kilobytes**, with `1 KB = 1024 tokens` (the same convention
that makes the default `64 × 1024 / 4 = 16_384`). So `--compaction-trigger-kb
8` means an 8192-token trigger, i.e. compaction fires earlier. A present but
unparseable env value is **ignored with a warning** rather than aborting
startup, since the env is not validated interactively; the CLI flag is strict
so a typo on the command line is a clear usage error. Only the `tell` entry
point sets the flag; `approve` leaves it unset and uses the default.

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

### Automated compaction stays quiet when it would fold nothing

Once the summaries have caught up to the present, the records since the last
summary are few (measured: four), so `compaction_cutoff` returns `None` —
there is genuinely nothing to fold. `latest_prompt_tokens`, however, still
reflects the *last full prompt*, which stays above `COMPACTION_TRIGGER_TOKENS`
as long as a fresh record keeps a prompt large. Without a guard, every turn
would therefore print `previous prompt was N tokens ... summarising...`
followed by `no records eligible for summarisation. skipping.` and do
nothing.

`try_auto_compact` now peeks `compaction_cutoff(&records, ...)` *before*
the log lines and returns silently when it is `None`. The trigger and the
fold decision are the same call, so the two can no longer disagree in the
log.

## Cumulative summary (only the newest is sent)

Each `summary` record is **cumulative**: it folds in the previous summary
along with the newly folded records, so it reads as one continuous summary
of the whole conversation (see the `prior` parameter of `run_summariser`, and
the `latest_summary` accessor).

`build_initial_messages` sends **only the newest summary** as a system
message. Earlier summaries are dead weight — the newest one already stands
for everything at or before its `cutoff_ts`. Sending every summary ever
written made the prompt grow without bound: on a mature session that had
accumulated 165 summaries, the summary block alone was ~342k chars (~86k
tokens), and every compaction pass *added* to it (each pass made the prompt
climb `124450 -> 125675 -> 126793` tokens across three passes). Dropping the
older summaries is what actually makes compaction shrink the prompt.

## Summary rendering

The summariser sends a **bounded prose transcript** rather than raw
`ChatMessage` records. `render_summary_transcript` collapses each record to a
short line, capping tool-call args (`90` chars), tool results (`200` chars),
and assistant/user text (`16k` chars each); the newest side is kept within
`SUMMARY_MAX_CHARS` (`200_000`), with a note when old parts are dropped.
This avoids feeding a huge `reasoning_content` or tool-result JSON back to
the model during compaction.

## Guards

- `try_auto_compact` is gated on the continuation **as originally
  requested**, before `drive` normalises an `Approve` into a
  continuation (`runs_pre_prompt_compaction`). It therefore fires only
  for a fresh `attini tell` prompt and never for `attini approve`.
  Relying on the `pending.json` check alone was not enough: a
  pending-free `approve` (the `max_turns` case) normalises into
  `Prompt(RESUME_PROMPT)` and would otherwise look identical to a
  `tell` by the time the gate runs, triggering a summariser call the
  human never asked for.
- After a successful fold, the newest summary stands for the older history
  and is the only summary sent, so the next prompt contains just that summary
  plus the retained tail — a second pass then sees a small total. If the
  trigger fires again but nothing is foldable, `try_auto_compact` bails
  silently (see "Automated compaction stays quiet", above).

## Compaction is automatic

The only compaction path is the size-triggered one on `attini tell`. A plain
`attini approve` never compacts: approving a pending tool call, resuming after
`max_turns`, or re-issuing a transport failure is one model call. There is no
separate manual compaction command — the token threshold is configurable (see
`--compaction-trigger-kb` / `ATTINI_COMPACTION_TRIGGER_TOKENS_KB`), which is
what a manual step used to be for.

The automatic path is size-triggered with no plan, and the log is additionally
bounded by automatic pruning, described in [pruning.md](pruning.md).

## The past is forgotten, not lost

Compaction only shortens what is sent to the model: the prompt keeps the
newest summary plus the retained tail, and older records stop being sent.
The records themselves are **not deleted** — they stay in
`.attini/<session>/conversation.jsonl` (pruning, which does drop them, runs
only past 100 MB; see [pruning.md](pruning.md)). So "forgetting" is a
per-call decision, not a destructive one: the model works from the summary,
while a human can still read the full history on disk.

Because the log is a plain, line-oriented text file, any tool that can read
a file can inspect it: `read` the raw `conversation.jsonl` directly, or run
`attini logstats <NAME>` for a deterministic breakdown. So a session that
has lost some earlier detail to compaction can simply read its own log back
— the records are still there — instead of relying on an export step or a
special API. That is the simple payoff of a text-file base: the transcript
the model has "forgotten" is still ordinary data a person (or another
session) can open.

## Related

- [pruning.md](pruning.md) — automatic physical pruning of the log (the one
  path that does discard older records).
- [conversation-analysis.md](conversation-analysis.md) — `attini logstats`,
  the deterministic reader for a session's log.
- `docs/bug/conversation-log-bloat.md` — the investigation that motivated the
  bounded prose summariser and the retained-tail cap.
