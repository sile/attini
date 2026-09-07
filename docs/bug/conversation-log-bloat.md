# Conversation-log bloat: investigation report

**Status:** Investigation record; part of the findings are being fixed now
(option C, below). Other parts are deferred or rejected.

## What this is

Record of an analysis of a very large session log
(`.attini/main/conversation.jsonl`, 12.6 MB / 8403 records). The goal was to
find why the log is so large and to judge whether turning external `command`
invocations into dedicated tools is worth it.

## Measured picture

| Category | Bytes | Records | Notes |
|---|---|---|---|
| assistant.reasoning | 4.29 MB | 1469 | **The model's chain-of-thought. Re-sent to the model on every restart.** |
| tool: read | 2.93 MB | 744 | Full file contents. `src/agent_cli.rs` alone is re-read 272 times (1.04 MB). |
| tool: command | 1.97 MB | 579 | `cargo test` is 1.40 MB across 69 calls. |
| tool: search | 0.70 MB | 811 | Search results. |
| token_usage | 0.34 MB | 1873 | |
| assistant.content | 0.21 MB | 1132 | Model's actual text output. |
| summary | 0.15 MB | 86 | |
| other | ~0.3 MB | | user / approval / invocation / metrics / patch. |

So the three leading causes are, in order: **(1) reasoning**, **(2) read**, and
**(3) command** (dominated by `cargo test`).

## Root cause 1: `reasoning_content` is re-sent to the model

`parse_conversation_line` (`src/session.rs`, around line 1341) restores an
assistant record with its `reasoning` field, and `src/sansio/deepseek.rs`
(around line 92) serialises that `reasoning` as `reasoning_content` in the API
request body.

That means the model's own chain-of-thought trace (4.29 MB in this session) is
**fed back into the model on every restart / every call** as input. It is
"private" continuation state that the model does not need as context — it is
the model's own prior reasoning, not user or repository content. The only
real use of `reasoning` is to *display* it (via `--show-reasoning`, on stderr);
it does not need to be re-sent.

This is the single largest source of bloat, and removing it is a pure win:
4.29 MB -> 0 with no loss of useful context.

## Root cause 2: repeated full-file `read`s

`read` accounts for 2.93 MB. The model re-reads the same sources repeatedly
with overlapping ranges; `src/agent_cli.rs` was read 272 times (1.04 MB,
max a single read of 119 KB). There is no file-content caching across turns in
the tool layer; every `read` returns the full requested file/range.

This is inherent to the agent loop and not easily fixed by "turning commands
into tools". Caching read contents is a separate, larger design change.

## Root cause 3: verbose `command` output, dominated by `cargo test`

Command breakdown by argv[0]:

| Command | Bytes | Calls |
|---|---|---|
| cargo test | 1.40 MB | 69 |
| git (status/diff/log/rev-parse) | 209 KB | 226 |
| bash -c | 140 KB | 62 |
| ./target/debug/attini (self-exec) | 93 KB | 59 |
| grep | 39 KB | 23 |

`cargo test` is the biggest single contributor: 28 KB per run, of which
~436 lines are the `test <name> ... ok` boilerplate. Only the final
`test result: ok. N passed; 0 failed` line carries signal — roughly **1% of the
output is useful**. Repeated across 69 calls this builds 1.4 MB of pure noise.

## The proposed "command -> tool" questions

Turning external commands into dedicated tools has different value per target:

* `run_tests` (structured pass/fail summary + only failing test names) would
  shrink 1.4 MB -> tens of KB. **Highest value**, but Rust-specific.
* git status/log/diff tools: moderate, structured and bounded.
* Native session viewer (replacing self-exec): small.

A general, non-language-specific alternative is a `command` `--summary` mode
that, on exit 0, keeps only the tail of stdout (where `test result: ...`
lives) plus failures, and on non-zero exit keeps the full output.

## Options

* **A. General `command` output summary mode** — keep only the tail of a
  successful command's stdout; full output on failure. Applies to all commands
  (`cargo`, `git`, `grep`) without adding tools. The dead
  `COMMAND_MAX_STREAM_BYTES` constant (see `docs/bug/command-max-stream-bytes.md`)
  should also be wired in as a cap.
* **B. Dedicated tools** (`run_tests`, `git_status`, ...) — structurally clearer
  but adds surface area, maintenance, and is Rust-specific. Only worth it if A
  proves insufficient.
* **C. Stop re-sending `reasoning`** (the highest-value, simplest fix) —
  persist `reasoning` for `--show-reasoning` but strip it when the conversation
  is rebuilt for the API.

## Decision

* **C first**: drop `reasoning_content` from the request body (keep it for
  display). Removes 4.29 MB immediately. This is being implemented now.
* **A next**: add `--summary`-style trimming to `command` output handling and
  wire in `COMMAND_MAX_STREAM_BYTES`. Removes the `cargo test` noise (1.4 MB).
* **B**: not recommended unless A is insufficient.
* **read re-read**: acknowledged, but a caching design is a larger separate
  effort; not part of this change.

## How to revive / verify

* After C, confirm the log still has `reasoning` (for display) but the outgoing
  API request body's assistant messages no longer carry `reasoning_content`.
* Measure the new `ask`/`agent` request size against the same history to see
  the reduction.
* After A, observe a fresh `cargo test` tool result to confirm it is reduced to
  the tail + failure details.
