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
  `--skill PATH`), and no tool lets the model pull in skill content at runtime.
  Nothing is added to the system prompt unless you named it.
- **State changes are surfaced.** `patch` shows a preview (SHA-256 hashes + a
  diff summary) and waits for approval on any non-tracked write; the model's
  in-flight intent is observable via `attini ask` / `session show`; and a
  one-line status is printed to stderr so you always know which session/model
  is advancing. Diagnostic output never pollutes stdout.
- **No silent side effects.** The workspace boundary is enforced on every read
  and write, destructive operations require explicit confirmation, and a
  non-tracked file never silently overwrites a tracked one. Where a behaviour
  is too risky to do safely, attini refuses rather than guesses — for example
  rejecting multiple edits to the same path in one `patch`.
- **The model's own space is gated too.** The model can work freely in its own
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

### Agent CLI (`attini agent`)

```sh
attini agent [--reference PATH ...] [--skill PATH] [--read-path PATH ...] [--max-tokens N] [--temperature N] [--plan=on|off] [--thinking-effort none|low|high|max] [--stdin] "<PROMPT>"
```

`--reference PATH` / `-r PATH` (repeatable) inlines the contents of an arbitrary
UTF-8 file into the system prompt before the first turn, so context is present
without a `read` round-trip. Relative paths resolve against the workspace root.
Files larger than 32 KiB are not inlined; instead they are granted as extra read
roots and referenced by absolute path (readable with the `read` tool).

`--stdin` reads standard input (until EOF) and appends it to the prompt as a
clearly marked `--- stdin ---` block, so small pasted fragments need no temp
file. When stdin is a terminal it prints a note and reads interactively until
EOF (Ctrl+D); Ctrl+C cancels. It caps input at 1 MiB and warns when stdin is
empty. It is meant for *data*, not background context — use `--reference PATH`
for that. `--stdin` cannot be combined with `--approve` or `--reject`.

Extra positional tokens are now rejected as a usage error (`attini agent hello
world` fails instead of silently dropping `world`), so multi-word prompts must
be quoted or passed via `--stdin`.

`--max-tokens N` caps the completion-token budget for every model call in the
run. When omitted the model's own default is used.

`--temperature N` / `-t N` sets the sampling temperature for model calls.
Default is 0 (deterministic), which DeepSeek recommends for coding/math; the
value is ignored while thinking mode is enabled. It can also be set via
`ATTINI_TEMPERATURE`.

`--skill PATH` / `-S PATH` loads a skill: either a directory containing `SKILL.md`,
or a `SKILL.md` file directly. Its body is prepended to the system prompt before
the first turn. Relative paths resolve against the workspace root. There is **no
implicit skill discovery** — attini never scans `~/.attini/skills` or
`.attini/skills`, and the model has no `skill_load` tool. Context enters only
because you asked for it, explicitly, at invocation start. `--skill` cannot be
combined with `--approve` or `--reject`.

`--plan=on` / `--plan=off` turns plan mode on or off **persistently** for the
session. In plan mode every patch — including edits on git-tracked files, which
would otherwise be auto-applied — requires explicit human approval before it
touches the workspace. Commands are unchanged (deny rules still hard-reject),
and `--approve` / `--reject` still work. The flag persists in
`.attini/<SESSION>/plan_mode` and is reflected in `attini session show`; omitting
it leaves the session's current plan-mode state unchanged.

`--thinking-effort none|low|high|max` sets the session's DeepSeek thinking-mode
effort **persistently** (default `high`, i.e. chain-of-thought on). `none`
disables thinking so no `reasoning_content` is produced and no reasoning is
re-sent on tool-bearing requests; `low`/`high`/`max` enable chain-of-thought at
that depth. It persists in `.attini/<SESSION>/thinking_effort`, is reflected in
`attini session show` as `thinking: ...`, and is shown in the status line as
`thinking=...` when enabled. Omit the flag to leave the session's current value
unchanged.

When an `attini agent` invocation starts, a one-line diagnostic is printed to
stderr (never stdout, so streamed content and `| jq`/redirects stay clean):

```
[agent] model=deepseek-v4-flash session=main ctx=20736
```

`model=`/`session=` show which session/model is about to advance, `ctx=` is
the **current** conversation size (the last recorded `prompt_tokens`, not the
cumulative billed total), `plan=on` appears when plan mode is active, and
`thinking=low|high|max` appears when thinking mode is enabled — so you can see
how close the session is to compaction before it runs.
`ATTINI_STATUS_LINE=0` disables the line.

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

## Environment variables

| Variable | Description |
| --- | --- |
| `DEEPSEEK_API_KEY` | API key (required). Use any non-empty value for local servers that ignore auth. |
| `DEEPSEEK_BASE_URL` | OpenAI-compatible API base URL (optional). Default: `https://api.deepseek.com`. Trailing slash is stripped; `/chat/completions` is appended. Example: `http://host:8888/v1`. |
| `ATTINI_SESSION_NAME` | Default session name when `-s/--session` (or a positional `<SESSION>`) is omitted. Precedence: CLI flag, then this env var, then `main`. |
| `ATTINI_MODEL_NAME` | Default model name when `--model` is omitted. Precedence: CLI flag, then this env var, then the built-in default. |
| `ATTINI_MAX_TOKENS` | Default completion-token cap when `--max-tokens` is omitted. Precedence: CLI flag, then this env var, then the model's own default (no cap). |
| `ATTINI_TEMPERATURE` | Default sampling temperature when `--temperature` is omitted. Precedence: CLI flag, then this env var, then 0 (deterministic). Ignored while thinking mode is enabled. |
| `ATTINI_STATUS_LINE` | Set to `0` to suppress the one-line end-of-invocation status that `attini agent` prints to stderr. Unset (or any other value) keeps it on. |
