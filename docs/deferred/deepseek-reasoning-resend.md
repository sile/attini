# DeepSeek reasoning_content resend spec gap

**Status:** Deferred. Open question about whether to restore `reasoning_content`
resending when a request carries `tools`.

## Observation

`docs/bug/conversation-log-bloat.md` documents Option C: "stop resending
`reasoning_content` on session resume," implemented in commit `52a9ee6`.
That commit changes `parse_conversation_line` (`src/session.rs:1352`) so the
restored `ChatMessage::Assistant` **always** drops `reasoning_content`, with
one exception: if `content` is empty **and** `tool_calls` is empty, the
reasoning is promoted to `content` (so an answer living only in the CoT is
not lost).

The rationale was sound: DeepSeek's `reasoning_content` is a private CoT
trace, re-sending it bloats every request and can anchor the model to stale
thinking (the 4.29 MB / 1,469-entry blob in `.attini/main/conversation.jsonl`).
A/B testing confirmed the difference in practice: with reasoning stripped,
a resumed session produced `SECOND_OK` / `ctx=1338`, so the agent continues to
work.

## The spec gap

`https://api-docs.deepseek.com/guides/thinking_mode` states:

> Please note that for requests carrying the tools parameter, the
> reasoning_content must be fully passed back to the API in all subsequent
> requests — even for turns where the model did not perform a tool call. If
> your code does not correctly pass back reasoning_content, the API will
> return a 400 error.

And:

> If the request carries the tools parameter: the reasoning_content of all
> previous turns should be passed back to the API and will be concatenated
> into the context.

This conflicts with the conditional strip. The strip was applied to **all**
requests regardless of whether `tools` was present. When `attini agent` runs
the main loop, it always sends `tools`, so the strip removes a field that the
spec (strictly) requires.

## Why the gap is easy to miss

`absent reasoning_content` did not reproduce a 400 during the A/B test. The
session resumed and produced `SECOND_OK`. So the API appears to tolerate the
omission in practice, or the test's message shape (low history, no tool_calls
in the resumed turn) avoided the strict path.

## "Final answer makes prior thinking irrelevant"?

One might argue: per the Thinking Mode guide, the model performs "multiple
turns of reasoning Before outputting the final answer". Once the final answer
is emitted, the preceding CoT has served its purpose, so it should be safe to
drop it. This intuition is correct *conceptually*.

But DeepSeek's API is **stateless**: the server holds no conversation. The
client resends the full history each call, and the server rebuilds context by
means of concatenation. So the reasoning_content of earlier turns is not
"consumed" once the final answer appears — it must be resent in subsequent
requests for the server to reconstruct context. The spec makes this mandatory
for tools-bearing requests (see above), even after the final answer.

Therefore:

- With `--thinking-effort=none`, no reasoning content is generated, so this
resend requirement does not apply at all (the session default is `none`, so the
requirement binds only when a session explicitly enables thinking via `low|high|max`).
- With thinking enabled (`low|high|max`), the requirement technically binds for
tools-bearing requests. If the model has already produced its final answer,
that answer is kept as part of the next request, and the CoT that preceded it
must still be resent to satisfy the spec at the API level.

In practice the strict path has not been triggered (no 400 observed), but the
conceptual argument "prior thinking is no longer needed after a final answer"
should not be used to justify dropping it: the server has no memory of the
prior turn, and relies on the client to resend the trace.

## Two directions

1. **Restore resend for tools-bearing requests.** This matches the documented
   spec. `parse_conversation_line` would need to know whether the outgoing
   request has `tools` (or simply always keep `reasoning_content` when the
   restored message is an assistant turn with tool_calls). Risk: cache hits
   drop, context may re-bloat.

2. **Keep the strip.** Observable behavior works today (A/B passed). The spec
   text is a constraint on the API, not a guarantee that omitting it fails. If
   a future `attini agent` run hits a 400 with `error.message` about
   `reasoning_content`, this is the first place to look.

## Why deferred

No concrete bug is reproduced today; the tradeoff is speculative. A/B showed
stripping works. Reverting the strip to blindly conform to the doc risks
reintroducing the very context bloat that `52a9ee6` fixed, without evidence
that the strict path is needed.

## Why not "recent-only" resend

A natural middle ground is to resend only the *most recent* `reasoning_content`
(e.g., the last N turns) instead of the full 4.29 MB history, to keep the
context bounded while still giving the model a hint of its recent thinking.
This is **intentionally not pursued**:

- **The spec is all-or-nothing.** The documented requirement is that
  `reasoning_content` of *all* previous turns be passed back, not a sliding
  window. A partial resend is exactly the kind of request the strict path is
  described to reject (400), so "recent-only" is *more* likely to trip the
  error than full strip, not less.
- **No evidence a prefix helps.** A/B stripping *everything* still produced a
  working agent (`SECOND_OK` / `ctx=1338`). If the model continues without any
  prior CoT, there is no demonstrated benefit to feeding it a partial slice.
- **Adds complexity without a use case.** "Recent-only" needs a per-turn
  cutoff policy, risks inconsistent context, and has no measured payoff.

## Decision snapshot

- Commit `52a9ee6` (conditional strip) remains.
- No code change planned until a real 400 is observed.
- "Recent-only" resend is rejected.
- Keep the strip; observe in practice. If a `400` referencing
  `reasoning_content` ever appears, revisit the conditional-resend fix.
- Track this as a watch item on the next DeepSeek API behavior change.

## How to revive

If an `attini agent` invocation ever returns `API request failed: ...` with a
message mentioning `reasoning_content` / `400`, re-introduce conditional
resend: send `reasoning_content` for assistant turns that carry `tool_calls`
in a `tools`-present request, and drop it otherwise.

## Decision snapshot

- Commit `52a9ee6` (conditional strip) remains.
- No code change planned until a real 400 is observed.
- Track this as a watch item on the next DeepSeek API behavior change.
