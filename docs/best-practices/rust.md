# Rust best practices

Practical notes for Rust repositories that are worked on by an agent (and by
people) in attini.

## Quiet cargo output

`cargo test` (and related commands) print a lot of per-test boilerplate on
success: `running N tests`, `test foo::bar ... ok` lines, and a final blank
line. In an agent-run tool result this becomes tens of KiB of noise per
invocation, which both bloats the conversation log and distracts the model.

### The fix

Add a repository-level `.cargo/config.toml`:

```toml
[term]
quiet = true
```

This applies to **every** cargo invocation in the repository and strips only
the success boilerplate. It is emphatically **not** a hard output cap: a single
command can still produce unbounded output if the tool itself produces it.

### What `quiet` preserves

| Command / case | Behaviour under `quiet = true` |
|---|---|
| `cargo test` (success) | `test result: ok` only; per-test boilerplate removed |
| `cargo test` (failure) | `panicked at ...`, `---- <name> stdout ----`, `test result: FAILED` all kept |
| `cargo build` (warnings) | `warning:` lines fully shown |
| `cargo fmt --check` | `Diff in ...` fully shown |

This is the crucial property: **failures, warnings, and formatting diffs are
never swallowed**, only the success noise is.

### What does *not* work

```toml
[alias]
test = "test -q"
```

cargo refuses user-defined aliases that shadow a built-in command
(`warning: user-defined alias `test` is ignored`). Do not try this.

### Scope and limits

- It affects **cargo only**. `git`, `bash`, `grep`, and other tools are
  unaffected.
- It is **repository-wide**: any cargo call in the checkout is quiet, so it
  applies to developers too, not just the agent.
- It is a **noise reduction**, not a size cap. If a tool itself emits a huge
  stream, `quiet` will not help — see `docs/bug/command-max-stream-bytes.md` for
  the unrelated, still-unimplemented output cap.

### Runtime override

`[term] quiet = true` is a config-file **default**, not a hard setting. You can
opt back in to full output for a single command without editing the file:

```sh
cargo test -v                 # verbose, per-invocation
CARGO_TERM_QUIET=false cargo test   # env var overrides the config
```

The environment variable takes precedence over `.cargo/config.toml`, so
`CARGO_TERM_QUIET=false` reliably restores normal output. Use `-v` for that
one command, or the env var when you want a deterministic, explicit override.

### Why this exists

This repository was set up after measuring a 12.6 MB / 8.4k-line conversation
log where `cargo test` accounted for 1.40 MB across 69 calls
(`docs/bug/conversation-log-bloat.md`). A one-line config removes ~99% of that
noise without touching the tool, the model, or the approval flow.
