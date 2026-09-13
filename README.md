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
  context: nothing is added to the system prompt unless you put it there — the
  prompt itself, or `--stdin` for pasted data. No tool lets the model pull in
  extra context at runtime.
- **State changes are surfaced.** `patch` shows a preview (file list + a
  diff) and waits for approval on any non-tracked write; the model's
  in-flight intent is observable via `attini ask` / `attini show`; and a
  one-line status is printed to stderr so you always know which session/model
  is advancing. Diagnostic output never pollutes stdout.
- **No silent side effects.** The workspace boundary is enforced on every read
  and write, destructive operations require explicit confirmation, and a
  non-tracked file never silently overwrites a tracked one. Where a behaviour
  is too risky to do safely, attini refuses rather than guesses — for example
  rejecting multiple edits to the same path in one `patch`.
- **The model's own space is gated too.** The model can work freely in its own
  scratchpad, but every write there still passes through approval.

This is why, for instance, there is no automatic skill or instruction-file
discovery (no `~/.attini/skills`, no `.attini/skills`, no `AGENTS.md` scan) —
context should enter a session only because the human asked for it, in the
prompt.

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

### Tell CLI (`attini tell`)

```sh
attini tell [--system-prompt TEXT] [--max-tokens N] [--temperature N] [--stdin] "<PROMPT>"

attini approve [-s NAME] [--grant oneshot|session|workspace]
```

`--stdin` reads standard input (until EOF) and appends it to the prompt as a
clearly marked `--- stdin ---` block, so small pasted fragments need no temp
file. When stdin is a terminal it prints a note and reads interactively until
EOF (Ctrl+D); Ctrl+C cancels. It caps input at 1 MiB and warns when stdin is
empty.

Extra positional tokens are now rejected as a usage error (`attini tell hello
world` fails instead of silently dropping `world`), so multi-word prompts must
be quoted or passed via `--stdin`.

`--max-tokens N` caps the completion-token budget for every model call in the
run. When omitted the model's own default is used.

`--temperature N` / `-t N` sets the sampling temperature for model calls.
Default is 0 (deterministic), which DeepSeek recommends for coding/math. It
can also be set via `ATTINI_TEMPERATURE`.

`--system-prompt TEXT` prepends a system message to the conversation. It can
also be set via `ATTINI_SYSTEM_PROMPT`; precedence is CLI flag, then env var,
then none.

DeepSeek thinking mode is always **disabled**: attini sends
`{"thinking":{"type":"disabled"}}` on every request and offers no way to
enable chain-of-thought. The human is the final gate, so a private exploration
is mostly wasted, and it was the largest source of context bloat — see
`docs/design/thinking-mode.md`. With thinking off no `reasoning_content` is
produced or replayed, and `temperature` is always effective.

### Approving (`attini approve`)

```sh
attini approve [-s NAME] [--grant oneshot|session|workspace]
```

`approve` resumes a **stopped** session, which is the same human act — "yes, go
on" — however the stop happened:

- **Pending tool call** (the loop suspended for approval): the call is approved
  and executed, then the turn continues.
- **Transport failure** (a model call failed at the connection level — reset,
  timeout, DNS — before any assistant output was recorded): the *identical*
  request is re-issued. Nothing is appended, so a transient outage can be
  retried with the same command. (A definitive HTTP/API rejection is **not**
  offered this retry; only transport-level faults are.)
- **No pending call** (the loop hit `DEFAULT_MAX_TURNS`): a fixed continuation
  message is appended and the turn continues, so the session — its context and
  the model's understanding — carries over.

The message printed when the turn cap is reached names `approve` directly, so
the two paths stay connected:

```
tell loop exceeded max_turns=20; to continue this session run:
  `attini approve -s main` (or give a new instruction with `attini tell -s main "..."`)
```

If you actually want to change direction, use `attini tell` with a new prompt
instead. The exit code stays the generic `1`; attini does not assign a distinct
code to "hit the turn cap". If machine-readable distinction is ever needed, it
should be carried by a structured message rather than more exit codes.

Approving is a **dedicated subcommand**, not a `--approve` flag on `tell`. The
reason is typo safety: `tell` keeps an optional positional `<PROMPT>`, so a
mistyped flag like `attini tell --approv` is silently absorbed as the prompt and
starts an unintended model turn. `approve` has no positional, so `attini approve
--approv` fails cleanly as an unknown flag. (`attini agent --approve` was
removed, not deprecated.)

`--grant SCOPE` folds a persistent auto-approve rule into the approval, so you
do not have to copy-paste the `attini grant ...` suggestion afterward:

| `SCOPE` | Effect |
|---|---|
| `oneshot` | approve only, persist nothing (the default) |
| `session` | approve, then append the command's argv-prefix to the session `permissions.json` |
| `workspace` | approve, then append it to the workspace-wide `permissions.json` |

