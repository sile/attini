# DeepSeek API deferred options

**Status:** Deferred. Options 2, 5, and 6 below remain open. Implemented and
explicitly-not-planned decisions are recorded in
`docs/design/deepseek-api.md`.

## 2. `reasoning_content` resend spec (watch item)

`docs/deferred/deepseek-reasoning-resend.md` records the documented rule: when
a request carries `tools`, `reasoning_content` must be passed back or the API
returns 400. Commit `52a9ee6` strips it. No 400 has been observed in practice;
kept as a watch item.

## 5. Model split (agent vs. summariser vs. ask)

`DEFAULT_MODEL = "deepseek-v4-flash"` is used uniformly for agent,
summariser, and `ask`. Some reasoning models do not support tool calling, so a
future split (a cheaper model for summariser/ask, a tool-capable one for the
agent loop) may be desirable if `deepseek-reasoner` becomes the primary
agent model.

## 6. Additional finish reasons

Docs list `finish_reason: content_filter` and `insufficient_system_resource`.
attini currently treats a non-`stop` finish generically. `content_filter`
means the response was truncated by the filter; surfacing it as a tool error
would help the model react. Low priority.

## How to revive

Implement Option 1 first (explicit `thinking` / `reasoning_effort`), verify
with a live run that tool-calling still works with `thinking: disabled`, then
wire `temperature: 0`. Retry (Option 4) is currently rejected — revisit
only if transient failures become frequent enough that a manual re-run
becomes genuinely painful. Re-check all model names / parameter names
against the official docs at implementation time; this note's values came
from the docs review session and could drift.
