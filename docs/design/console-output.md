# Console output for tool calls

**Status:** Design. Partly implemented: patch previews and read head previews exist, and step 1 (human-readable error lines, no `{:?}`) is done. The unified per-tool before/after summary is still proposed here.

attini streams a short human-readable trace of what the model is doing to stderr as it runs. This document records what that trace shows, the style it follows, and the consistency gaps that remain.

## Principle

Output only on stderr. The model-facing tool result (JSON) is separate and is never printed; the stderr trace is for the human watching the invocation.

Each built-in tool should read like an external `command` does: a short line saying **what it will do**, then a short line saying **what it did**, with detail underneath only when the detail is bounded. An external command is already the reference point — `git status`, `cargo test` print their own output in their own established style, and attini simply lets that through. Built-in tools do not have a tool behind them, so attini writes the summary itself, and that summary should follow the same shape.

## Before and after

Both an **before** line and an **after** line are emitted:

- Before-only is not enough: once the result scrolls in, a line about what is happening can no longer be tied to the output it produced.
- After-only is not enough either: the human cannot tell what is being attempted while a slow tool runs.

So every built-in tool prints a one-line **before** summary (what it will do) and a one-line **after** summary (what it did), and the after line carries the detail.

## Detail is kept only when it is known to be small

A full dump is noise; a one-line summary loses too much. The rule is per tool, based on whether the output size is bounded:

| Tool | Before | After | Detail |
|---|---|---|---|
| `list` | `[list] src/ recursive` | `[list] 23 entries` | full list (small by nature) |
| `read` | `[read] src/x.rs lines 10..20` | `[read] 11 lines` | head preview, capped (already implemented) |
| `search` | `[search] "foo" under src/` | `[search] 7 hits in 3 files` | hit list, capped (can grow) |
| `patch` (auto) | `[patch] 3 edits across 2 files` | `[patch] applied: 2 files, +45/-3` | diff capped |
| `patch` (approval) | `[patch] approval required` | (after approval) result line | full diff for review |
| `command` | `[command] cargo test` | external output | none: the command prints its own result |

`list` and `read` reports are kept in full because they are bounded (a directory listing, one file's head); `search` and `patch` diffs are capped because they can grow without limit. This matches the existing `PATCH_PREVIEW_MAX_LINES` and `READ_PREVIEW_MAX_LINES` caps.

## Errors are human-readable, never `{:?}`

**Implemented.** The stderr trace and the model-facing error JSON share one source of truth:

- `ToolExecutionError::message()` returns the human-readable string; `to_json_string()` embeds the same string as its `message` field. Both are built from one private `code_and_message()` match, so they cannot diverge.
- The command/patch parse, preview, and apply error lines now print `[command] parse err: <message>` / `[patch] preview err: <message>` / `[patch] apply err: <message>`, where `<message>` comes from `ToolExecutionError::message()` or `PatchError::to_code_and_message()`. The `short_err` helper (which merely truncated a `Debug` dump at the first space or parenthesis) is gone.
- The model-facing payloads that previously embedded `{e:?}` now carry the same readable message with a stable error code.

Rule: every tool error line is `[<tool>] <phase> err: <message>`, where `<message>` is the same string the model receives.

## What is implemented today

- `patch`: approval preview with per-edit `-`/`+` lines, capped; auto-approve prints the diff too; an approval footer restates the counts after a long diff.
- `read`: after-summary prints `[read "..."] ok` plus a capped head preview (`READ_PREVIEW_MAX_LINES`).
- `command`: an approval preview; otherwise the command's own stdout/stderr.

## What is proposed (not yet implemented)

- A one-line **before** summary for every built-in tool (`read`/`list`/`search`/`patch`), printed before the tool runs.
- A unified **after** summary shape (`[<tool>] <what it did> (<count>)`) across tools, replacing the current mixed shapes (`[read ...] ok`, `[patch] auto-approved: N file(s) (via)`, etc.).
- Detail policy made explicit per tool (see table above), with `read`/`list` keeping full bounded output and `search`/`patch` capping.
- Resolving the patch approval triple-print (`[patch] approval required` + preview + footer) into a single coherent block.

## How to revive

The work is small and independent per tool. Suggested order:

1. ~~Error lines: switch the `{:?}` sites to `ToolExecutionError::message`/`PatchError::to_code_and_message` (low risk, no behaviour change).~~ Done.
2. After-summary shape: unify the built-in tools' one-line result summaries.
3. Before-summary: add the pre-execution line for each built-in tool.
4. Patch approval block: settle on one of summary-then-diff or diff-then-footer, not both.
