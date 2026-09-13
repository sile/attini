# Conversation checkpoint + rollback (deferred)

**Status:** Deferred. Not implemented. This document records the design exploration, the decision, and how to revive it.

## Problem

`attini ask` exists to let a human interrogate a session without dirtying the conversation log. It is read-only: it never appends to `conversation.jsonl`, and its own Q&A cache lives in `ask.json`. The stated motivation is **"I don't want to dirty the conversation log with questions."**

But that motivation is a symptom. The deeper need is **"I want to be able to try things without fear of permanently polluting the log"** — and the cleanest way to satisfy that is not to forbid writes, but to be able to *undo* them. A read-only `ask` side path is one narrow answer; a general **conversation checkpoint + rollback** primitive is the direct answer.

## Why the `ask` motivations are weaker than they look

1. **"Don't dirty the log"** — if you can roll back to before a turn, dirtying the log is no longer a reason to avoid writing. The write can be undone.

2. **"Ask without stopping a running session"** — attini sessions are short-lived and non-autonomous; stopping and restarting is trivial (Ctrl+C / a fresh `attini tell`). The concurrency benefit is negligible. This motivation does not justify a special read-only path on its own.

So the real gap is not "how do I ask without writing" but "how do I safely try things." `ask` is fine as a cheap read-only summariser, but it is not the fundamental fix.

## Proposed design (if revived)

### Checkpoint identity

Every record in `conversation.jsonl` already has a `ts` (millis). Options:

- **(a) Use `ts` as the checkpoint** — simplest, but `ts` is not guaranteed unique under rapid writes, and a compaction `Summary` record replaces a range of real records, so a pure `ts` boundary interacts with compaction in subtle ways.
- **(b) Add a monotonically increasing `seq` to every record** — a true linear point, robust to compaction (a `Summary` has a `seq` too). Cleaner, but touches the on-disk schema and all emitters/parsers.
- **(c) Use file line number** — cheap but not semantically meaningful once summaries/rewrites happen; truncation also invalidates it.

**Recommendation:** (b) `seq` for robustness; (a) is a quick MVP.

### Surface the checkpoint at agent startup

When `attini tell` opens a session, print (or record in `attini status`) the checkpoint at the tail — e.g. `checkpoint: seq=12341` or the `ts` of the last record. The human can then say "reset to seq=12341" if the following turns go wrong. This is the "output an identifier at startup, roll back to it" idea in its direct form.

### Rollback (reset)

Truncate `conversation.jsonl` to before the checkpoint and clean up suffix-dependent sidecar state:

- `pending.json` dropped if its `ts` > checkpoint.
- `ask.json` entries with `ts` > checkpoint dropped (or the whole file reset).
- Derived counters (metrics) recomputed or reset.
- Session-level settings (e.g. `ask.json` state) are separate from conversation
  state and are not part of a checkpoint/rollback.
- **Compaction edge case:** if the checkpoint is before the latest `Summary.cutoff_ts`, that Summary must be removed *and* its underlying real records restored — but they are already replaced. Either (i) refuse rollback into a summarised range, or (ii) archive the pre-summary records long-term.
- **Lock safety:** refuse to truncate while another process holds the `LOCK`.

### Branch / checkout

The safer default, more like `git checkout` than `git reset`: create a **new session** that copies the conversation up to the checkpoint (plus relevant state), leaving the original intact. E.g. `attini fork <SRC> --to <DST> --at <checkpoint>`. This avoids the compaction-truncation problem and loses nothing.

### Human CLI surface

- Extend `attini status` to print a `checkpoint:` line.
- New destructive `attini reset <SESSION> --at <checkpoint>` (confirmation prompt / `-y`).
- New non-destructive `attini fork <SRC> --to <DST> --at <checkpoint>`.

## Implementation cost

- `seq` field: schema + serialise/parse + every emitter + tests. Medium-high.
- Reset: truncate + recompute/clear sidecars + compaction guard + lock check. Medium.
- Fork: copy prefix to new dir + bootstrap session state. Medium.
- CLI surface + confirmation prompting. Low.

**Overall: medium-high**, with the compaction edge case being the genuinely hard part.

## Value vs. cost

**Value (high, conceptual):** turns the conversation log into a versioned artifact (git-like). It directly solves the original `ask` motivation (don't be afraid to dirty the log, because you can undo), and it generalises beyond questions to the agent's own turns — roll back a bad agent direction instead of living with it. Aligns with the project's emphasis on reversibility and controllability.

**Cost:** medium-high; the robust option touches the on-disk schema, and rollback is destructive (needs lock safety + confirmation).

**Zero-code alternative (available today):** manually copy `.attini/<NAME>/` (or just `conversation.jsonl`) before risky work, and restore from that copy. Crude but works with no code. The proposed feature is essentially automating this snapshot + restore.

## Decision

**Deferred.** The idea is compelling and consistent with attini's philosophy, but it is non-trivial. `ask` already covers the narrow read-only case, and a manual snapshot/restore is a workable fallback. Revisit if:

- a future long-running mode makes "I generated a lot of work I dislike" a frequent pain, or
- the manual snapshot/restore workflow is demonstrated to be too error-prone, or
- `ask` / read-only summarisation keeps being used as a workaround for the "don't dirty the log" fear (a signal that people want to safely experiment).

## How to revive

Start with the non-destructive half: surface a checkpoint (`ts` or `seq`) in `attini status` and at agent invocation start, and add `attini fork <SRC> --to <DST> --at <checkpoint>`. Fork copies the recorded prefix, so summarised history stays as-is and avoids the compaction-truncation problem.

Add the destructive `reset` only after fork experience shows demand, and only after solving the compaction edge case (either refuse rollback into a summarised range, or archive the pre-summary records).
