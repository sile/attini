# Conversation analysis tooling

**Status:** Implemented (measurement half). The deterministic `attini
analyze` subcommand is implemented and committed; the interpretation skill is
still deferred. This document records the design of the implemented half and
points at the deferred remaining half.

## Problem

Inspecting a session's `conversation.jsonl` for bloat and composition
currently requires ad-hoc shell work. The motivating case was
`.attini/main/conversation.jsonl`:

- **12.6 MB / 8403 lines.**
- **Record-kind distribution:** 2592 `tool`, 1871 `token_usage`, 1871
  `assistant`, 654 `tool_approval`, 398 `invocation_start`, 386
  `metrics_snapshot`, 86 `summary`.
- **Assistant payload split:** `reasoning` ≈ 4.29 MB across 1469 records
  (max 73,050 bytes); `content` only ≈ 0.21 MB across 1132 records.
- **Tool-result consumers:** `read` = 744 calls / ≈ 2.93 MB (max 119,018);
  `command` = 579 calls / ≈ 1.97 MB (max 43,703). Within `command`,
  `cargo test` = 69 calls / ≈ 1.40 MB.

Getting these numbers needed a chain of `grep -c` / `sort` / byte-counting
commands. That is:

- **Repetitive** — the same kind of inspection is needed whenever a session
  grows large or compaction behaves oddly.
- **Permission-gated** — exactly which helpers are allowed varies by machine
  and each step needs approval (`python3` was blocked here; `sort` was not).
- **Not reproducible** — the analysis lives in a conversation, not in
  checkable source, and can't be rerun with one command.

## What it solves

A single deterministic command that answers, for one session: which record
kinds dominate, which tool results are largest, which files are read
repeatedly, which external commands produce the most output, and how much of
the assistant payload is `reasoning` vs `content`.

## Implemented: `attini logstats <NAME>`

A deterministic Rust subcommand that reads `conversation.jsonl` (no LOCK
required, read-only) and prints:

- **Record-kind histogram** — count + total bytes per kind (`assistant`,
  `tool`, `token_usage`, `metrics_snapshot`, `summary`, ...).
- **Assistant payload split** — sum of `content` vs `reasoning` (bytes and
  records), including the max single record.
- **Tool-result consumers** — top `read` targets by bytes and by distinct
  ranges; top `command` families (program, subcommand) by bytes and call
  count; max single stdout.
- **Aggregate totals** — total bytes, record count, and (from `token_usage`)
  prompt / completion tokens.

Output is a human-readable table by default, with `--json` for a
machine-consumable baseline. It reuses `parse_conversation_line`
(`src/session.rs`); no new model call. The analysis is written to stdout
only, never appended to `conversation.jsonl`.

### Implementation notes

- Tool results are attributed by joining `assistant` records' `tool_calls[]`
  (id / function_name / arguments_json) with `tool` records' `call_id`.
- Command families are generalised to `(argv[0], argv[1])` pairs — no
  hardcoded `cargo`/`git` logic. A single-arg program is `(argv[0],)`.
- JSON `command_families` is emitted as a flat array of
  `{program, subcommand, count, bytes}` objects (not nested per program), so
  multiple subcommands under one program are all preserved.
- Human output uses top-10 per section; `--json` emits the full list.

## Deferred remaining half: interpretation guidance

See `docs/deferred/conversation-analysis.md` — guidance for interpreting
`attini logstats` output is still deferred. (Originally framed as a `-S` skill,
but `--skill` has since been removed, so it can only live in the prompt now.)
