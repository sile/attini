# Design philosophy: tools amplify, they do not replace

**Status:** Deferred. Under discussion. Not yet added to README.

## Problem

attini's README has a "Design philosophy" section with four principles
(Context is requested / State changes are surfaced / No silent side effects /
The model's own space is gated too). The working hypothesis is that attini has
a fifth, currently implicit trait worth writing down: **tools exist to make
the agent's existing abilities efficient, not to give it abilities it does not
understand.**

This memo records the analysis and several phrasings. The decision is
intentionally left open; the idea still needs full discussion.

## Why this trait seems real

attini has removed, rather than retained, several "magic" tools that would
have done the thinking for the model:

- `plan` / `submit_plan` — would have amortised approval into a plan; removed
  (`4c7a211`, and the model-facing tool in later commits).
- `skill_load` — would have pulled skill content in mid-run; removed
  (`a159db0`). Implicit skill discovery was also removed, and the explicit
  `--skill PATH` flag that briefly replaced it has since been removed too —
  context now enters only via the prompt or `--stdin`.
- `subagent` — would have delegated a subtask to a child process; removed
  entirely (`ec12bc5`).

Meanwhile the retained tools (`list`, `read`, `search`, `patch`, `command`)
all assume the model already understands the work: they read, search, edit, and
run shells. They widen throughput, not comprehension.

There is also a concrete example of "amplify the environment, not the model":
when `cargo test` output was the largest source of conversation bloat, attini
chose `.cargo/config.toml [term] quiet = true` (`ea79845`) rather than adding a
model-facing `command --summary` mode. The fix tuned the environment instead of
adding a tool that would require the model to remember to pass `summary: true`.

## Candidate phrasings

### A. Tools amplify understanding; they do not replace it

> **Tools amplify understanding; they do not replace it.** The agent already
> reads, searches, edits, and runs commands — tools make those faster and
> safer, but they never hand the agent a capability it does not understand.
> attini has no `plan` tool that plans for the model, no `skill_load` that
> injects context mid-run, and no `subagent` that delegates the thinking away.
> When it can already do the work, attini tunes the environment itself (for
> example quieting the default `cargo` output) rather than inventing a magic
> tool. A tool that hides work is a liability; a tool that removes friction is
> worth keeping.

Strongest correspondence to the examples. Reads slightly long for a bullet,
but matches the length of the existing four.

### B. Friction, not capability

> **Friction, not capability.** attini adds no tool that lets the model do
> something it could not do before — only tools that reduce the cost of what it
> already does. If a task is already possible, attini improves the environment
> (e.g. quiet cargo output) rather than inventing a convenience tool.

Punchier and easier to remember, but leaves the "no magic tools" implication
less explicit.

### C. No magic tools

> **No magic tools.** The model is the one that thinks. attini's tools only
> remove friction around actions the model can already propose; they never
> perform a step the model cannot explain. When a behaviour could be improved
> without a new tool, attini changes the environment instead.

Short and direct; risks reading as a rule against any future tool rather than a
principle about where tools should sit.

### D. Fold into the opening slogan

Add a short clause to the README's first line:

> A CLI coding agent that aims to be as autonomous as it can be, without ever
> leaving your control — and whose tools make what it already does cheaper
> rather than give it powers it cannot explain.

Keeps the philosophy in one place but makes the opening line long and weakens
its punch.

## Discussion points

1. **Is it a separate principle or a corollary of existing ones?** "Context is
   requested, not discovered" already argues against implicit magic. "No
   silent side effects" covers safety. The new trait is about the *shape of
   the toolset*, so it may stand alone or be folded into the existing text.
2. **Should it be a bullet or a closing paragraph?** The four existing
   principles are bullets. This trait reads naturally as a fifth bullet, or as
   a one-line coda after them.
3. **Does the slogan need it?** Currently the slogan is about
   controllability/understandability over autonomy. "Tools amplify" is a
   distinct claim about the toolset, not about autonomy, so adding it to the
   slogan may muddy the message.

## How to revive

Look at README's "Design philosophy" section. If the trait is accepted, add
one bullet (probably A, trimmed) after the four existing bullets, then re-read
for length and redundancy. The decision is deferred; this memo records the
options so the future conversation can pick a phrasing rather than start from
scratch.