Approval and grant are independent: the approval always stands, and a grant that
cannot be written (already granted, a conflicting deny rule, or an I/O error) is
reported as a one-line warning rather than rolling the approval back. A grant
that cannot be *formed* — the pending call is not a command, its argv yields no
prefix, or several commands are pending — is rejected up front. The argv-prefix
is truncated the same way as the printed suggestion (first two elements, e.g.
`cargo test`), so the two never disagree.

When an `attini tell` invocation starts, a one-line diagnostic is printed to
stderr (never stdout, so streamed content and `| jq`/redirects stay clean):

```
[tell] model=deepseek-flash session=main ctx=20736
```

`model=`/`session=` show which session/model is about to advance, and `ctx=` is
the **current** conversation size (the last recorded `prompt_tokens`, not the
cumulative billed total) — so you can see how close the session is to
compaction before it runs. `ATTINI_STATUS_LINE=0` disables the line.

### Inspecting sessions

attini keeps no abstraction over session data. A session is a directory under
`.attini/<NAME>/` holding `conversation.jsonl` (append-only JSONL), `pending.json`
(when a turn stopped for approval), `ask.json`, a `scratchpad/`, and a `LOCK`.
You can list, read, or delete sessions with ordinary shell tools:

```sh
ls .attini/                       # list sessions
cat .attini/main/conversation.jsonl   # the raw record log
tail -f .attini/main/conversation.jsonl
rm -rf .attini/main/              # delete a session (also clears its scratchpad)
```

A few read-only helpers remain, for cases where parsing the log by hand is
tedious. None of them acquire the session `LOCK` or write to the conversation log:

```sh
attini show    -s NAME            # invocation/approval/message counts + pending calls
attini metrics [-s NAME | --all] [--json]
attini analyze -s NAME [--json]   # record-kind histogram, bytes by tool/command family
attini ask     -s NAME [QUESTION] # ask the model to summarise the current state
attini prune   -s NAME [-y]       # drop records before the last summary
```

Permissions live in plain `permissions.json` files and are edited by hand (or
appended by `attini grant` / `attini grant-read`):

```sh
attini grant      [-s NAME] [--workspace] <ARG0> [ARG]...   # auto-approve argv-prefix
attini grant-read [-s NAME] [--workspace] <PATH>            # workspace-external read root
```

### Model selection

- Default model: `deepseek-flash` (override with `--model`)

## Agent tools

| Tool | Description | Constraints |
| --- | --- | --- |
| `list` | List files and directories under a workspace-relative path | `max_entries` limit (default 200) |
| `read` | Read a UTF-8 text file | Up to 1 MiB; optional `line_range` |
| `search` | Literal substring search (no regex) | `max_results` limit (default 50) |
| `patch` | Batch of add / unique-replacement edits | Edits limited to git-tracked files are auto-applied; any add or non-tracked edit needs approval. `before` must match exactly once; workspace-boundary check |

`patch` first presents a preview (SHA-256 hashes + a diff summary) and is applied only
after approval.

**Tool call batching:** the model may emit several tool calls in one turn. When a
turn contains an approval-gated call (a `command`, or a `patch` on a non-tracked
path), any tool call ordered *after* it in the same turn is left unanswered and
cancelled on the next resume by the orphan-repair pass, so the model has to
reissue it. attini therefore instructs the model to place an approval-gated call
last in the turn (or emit it alone); read-only calls may be freely batched and may
precede an approval-gated call.

For longer tasks the model may keep its own working notes under the session's
scratchpad directory (`.attini/{NAME}/scratchpad/`) using `patch`; those files are
not tracked by git and never appear in `git diff`. Because they are non-tracked,
`patch` writes there are still shown for approval (they are not auto-applied).

**Lifecycle:** scratchpad files are not auto-cleaned during a session — there is no
time- or size-based cleanup. They persist until you delete the session directory
yourself (`rm -rf .attini/<NAME>/`).

## Current ask (read-only)

`attini ask -s NAME [QUESTION]` asks the model to summarise the current state of a
session without touching the conversation log: what is in progress, any pending tool
call, and (when a `QUESTION` is supplied) a direct answer to that question. Records
since the last compaction summary are used by default; `--all` uses the whole
conversation and `--limit N` keeps only the most recent N records.
`--max-tokens N` caps the summariser response size. It is purely
observational — adjust the course by running `attini tell -s NAME "<new
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
| `ATTINI_TEMPERATURE` | Default sampling temperature when `--temperature` is omitted. Precedence: CLI flag, then this env var, then 0 (deterministic). |
| `ATTINI_SYSTEM_PROMPT` | Default system prompt when `--system-prompt` is omitted. Precedence: CLI flag, then this env var, then none. |
| `ATTINI_STATUS_LINE` | Set to `0` to suppress the one-line status that `attini tell` prints to stderr at invocation start. Unset (or any other value) keeps it on. |
