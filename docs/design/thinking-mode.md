# Thinking mode: disabled, fixed

**Status:** Implemented decision. attini never enables thinking mode; no
`reasoning_content` is requested, stored, or resent.

## Decision

attini sends `{"thinking":{"type":"disabled"}}` on every model call and does
not expose any way to enable chain-of-thought. This is a fixed design choice,
not a default.

## Why

### CoT is for a model that decides alone

The value of chain-of-thought is that the model reasons its way to an answer
by itself, across several internal turns. That is strong when **the human does
not intervene**.

attini's design is the opposite: the human is the final gate (patch preview +
approval, workspace boundary). Because the human approves, corrects,
or redirects each meaningful step, a long private exploration is mostly
**wasted** — the moment the human says "no, do it this way" the model discards
the CoT it just spent time and tokens producing. Worse, thinking tends to
deepen the *given* premise: when the premise itself is what the human will
correct, more thinking just elaborates a wrong direction more precisely. This
was observed in practice as an over-deliberation / inventory loop.

So: when the decision-maker is the human, CoT is friction, not capability.

### It was the source of the largest context bloat

`docs/bug/conversation-log-bloat.md` measured `reasoning` at 4.29 MB across
1,469 entries in a 12.6 MB conversation log. That trace was stored for every
thinking turn and (before the strip) re-sent on resume. With thinking fixed
off, the field is never produced in the first place, so an entire class of
context growth and a whole set of conditional-strip questions disappear.

### It removed a lingering spec liability

`docs/deferred/deepseek-reasoning-resend.md` records an unresolved gap:
DeepSeek's Thinking Mode guide requires that, for tools-bearing requests,
`reasoning_content` be resent in **all** subsequent requests or the API may
return a 400. attini strips it. The strict path was never observed to fire,
but the concern was real. With thinking disabled, no `reasoning_content`
exists to resend, so the spec requirement does not apply at all — the question
is closed by construction rather than answered.

## What was removed

- `ThinkingEffort` enum and the `thinking` / `reasoning_effort` wire fields
  (`src/sansio/deepseek.rs`). Requests now always carry
  `{"thinking":{"type":"disabled"}}`.
- `--thinking-effort` / `-E` and the session file
  `.attini/<NAME>/thinking_effort` (`src/main.rs`, `src/session.rs`).
- The `thinking=` token in the status line and the `thinking:` line in
  `session show` (`src/tell_cli.rs`, `src/session_cmd.rs`).
- `--show-reasoning` and the `reasoning` progress sink (`src/main.rs`,
  `src/curl.rs`). With thinking off there are no reasoning deltas to show.
- `reasoning_content` on `ChatMessage::Assistant`, the `reasoning` field on the
  `assistant` session record, and the conditional strip / promotion logic in
  `parse_conversation_line` (`src/sansio/deepseek.rs`, `src/session.rs`).
- The `reasoning` / `reasoning_delta` plumbing in the sans-io core: the
  `PendingResponse.reasoning` field, `Event::ReasoningDelta`,
  `on_reasoning_delta`, and the `reasoning_deltas_*` counters
  (`src/sansio/agent.rs`).
- `docs/deferred/deepseek-reasoning-resend.md` (moot: nothing to resend).

## Trade-off accepted

The one situation where CoT helps is "the human also does not know the answer
and wants the model to find it in one shot, to be reviewed afterwards." attini
is not used that way; its workflow is interactive. If that use case ever
becomes real, it should be reintroduced deliberately (and the resend-spec
question revisited alongside it), not kept as a half-on default.

## How to revisit

If a one-shot autonomous design task becomes a real workflow, re-add a
thinking toggle **and** the matching `reasoning_content` resend behaviour as a
single, tested change. Do not re-add the toggle without the resend handling;
that is the gap this removal closed.
