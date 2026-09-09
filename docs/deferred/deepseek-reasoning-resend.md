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

## How to revive

If an `attini agent` invocation ever returns `API request failed: ...` with a
message mentioning `reasoning_content` / `400`, re-introduce conditional
resend: send `reasoning_content` for assistant turns that carry `tool_calls`
in a `tools`-present request, and drop it otherwise.

## Decision snapshot

- Commit `52a9ee6` (conditional strip) remains.
- No code change planned until a real 400 is observed.
- Track this as a watch item on the next DeepSeek API behavior change.
