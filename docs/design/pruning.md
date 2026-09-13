# Automatic pruning of `conversation.jsonl`

**Status:** Implemented. This document records why attini has no manual `prune`
command and how the conversation log is bounded in size.

## Overview

`conversation.jsonl` is append-only. Compaction appends a `SessionRecord::Summary`
but never deletes the records it summarised, so the file grows without bound over a
long session. Pruning is the step that actually removes those older records.

There is deliberately **no `attini prune` command**. Pruning is housekeeping, not a
decision a human should have to remember to make, and a manual command would fight the
read-only inspection tools (`attini analyze` / `attini show`) that read the same log.
Instead, pruning runs automatically the next time the log has grown past a byte
threshold, as part of the same pass that compaction already performs.

## Trigger

`maybe_prune_conversation` (`src/tell_cli.rs`) runs at the top of
`try_auto_compact`, before the summarisation trigger is even evaluated — pruning is
independent of summarisation and fires purely on file size:

- When `conversation.jsonl` is **over `CONVERSATION_PRUNE_TRIGGER_BYTES`**
  (100 MB), the pass halves it.
- It runs only while the session `LOCK` is held (i.e. from the single writer that
ows the session), so the log is never rewritten concurrently.

100 MB is far larger than any session that still benefits from full history, so in
practice this fires rarely and silently; when it does it prints one line to stderr:

```
[prune] dropped N records, <orig> -> <new> bytes (file crossed 104857600 bytes)
```

## Where it cuts

`prune_offset_past_midpoint` scans the file to the first **safe boundary at or after
the byte midpoint** and drops everything before it, roughly halving the file. A safe
boundary (`line_is_safe_boundary`) is a `user` record, or an `assistant` record whose
text is non-empty or whose `tool_calls` are empty. Cutting there never separates an
`assistant -> tool` pair, mirroring the compaction cutoff's `is_safe_boundary` rule.

If no safe boundary exists at or after the midpoint (the tail is one unresolved
`assistant -> tool` pair), the file is left untouched for this pass rather than split
a pair. The rewrite itself is a tmp-file + rename (`rewrite_file_from_offset`), so a
crash mid-write cannot truncate the conversation.

## Interaction with inspection

Pruning removes history that `attini analyze` / `attini show` would otherwise read.
That is acceptable because it is automatic and infrequent (only past 100 MB), and
because the records it drops are already folded into a summary that inspection still
shows. Pruning never touches the newest summary or the retained tail.

## Related

- [compaction.md](compaction.md) — the summarisation pass pruning runs alongside.
- `docs/bug/conversation-log-bloat.md` — the original investigation into log growth.
