attini
======

A DeepSeek-powered coding agent prototype.

> This README is a work-in-progress draft. Keep it in sync with the implementation.

## Overview

- Talks directly to the DeepSeek Chat Completions API over HTTP/1.1 + TLS (streaming)
- Built on a Sans I/O design: parsers, data types, and state machines live in the
  core without performing I/O, while async transport, TUI, and filesystem
  integration live in surrounding modules
- The agent can use read-only tools (`list`, `read`, `search`) and a `patch` tool
  (add / update) that requires a preview and approval before applying changes
- Enforced constraints include workspace-boundary checks, file-size limits, and
  search-result limits

## Requirements

- Rust 1.93+ (`edition = 2024`)
- `DEEPSEEK_API_KEY` environment variable
- Optional `DEEPSEEK_BASE_URL` for OpenAI-compatible endpoints (local LLMs, etc.)

## Build / Test

```sh
cargo build
cargo test
```

## Usage

Set the environment variable first:

```sh
export DEEPSEEK_API_KEY=sk-...

# Optional: point at a local OpenAI-compatible server
# export DEEPSEEK_BASE_URL=http://127.0.0.1:8888/v1
# export DEEPSEEK_API_KEY=local
```

### Interactive TUI

```sh
attini tui [--model NAME] [--transcript PATH] [--metrics-snapshot-interval SECONDS]
```

Starts the agent with the current directory as the workspace.

`--transcript PATH` (optional) appends a JSON Lines session log to `PATH` for
later inspection with `jq`. The file is opened in append mode; each session
begins with a `session_start` record and ends with `session_end`. If the
file cannot be opened `attini tui` exits with a non-zero status.

`--metrics-snapshot-interval SECONDS` (optional, requires `--transcript`)
appends a `metrics_snapshot` record every `SECONDS` seconds, containing every
`AgentMetrics` counter as of that instant. Useful for tracking accumulator
trends over long sessions with `jq 'select(.kind=="metrics_snapshot")'`.

### One-shot chat

```sh
attini chat [--model NAME] [--system TEXT] [--show-reasoning] "<PROMPT>"
```

Streams the response to stdout. The tool loop is not run.

### Agent CLI (`attini agent`)

```sh
attini agent [--reference PATH ...] [--skill NAME] [--read-path PATH ...] [--max-tokens N] "<PROMPT>"
```

`--reference PATH` / `-r PATH` (repeatable) inlines the contents of an arbitrary
UTF-8 file into the system prompt before the first turn, so context is present
without a `read` round-trip. Relative paths resolve against the workspace root.
Files larger than 32 KiB are not inlined; instead they are granted as extra read
roots and referenced by absolute path (readable with the `read` tool).

`--max-tokens N` caps the completion-token budget for every model call in the
run. When omitted the model's own default is used.

The system prompt also includes a `# Workspace context` block that names the
current directory (the workspace root), the enclosing git repository (when
present), the current branch, and whether the root is a linked git worktree — so
the model is aware of which repo/branch it is editing even before the first turn.

When an `attini agent` invocation starts, a one-line diagnostic is printed to
stderr (never stdout, so streamed content and `| jq`/redirects stay clean):

```
[agent] model=deepseek-v4-flash session=main ctx=20736/65536
```

`model=`/`session=` show which session/model is about to advance, and `ctx=` is
the **current** conversation size (the last recorded `prompt_tokens`, not the
cumulative billed total) against the 64 K window — so you can see how close the
session is to compaction before it runs. `ATTINI_STATUS_LINE=0` disables the
line.

### Model selection

- Default model: `deepseek-v4-flash` (override with `--model`)

## Agent tools

