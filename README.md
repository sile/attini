attini
======

A CLI coding agent that aims to be as autonomous as it can be, without ever
leaving your control.

attini lives in your workspace and works on its own: it lists and reads files,
searches the codebase, applies patches, and runs commands. Safe, reversible
changes are applied without a prompt; anything consequential — a new file, a
change outside git, a command that isn't explicitly allowed — is shown as a
preview and waits for your approval.

> This README is a work-in-progress draft. Keep it in sync with the implementation.

## Overview

- Talks directly to the DeepSeek Chat Completions API over HTTP/1.1 + TLS (streaming)
- Built on a Sans I/O design: parsers, data types, and state machines live in the
  core without performing I/O, while async transport, TUI, and filesystem
  integration live in surrounding modules
- The agent can use read-only tools (`list`, `read`, `search`) and a `patch` tool
  (add / update); reversible changes to tracked files are applied directly, while
  consequential writes (new files, non-tracked changes) require a preview and
  approval
- Enforced constraints include workspace-boundary checks, file-size limits, and
  search-result limits

## Design philosophy

attini is a **semi-autonomous coding agent that values controllability and
understandability over autonomy**. It is deliberately explicit and
conservative: it prefers that the human tells it exactly what to do, rather
than letting the agent infer or guess:

- **Context is requested, not discovered.** The agent does not go looking for
  context: files are loaded only when you name them (`--reference PATH`,
  `--skill PATH`), and no tool lets the model pull in skill or memory content
  at runtime. The only context injected without being named is the
  `# Workspace context` block, a fixed, always-on part of every session's base
  prompt that is documented and predictable rather than discovered on demand.
- **State changes are surfaced.** `patch` shows a preview (SHA-256 hashes + a
  diff summary) and waits for approval on any non-tracked write; the model's
  in-flight intent is observable via `attini ask` / `session show`; and a
  one-line status is printed to stderr so you always know which session/model
  is advancing. Diagnostic output never pollutes stdout.
- **No silent side effects.** The workspace boundary is enforced on every read
  and write, destructive operations require explicit confirmation, and a
  non-tracked file never silently overwrites a tracked one. Where a behaviour
  is too risky to do safely, attini refuses rather than guesses — for example
  rejecting multiple edits to the same path in one `patch`, or refusing to
  write `.attini/*/memories.md`.
- **Human-edited state, model-extended space.** Persistent context such as
  memories is written by humans only. The model can work freely in its own
  scratchpad, but every write there still passes through approval.

This is why, for instance, skills take an explicit `--skill PATH` instead of
being auto-discovered from `~/.attini/skills` or `.attini/skills` — context
should enter a session only because the human asked for it.

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
attini agent [--reference PATH ...] [--skill PATH] [--read-path PATH ...] [--max-tokens N] [--stdin] "<PROMPT>"
```

`--reference PATH` / `-r PATH` (repeatable) inlines the contents of an arbitrary
UTF-8 file into the system prompt before the first turn, so context is present
without a `read` round-trip. Relative paths resolve against the workspace root.
Files larger than 32 KiB are not inlined; instead they are granted as extra read
roots and referenced by absolute path (readable with the `read` tool).

`--stdin` reads standard input (until EOF) and appends it to the prompt as a
clearly marked `--- stdin ---` block, so small pasted fragments need no temp
file. It errors when stdin is a terminal (it would block), caps input at 1 MiB,
and warns when stdin is empty. It is meant for *data*, not background context —
use `--reference PATH` for that. `--stdin` cannot be combined with `--approve`
or `--reject`.

Extra positional tokens are now rejected as a usage error (`attini agent hello
world` fails instead of silently dropping `world`), so multi-word prompts must
be quoted or passed via `--stdin`.

`--max-tokens N` caps the completion-token budget for every model call in the
run. When omitted the model's own default is used.

`--skill PATH` / `-S PATH` loads a skill: either a directory containing `SKILL.md`,
or a `SKILL.md` file directly. Its body is prepended to the system prompt before
the first turn. Relative paths resolve against the workspace root. There is **no
implicit skill discovery** — attini never scans `~/.attini/skills` or
`.attini/skills`, and the model has no `skill_load` tool. Context enters only
because you asked for it, explicitly, at invocation start. `--skill` cannot be
combined with `--approve` or `--reject`.

The system prompt also includes a `# Workspace context` block that names the
current directory (the workspace root), the enclosing git repository (when
present), the current branch, and whether the root is a linked git worktree — so
the model is aware of which repo/branch it is editing even before the first turn.

When an `attini agent` invocation starts, a one-line diagnostic is printed to
stderr (never stdout, so streamed content and `| jq`/redirects stay clean):

```
[agent] model=deepseek-v4-flash session=main ctx=20736
```

`model=`/`session=` show which session/model is about to advance, and `ctx=` is
the **current** conversation size (the last recorded `prompt_tokens`, not the
cumulative billed total) — so you can see how close the session is to compaction
before it runs. `ATTINI_STATUS_LINE=0` disables the
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
