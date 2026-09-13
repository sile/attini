# DeepSeek API deferred options

**Status:** Deferred. Options 5 and 6 below remain open. Implemented and
explicitly-not-planned decisions are recorded in
`docs/design/deepseek-api.md`.

> Option 2 (`reasoning_content` resend) is **moot**: thinking mode and the
> whole reasoning pipeline were later removed, so no `reasoning_content` is
> generated or sent. The former memo `docs/deferred/deepseek-reasoning-resend.md`
> was deleted with that change. Kept out of the open list below.

## 5. Model split (agent vs. summariser vs. ask)

`DEFAULT_MODEL = "deepseek-flash"` is used uniformly for agent,
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

Option 5 (model split) only matters once the primary agent model changes
(e.g. if a tool-capable `deepseek-reasoner` variant becomes the default).
Option 6 (`content_filter` / `insufficient_system_resource` finish reasons)
is a small, self-contained addition to the response handling. Retry (Option 4)
is currently rejected — revisit only if transient failures become frequent
enough that a manual re-run becomes genuinely painful. Re-check all model
names / parameter names against the official docs at implementation time;
this note's values came from the docs review session and could drift.