| Tool | Description | Constraints |
| --- | --- | --- |
| `list` | List files and directories under a workspace-relative path | `max_entries` limit (default 200) |
| `read` | Read a UTF-8 text file | Up to 1 MiB; optional `line_range` |
| `search` | Literal substring search (no regex) | `max_results` limit (default 50) |
| `patch` | Batch of add / unique-replacement edits | Edits limited to git-tracked files are auto-applied; any add or non-tracked edit needs approval. `before` must match exactly once; workspace-boundary check |

`patch` first presents a preview (SHA-256 hashes + a diff summary) and is applied only
after approval.

For longer tasks the model may keep its own working notes under the session's
scratchpad directory (`.attini/{NAME}/scratchpad/`) using `patch`; those files are
not tracked by git and never appear in `git diff`. Because they are non-tracked,
`patch` writes there are still shown for approval (they are not auto-applied).

**Lifecycle:** scratchpad files are not auto-cleaned during a session — there is no
time- or size-based cleanup. They persist for the life of the session and are
removed only when the session is deleted with `attini session rm <NAME>`.

## Current ask (read-only)

`attini ask -s NAME [QUESTION]` asks the model to summarise the current state of a
session without touching the conversation log: what is in progress, any pending tool
call, and (when a `QUESTION` is supplied) a direct answer to that question. Records
since the last compaction summary are used by default; `--all` uses the whole
conversation and `--limit N` keeps only the most recent N records.
`--max-tokens N` caps the summariser response size. It is purely
observational — adjust the course by running `attini agent -s NAME "<new
instruction>"` (or a fresh session) and letting the model revise its approach
naturally.

Each `ask` caches the last two question/answer pairs in `.attini/<NAME>/ask.json`
and feeds them back (as a non-authoritative hint) on the next `ask`, so a
follow-up question can build on an earlier answer. The cache is keyed to the
records actually observed: changing `--all`/`--limit`, or the session advancing,
produces a different fingerprint and resets the cache (a short `(prior ask context
reset: ...)` note is printed).

```sh
attini ask -s main
attini ask -s main "What is the model currently working on?"
```

## Memories

`attini agent` prepends persistent memory from up to three tiers to every
invocation's system prompt:

| Tier | Path | Scope |
| --- | --- | --- |
| Global | `~/.attini/memories.md` | All sessions, all workspaces |
| Project | `.attini/memories.md` | All sessions in this workspace |
| Session | `.attini/{NAME}/memories.md` | One session |

Memories are **human-edited**. The model can read them (they are injected as
context) but has no tool to write them, and `patch` refuses `.attini/*/memories.md`
for safety. Create a file at any of the paths above by hand; the parent directory
is created automatically. On every `attini agent` invocation, all existing tiers
are read and concatenated into a single system message that is prepended before
any `--system` prompt or skill body. Missing files are silently skipped and empty
files are ignored.

**Timing:** memories are loaded **once at invocation start**, before the first
turn. Editing a memory file while a session is running has no effect on that
session — it is picked up on the next `attini agent` call.

## Environment variables

| Variable | Description |
| --- | --- |
| `DEEPSEEK_API_KEY` | API key (required). Use any non-empty value for local servers that ignore auth. |
| `DEEPSEEK_BASE_URL` | OpenAI-compatible API base URL (optional). Default: `https://api.deepseek.com`. Trailing slash is stripped; `/chat/completions` is appended. Example: `http://host:8888/v1`. |
| `ATTINI_SESSION_NAME` | Default session name when `-s/--session` (or a positional `<SESSION>`) is omitted. Precedence: CLI flag, then this env var, then `main`. |
| `ATTINI_MODEL_NAME` | Default model name when `--model` is omitted. Precedence: CLI flag, then this env var, then the built-in default. |
| `ATTINI_MAX_TOKENS` | Default completion-token cap when `--max-tokens` is omitted. Precedence: CLI flag, then this env var, then the model's own default (no cap). |
| `ATTINI_STATUS_LINE` | Set to `0` to suppress the one-line end-of-invocation status that `attini agent` prints to stderr. Unset (or any other value) keeps it on. |
