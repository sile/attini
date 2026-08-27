attini
======

A DeepSeek-powered coding agent prototype.

> This README is a work-in-progress draft. Keep it in sync with the implementation.

## Overview

- Talks directly to the DeepSeek Chat Completions API over HTTP/1.1 + TLS (streaming)
- Built on a Sans I/O design: parsers, data types, and state machines live in the
  core without performing I/O, while async transport, TUI, and filesystem
  integration live in surrounding modules
- The agent can use read-only tools (`list`, `read`, `search`) and a `patch` tool
  (add / update) that requires a preview and approval before applying changes
- Enforced constraints include workspace-boundary checks, file-size limits, and
  search-result limits

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

### Interactive TUI

```sh
attini tui [--model NAME] [--transcript PATH] [--metrics-snapshot-interval SECONDS]
```

Starts the agent with the current directory as the workspace.

`--transcript PATH` (optional) appends a JSON Lines session log to `PATH` for
later inspection with `jq`. The file is opened in append mode; each session
begins with a `session_start` record and ends with `session_end`. If the
file cannot be opened `attini tui` exits with a non-zero status.

`--metrics-snapshot-interval SECONDS` (optional, requires `--transcript`)
appends a `metrics_snapshot` record every `SECONDS` seconds, containing every
`AgentMetrics` counter as of that instant. Useful for tracking accumulator
trends over long sessions with `jq 'select(.kind=="metrics_snapshot")'`.

### One-shot chat

```sh
attini chat [--model NAME] [--system TEXT] [--show-reasoning] "<PROMPT>"
```

Streams the response to stdout. The tool loop is not run.

### Model selection

- Default model: `deepseek-v4-flash` (override with `--model`)

## Agent tools

| Tool | Description | Constraints |
| --- | --- | --- |
| `list` | List files and directories under a workspace-relative path | `max_entries` limit (default 200) |
| `read` | Read a UTF-8 text file | Up to 1 MiB; optional `line_range` |
| `search` | Literal substring search (no regex) | `max_results` limit (default 50) |
| `patch` | Batch of add / unique-replacement edits | Preview + approval before applying; `before` must match exactly once; workspace-boundary check |

`patch` first presents a preview (SHA-256 hashes + a diff summary) and is applied only
after approval.

## Environment variables

| Variable | Description |
| --- | --- |
| `DEEPSEEK_API_KEY` | API key (required). Use any non-empty value for local servers that ignore auth. |
| `DEEPSEEK_BASE_URL` | OpenAI-compatible API base URL (optional). Default: `https://api.deepseek.com`. Trailing slash is stripped; `/chat/completions` is appended. Example: `http://host:8888/v1`. |
