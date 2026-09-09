# DeepSeek API design/implementation considerations

**Status:** Partially implemented. Option 1 (`--thinking-effort`, default
`none`) and Option 3 (`temperature: 0`, CLI `--temperature/-t` /
`ATTINI_TEMPERATURE`) are done. Options 2, 4, 5, 6 remain deferred.

## How this was produced

The DeepSeek API site was fetched with `curl` into `.attini/main/scratchpad/`
(SPA pages resolved via `sitemap.xml`; readable text extracted with Python).
Pages used: `/api/create-chat-completion`, `/guides/thinking_mode`,
`/quick_start/token_usage`, `/api/get-user-balance`,
`/guides/kv_cache`, `/orchestration/...`. The current code was read and
compared: `src/sansio/deepseek.rs`, `src/curl.rs`, `src/sansio/sse.rs`,
`src/agent_cli.rs`, `src/session.rs`, `src/main.rs`.

## What attini sends today

- `model` (default `deepseek-v4-flash`, `src/main.rs:9`)
- `messages` (session history + system/tool records)
- `stream: true`, `stream_options.include_usage: true`
- `max_tokens` (optional, `--max-tokens` / `ATTINI_MAX_TOKENS`)

**Not sent:** `temperature`, `top_p`, `tool_choice`, `response_format`, `stop`,
`thinking`, `reasoning_effort`. Searches for these in `src` returned **no
matches**.

## Candidate improvements (ordered)

### 1. `thinking` mode is on by default — make it explicit / opt-out

The docs define `thinking: {type: enabled|disabled}` and
`reasoning_effort: low|high|max`. The default is `enabled` with `high`
effort. **attini never sends `thinking`, so every agent run is in thinking
mode today.** This is consistent with the 4.29 MB / 1,469-entry
`reasoning_content` blob in `.attini/main/conversation.jsonl`.

For a code-editing agent, thinking is arguably wasteful: it inflates
`reasoning_content` (context bloat, cost) and is partly redundant with the
agent's own step-by-step tool use. **Candidate:** add `thinking` config
(CLI `--no-think` / `--think`, and/or `reasoning_effort low|high|max`), and
consider making `disabled` the default for `attini agent`.

Caveat: `deepseek-reasoner` handles reasoning natively and may not support
`thinking` overrides the same way. Verify exact model names and parameter
availability against the official docs before implementing.

### 2. `reasoning_content` resend spec (see separate memo)

`docs/deferred/deepseek-reasoning-resend.md` records the documented rule: when
a request carries `tools`, `reasoning_content` must be passed back or the API
returns 400. Commit `52a9ee6` strips it. No 400 has been observed in practice;
kept as a watch item.

### 3. `temperature` for determinism — **DONE**

DeepSeek recommends `temperature: 0` for coding/math. `ChatRequest` now defaults
to `temperature: 0` and accepts `--temperature N` / `-t N` / `ATTINI_TEMPERATURE`.
The value is sent only when thinking is `disabled` (sampling parameters are
ignored in thinking mode). Note nojson renders float `0.0` as `0` on the wire;
that is valid JSON and accepted by the API.

### 4. Retry / backoff

`curl.rs` has no retry for 429 / 5xx / connection errors. A single transient
failure kills the whole invocation. Candidate: bounded exponential backoff
with jitter for transport errors and 429, retrying only idempotent request
shapes (a fresh chat completion is safe to retry).

### 5. Model split (agent vs. summariser vs. ask)

`DEFAULT_MODEL = "deepseek-v4-flash"` is used uniformly for agent,
summariser, and `ask`. Some reasoning models do not support tool calling, so a
future split (a cheaper model for summariser/ask, a tool-capable one for the
agent loop) may be desirable if `deepseek-reasoner` becomes the primary
agent model.

### 6. Additional finish reasons

Docs list `finish_reason: content_filter` and `insufficient_system_resource`.
attini currently treats a non-`stop` finish generically. `content_filter`
means the response was truncated by the filter; surfacing it as a tool error
would help the model react. Low priority.

## Why deferred

None of these is a live bug. Option 3 is the only one that is effectively
dead without the thinking change (Option 1), so the two are coupled. Options
4 and 5 are ergonomics / robustness, not correctness. This memo records the
landscape so the next API-related change can be made deliberately.

## How to revive

Implement Option 1 first (explicit `thinking` / `reasoning_effort`), verify
with a live run that tool-calling still works with `thinking: disabled`, then
wire `temperature: 0`. Add retry (Option 4) only if transient failures are
observed in practice. Re-check all model names / parameter names against the
official docs at implementation time; this note's values came from the docs
review session and could drift.
