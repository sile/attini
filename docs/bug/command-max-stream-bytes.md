# COMMAND_MAX_STREAM_BYTES is dead and the "total capped" claim is false

**Status:** Fixed. The constant was dead and the runtime accumulator had no
bound; `run_streamed` now truncates each stream at 256 KiB and sets a
`truncated` flag on the tool result, and the tool description says so.

## Where the constant lives

`src/sansio/agent.rs` (line 61):

```rust
/// Maximum bytes retained from either stdout or stderr of a running
/// command. Reaching this limit terminates the process group and
/// marks the tool result as `truncated`.
pub const COMMAND_MAX_STREAM_BYTES: usize = 256 * 1024;
```

It is `pub` and exported, but a repo-wide search finds **no reader**. The only
textual references are this constant itself and the note in
`docs/deferred/command-tool-timeout.md` (which mislocates it as "in
`child_output.rs`" — it is in `sansio/agent.rs`).

## The claim vs. reality

The doc comment asserts three behaviours that do not exist:

1. **"Reaching this limit terminates the process group"** — there is no
   process-group kill anywhere. `run_streamed` / `run_command_sync` never
   send signals; a hung or firehose command runs to completion.
2. **"marks the tool result as `truncated`"** — `command_result_json`
   (`src/tell_cli.rs`) emits only `exit_code`, `termination_reason`,
   `duration_ms`, `stdout`, `stderr`. There is no `truncated` member.
3. **The cap is actually applied** — it is not. The accumulation path
   (`child_output::pump`) does `accumulated.extend_from_slice(&chunk[..n])`
   for every byte, unbounded. `STREAM_DISPLAY_BYTES_PER_SECOND` (64 KiB/s)
   limits only the user-facing display copy, never the retained buffer.

So the tool description `"Output byte totals are capped; non-zero exit status is
returned as a normal result"` is **half true**: the non-zero status part is true,
the byte-cap part is false for retained output.

## Impact

- A single `command` that writes gigabytes (a build tool, a streaming
  `cat`, `yes`, a server dump) accumulates all of it into RAM and embeds it
  into `command_result_json`, which then becomes a huge `tool` message in
  the conversation. This inflates context and can push a session toward
  the compaction trigger / context overflow (the same failure mode fixed
  for compaction in `7eb4432`, but from the tool-result side).
- It contradicts the bounded-tool philosophy that `read` (1 MiB) and
  `search` (result caps) follow, and the advertised model contract.

## Adjacent finding

`COMMAND_STREAM_CHUNK_SIZE` (`src/sansio/agent.rs` line 66) is also unused;
`child_output.rs` has its own private `CHUNK_SIZE = 4 * 1024`. The two are
redundant, and the public constant duplicates an implementation detail that
lives in a module that cannot import it.

## Fix directions

**Minimum / correctness-first:** either wire the constant into the accumulator
(truncate the retained buffer at `COMMAND_MAX_STREAM_BYTES` and return a
`truncated` flag the way the doc comment describes), or delete the constant and
rewrite the tool description to say honestly that output is not capped.

**With the timeout work (`docs/deferred/command-tool-timeout.md`):** that memo's
MVP already proposes a process-group SIGTERM→SIGKILL watchdog. A natural
companion is to enforce `COMMAND_MAX_STREAM_BYTES` so that a streaming child
that exceeds the cap causes the same process-group termination and a
`truncated: true` tool result — i.e. treat the cap as a *runtime* bound, not a
post-hoc trim. The two fixes share the watchdog scaffold in `child_output.rs`.

**Note:** the constant lives in the Sans I/O layer (`sansio/agent.rs`), which
does not import `child_output.rs`. Threading it through requires either
duplicating it in `child_output.rs` or moving the command-execution cap into
the I/O layer (or a small shared module).

## How to verify the bug

1. `grep -rn COMMAND_MAX_STREAM_BYTES src` → only the definition in
   `sansio/agent.rs` (plus `docs/deferred/command-tool-timeout.md`).
2. Run a command that emits > 256 KiB (e.g. `bash -c "yes x | head -c 1000000"`
   via the `command` tool); the returned `stdout` exceeds 256 KiB, so the cap
   is demonstrably not enforced.

## Decision

Fixed (minimal/truncation route, not the watchdog): `run_streamed` now bounds
each retained stream at `COMMAND_MAX_STREAM_BYTES`, sets `ChildOutput::truncated`
when a stream exceeds it, and `command_result_json` serializes a `truncated`
member so the model sees that output was cut. The dead constants in
`sansio/agent.rs` were removed; the cap lives in `child_output.rs`, next to the
code that enforces it. The process-group watchdog from
`docs/deferred/command-tool-timeout.md` was deliberately left out for now.
