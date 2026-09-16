attini
======

**WIP**

A CLI coding agent that aims to be as autonomous as it can be, without ever
leaving your control.

attini lives in your workspace and works on its own: it lists and reads files,
searches the codebase, applies patches, and runs commands. Safe, reversible
changes are applied without a prompt; anything consequential — a new file, a
change outside git, a command that isn't explicitly allowed — is shown as a
preview and waits for your approval.

The name comes from the Attini tribe of ants — leaf-cutter ants that do not eat
the leaves they gather, but cultivate a fungus with them. attini follows the
same idea: the agent gathers changes, but nothing becomes real until the human
cultivates it through approval.

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
  in-flight intent is observable via `attini ask` / `attini status`; and a
  one-line status is printed to stderr so you always know which session/model
  is advancing. Diagnostic output never pollutes stdout.
- **No silent side effects.** Reaching outside the workspace — a read or a
  write — always requires explicit human approval, destructive operations
  require explicit confirmation, and a non-tracked file never silently
  overwrites a tracked one. Where a behaviour is too risky to do safely, attini
  refuses rather than guesses — for example rejecting multiple edits to the
  same path in one `patch`.
- **The model's own space is gated too.** The model can work freely in its own
  scratchpad, but every write there still passes through approval.
- **Tools amplify understanding; they do not replace it.** The agent already
  reads, searches, edits, and runs commands — tools make those faster and
  safer, but they never hand the agent a capability it does not understand.
  attini has no `plan` tool that plans for the model, no `skill_load` that
  injects context mid-run, and no `subagent` that delegates the thinking away.
  When it can already do the work, attini tunes the environment itself (for
  example quieting the default `cargo` output) rather than inventing a magic
  tool.

This is why, for instance, there is no automatic skill or instruction-file
discovery (no `~/.attini/skills`, no `.attini/skills`, no `AGENTS.md` scan) —
context should enter a session only because the human asked for it, in the
prompt. See [Intentionally not supported](#intentionally-not-supported) for the
concrete list of removed and never-added features.

## Intentionally not supported

attini has grown by *removing* whole classes of convenience features rather
than retaining them. That removal is policy, not an accident or unfinished
work. The list below answers "if you look for feature X, is it gone because it
was bad, or just not built yet?" — for these, deliberately:

- **Memory / automatic context persistence.** The three-tier `memories.md`
  loading was removed. If you need persistent context, put it in the prompt.
- **Subagent / delegation.** `subagent_run` was removed: it was synchronous and
  serial, and its only real value (context isolation) is already available by
  running a separate session yourself.
- **`AGENTS.md` / agent instruction files.** Not implemented, and intentionally
  not planned — implicit discovery by file-name convention is exactly what
  attini avoids.
- **Auto skill load / implicit skill discovery.** No `skill_load` tool, no
  scanning of `~/.attini/skills` or `.attini/skills`. Context enters only
  because you asked for it.
- **`--skill` / `--reference` flags.** Removed. The only thing they added over
  a plain prompt was landing text in the system prompt — not a guarantee the
  model obeys (there is none, by LLM nature). Paste via `--stdin` instead.
- **Plan mode (`--plan=on|off`).** Removed. attini already gates consequential
  writes; a separate mode added a second, redundant notion of "how much
  approval" and a state file to keep in sync. Use `attini ask` to inspect a
  session read-only.
- **`--local-only` mode and rule attributes (`readonly` / `network`).**
  Removed, along with the `Mode` axis. Approval is a single, flat thing: a rule
  is allow or deny, or absent (which falls through to a pending approval).
- **Convenience tools that duplicate what the model already has** (`search`
  regex, a `sed`-style replace tool, a cross-file replace tool). Not added.
  `patch` already replaces unique substrings across multiple files, `command`
  already runs `grep`/`sed` under permission control, and literal `search`
  stays literal — a regex engine would add a dependency and a hang risk for
  little measured gain. Adding tools raises the permission/approval surface;
  see the rationale in
  [the extended list](docs/design/intentionally-not-supported.md).

The common thread: implicit, convention-based context or delegation that
attini cannot see or control. attini's answer to each is "be explicit" — put
it in the prompt, or run the other session yourself. "Unsupported" here means
"ask for it explicitly and it works as asked", not "the feature is missing and
should be added".

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
attini tell [--system-prompt TEXT] [--max-tokens N] [--temperature N] [--command-timeout N] [--stdin] "<PROMPT>"

attini approve [-s NAME] [--grant oneshot|session|workspace] [--command-timeout N]
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

`--command-timeout N` caps how long a single `command` tool call may run, in
seconds (default 180). The child runs in its own process group and is killed
with SIGTERM (then SIGKILL after a one-second grace) on expiry; the tool
result then reports `termination_reason: "timeout"`. `0` disables the cap. It
can also be set via `ATTINI_COMMAND_TIMEOUT_SECONDS`; precedence is CLI flag,
then env var, then the 180-second default.

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
  and executed, then the turn continues. A pending call is a `patch`, a
  `command`, or a `read`/`list`/`search` that targeted a path **outside the
  workspace**. The read case is a **one-shot** grant by default: that single
  call is allowed through, nothing is written to `permissions.jsonl`, and a
  later read of the same path asks again. To make it stick, use
  `--grant session|workspace` (which appends a `read` rule —
  workspace-relative inside the workspace, absolute outside it), or add a
  `read` rule to `permissions.jsonl` by hand.
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
do not have to edit `permissions.jsonl` by hand afterward:

| `SCOPE` | Effect |
|---|---|
| `oneshot` | approve only, persist nothing (the default) |
| `session` | approve, then append a rule to the session `permissions.jsonl` |
| `workspace` | approve, then append it to the workspace-wide `permissions.jsonl` |

What gets persisted depends on the pending call's **kind**: a `command` stores
its args-prefix (`{"type":"command","allow":true,"args_prefix":[...]}`), a
`read` stores its path (`{"type":"read","path":...}` — read rules are
allow-only, so `allow` is omitted), and a `patch` stores its path as a `write`
rule (`{"type":"write","allow":true,"path":...}`). Inside the workspace that
path is workspace-relative, the shape rules are written in; outside it, the
absolute path is kept. `SCOPE` keeps the same meaning throughout — how long the
grant lives — so one flag covers all three kinds.

Approval and grant are independent: the approval always stands, and a grant that
cannot be written (already granted, a conflicting deny rule, or an I/O error) is
reported as a one-line warning rather than rolling the approval back. A grant
that cannot be *formed* — a command's argv yields no prefix, a read has no
resolvable path, a patch touches more than one distinct path, or several calls
are pending — is rejected up front. The argv-prefix is truncated the same way as
the printed suggestion (first two elements, e.g. `cargo test`), so the two never
disagree.

A patch that touches a non-git-tracked path (including any non-scratchpad write
in a non-git workspace) is still *writable*, but it is parked for approval with a
`NOTE:` line explaining that `git checkout` cannot undo it. `approve --grant` is
the way to auto-approve such a write without hand-editing `permissions.jsonl`.
The only hard refusal left is an `Add` into a gitignored region, which must be
lifted by a hand-written `write` rule (see the permissions section), not by
`--grant`.

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
attini status   -s NAME [--json]   # lock + summary + pending calls + aggregate metrics
attini logstats -s NAME [--json]   # record-kind histogram, bytes by tool/command family
attini ask      -s NAME [QUESTION] # ask the model to summarise the current state
```

There is no manual `prune`: `conversation.jsonl` is append-only, but once it grows
past 100 MB the next compaction pass drops the records before the midpoint at a safe
boundary (never splitting an `assistant -> tool` pair), roughly halving the file.

Permissions live in plain `permissions.jsonl` files -- **JSONL**: one rule per
line, `#` comments allowed, edited by hand. Each rule has `type` (`command`,
`read`, or `write`); `command` and `write` rules carry `allow`
(`true`/`false`), while `read` rules are allow-only and omit it. The layer a
rule belongs to is the file it lives in: `.attini/permissions.jsonl`
(workspace) or `.attini/<NAME>/permissions.jsonl` (session). Evaluation is
last-match-wins over `workspace ++ session`, so a session rule overrides a
workspace one.

