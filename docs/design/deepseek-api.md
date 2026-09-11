# DeepSeek API design decisions

**Status:** Implemented decisions + explicit not-planned decisions. This
document records the settled design choices for DeepSeek API usage. Still-
deferred options live in `docs/deferred/`.

## How this was produced

The DeepSeek API site was fetched with `curl` into scratchpad (SPA pages
resolved via `sitemap.xml`; readable text extracted with Python). Pages used:
`/api/create-chat-completion`, `/guides/thinking_mode`,
`/quick_start/token_usage`, `/api/get-user-balance`,
`/guides/kv_cache`, `/orchestration/...`. The current code was read and
compared.

## What attini sends today

- `model` (default `deepseek-flash`, `src/main.rs:10`)
- `messages` (session history + system/tool records)
- `stream: true`, `stream_options.include_usage: true`
- `max_tokens` (optional, `--max-tokens` / `ATTINI_MAX_TOKENS`)
- `temperature` (default `0`, `--temperature/-t` / `ATTINI_TEMPERATURE`)
- `thinking`: `{type: enabled|disabled}` + `reasoning_effort`
  (`--thinking-effort/-E`, session-persistent)

## Implemented decisions

### 1. `thinking` mode control

`--thinking-effort/-E` sets `thinking` + `reasoning_effort` per session.
Values: `none|low|high|max`. Default for new sessions is `none` (chain-of-
thought off). `none` maps to `{type: disabled}`; `low|high|max` map to
`{type: enabled}` + `reasoning_effort`. Persisted under
`.attini/<NAME>/thinking_effort`.

### 2. `temperature` for determinism

DeepSeek recommends `temperature: 0` for coding/math. `ChatRequest` defaults
to `temperature: 0` and accepts `--temperature N` / `-t N` /
`ATTINI_TEMPERATURE`. The value is sent only when thinking is `disabled`
(sampling parameters are ignored in thinking mode). Note nojson renders float
`0.0` as `0` on the wire; that is valid JSON and accepted by the API.

## Explicitly not planned

### Retry / backoff

`curl.rs` has no retry for 429 / 5xx / connection errors. A single transient
failure kills the whole invocation. Candidate: bounded exponential backoff
with jitter for transport errors and 429, retrying only idempotent request
shapes (a fresh chat completion is safe to retry).

**Decision:** do not implement. attini does not prioritise autonomy so
highly — a transient failure is handled acceptably by the human simply
re-running the command. The added backoff/jitter/idempotency complexity is
not worth it while manual intervention stays cheap. Revisit only if transient
failures become frequent enough that a manual re-run becomes genuinely
painful.
