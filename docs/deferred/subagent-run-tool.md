# Subagent tool evaluation (deferred)

**Status:** Deferred. This document records an evaluation of the existing
`subagent_run` tool and the decision not to invest further in it. It is not a
bug report and no code change is required.

## What the tool is

`subagent_run` is a single model tool that lets the parent self-exec the
current `attini` binary as a child `attini agent` in a separate session and
**blocks until the child reaches a terminal state**. It returns
`{session_name, state, content}` where `content` is the latest Assistant text
from the child's `conversation.jsonl`.

It is **synchronous and blocking** — it is *not* a concurrency / parallelism
mechanism. The child's stdout is streamed to the parent's stderr under the
shared rate limit; the tool result content still comes from the child session
on disk, not from that stream.

Availability is gated on `$ATTINI_IS_SUBAGENT` being unset. In the normal
(non-subagent) case this is true, so the tool **is always advertised** and its
full tool def + params schema are sent to the model on every turn.

## Merits (beyond concurrency)

The only real, unique value is **context isolation**:

- A child session has its own conversation, so a self-contained sub-task's
tool results and intermediate reasoning do not inflate the parent's context.
- The parent only receives the last Assistant text, so a huge child transcript
does not pollute the parent.
- This can act as an alternative to `compact` (give a small prompt to a fresh
child rather than retaining the parent's megahistory).

Secondary benefits:

- **Permission separation** — the child does not inherit the parent's
`ApprovedPlan`; it runs with its own authorization.
- **Separate audit / metrics** — the child creates its own session, so
tool-call counts and token usage are recorded independently.

## Demerits

The user's initial list (complexity, memory/cost, reduced controllability) is
accurate. Additional observations from the implementation:

- **Complexity** — self-exec (`current_exe`), child spawn, preflight state
machine (`fresh` / `idle` / `Pending` / `Busy`, via `preflight`), terminal-state
classification (`Completed` / `AwaitingApproval` / `Error` / `Crashed`, via
`classify_state`), and reading the child session back from disk. Many edge
cases.
- **Resource / cost** — a child process plus a fresh model session. Both the
parent's turns and the child's turns bill against the same API key: double
token spend, plus a second API connection.
- **Controllability** — the parent blocks and cannot observe or steer the child
mid-run. Only the final `content` is returned; the child's tool trail is
invisible unless you read its `conversation.jsonl` or manually run
`attini agent -s <child>`.
- **Latency** — synchronous, so a long sub-task blocks the parent for the
whole duration.
- **Failure modes** — `Crashed` (OOM / SIGKILL / panic-abort before
`InvocationEnd` is appended) is reported and the caller must decide whether to
retry. No partial continuation.
- **Always-on exposure** — because `subagent_available` defaults to true, the
tool definition is in the prompt every turn, consuming tokens even when unused
and creating a risk that the model delegates something that would have been
simpler inline.
- **Session-reuse ambiguity** — omitting `session_name` auto-generates a name,
but an idle existing session is reused; repeated calls can re-target a previous
child's session, carrying stale state across calls.

## Existing alternatives (zero code)

The context-isolation merit is largely achievable without `subagent_run`:

1. **Separate session by hand.** Start `attini agent -s <other> "..."` in the
same workspace. The human carries the result back.
2. **`attini ask -s <name>`.** A read-only model summary of an existing session
(no continuation, no tool calls) that gives a fresh, bounded description of
what a session did.

Both deliver most of the clean-room read benefit without a model-facing,
blocking, always-advertised tool.

## Value vs. cost

**Value (low-to-moderate, mostly ergonomic):** fresh, isolated context for a
self-contained sub-task; independent audit/permissions.

**Cost:** the hardest problem is the synchronous blocking design plus the
per-turn tool-definition cost and the risk of unexpected delegation. The tool's
only distinctive benefit is duplicated by existing manual multi-session
workflows.

## Decision

**Deferred / not worth further investment.** While unused and not intended for
concurrency, the tool's value does not justify its complexity. The marginal
benefit over a manual `attini agent -s <other>` / `attini ask -s <other>`
workflow is small.

## How to revive (or simplify)

- **Simplify option (preferred if the tool stays):** stop advertising it by
default. Gate it behind `--enable-subagent` (CLI) or an env var so its tool
def + params schema are not sent to the model every turn.
- **Remove option:** full removal of `subagent_run`, `subagent.rs`, and the
`ToolKind::SubagentRun` / `subagent_available` plumbing, plus README and
metrics updates.
- **Revive only if** a concrete, recurring use case appears where a manually
run separate session proves too awkward (e.g. the user wants to delegate a
sub-task from inside the loop and reliably capture only its final answer).
Document that use case here before re-implementing or re-exposing.