```jsonl
# allow cargo test
{"type":"command","allow":true,"args_prefix":["cargo","test"]}
# deny destructive rm
{"type":"command","allow":false,"args_prefix":["rm"]}
# read outside the workspace (read rules are allow-only)
{"type":"read","path":"../docs/"}
# let the model write under src/ without prompting
{"type":"write","allow":true,"path":"src"}
# never touch generated output
{"type":"write","allow":false,"path":"dist"}
# allow writing a file outside the workspace (absolute path)
{"type":"write","allow":true,"path":"/home/me/other-repo/notes.md"}
```

A `write` rule governs `patch` edit targets before the git-tracking heuristic: a
winning `allow:true` rule permits a write even to an untracked file (or one
outside the workspace); a winning `allow:false` rule refuses one even to a
tracked file. A write outside the workspace with no matching rule is parked for
one-shot approval, mirroring `read`. See the `patch` row in *Agent tools* below.

A `read` rule **widens** the roots a `read`/`list`/`search` may reach; it is not
a gate. There is no `read` deny, and `allow:false` on a `read` rule is a load
error, because a read deny cannot be enforced (the model can always read through
the `command` tool). Keep a file you do not want read out of the workspace rather
than writing a deny rule.

Rules are added by hand, or through `attini approve --grant` (see the Approving
section above), which folds a persistent rule into the approval you were already
giving.

### Model selection

- Default model: `deepseek-flash` (override with `--model`)

## Agent tools

| Tool | Description | Constraints |
| --- | --- | --- |
| `list` | List files and directories under a workspace-relative path | `max_entries` limit (default 200) |
| `read` | Read a UTF-8 text file | Up to 1 MiB; optional `line_range` |
| `search` | Literal substring search (no regex) | `max_results` limit (default 50) |
| `patch` | Batch of add / unique-replacement edits | Edits limited to git-tracked files, or covered by a `write` `allow:true` rule, are auto-applied; any add, non-tracked edit, or target outside the workspace not covered by a rule needs approval. `before` must match exactly once |

`patch` first presents a preview (file names + a diff body) and is applied only
after approval, unless every edit is auto-approvable (git-tracked, or allowed by
a `write` rule).

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
| `ATTINI_COMMAND_TIMEOUT_SECONDS` | Default `command` tool timeout in seconds when `--command-timeout` is omitted. Precedence: CLI flag, then this env var, then 180. `0` disables the cap. |
| `ATTINI_STATUS_LINE` | Set to `0` to suppress the one-line status that `attini tell` prints to stderr at invocation start. Unset (or any other value) keeps it on. |
