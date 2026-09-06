# `plan` command argument/help bugs (deferred)

**Status:** Deferred. Not urgent. This memo documents three related defects in the
`plan` (and friend) command arg/help path, the root cause, and the intended fix.
It was left as a memo on request and is not fixed yet.

## Repro

All on a debug build (`cargo build` then `./target/debug/attini ...`).

```text
$ attini plan ok --help
missing argument '<PLAN.md>'   (exit 2)

$ attini plan create -h
missing argument '<PROMPT>'    (exit 2)
```

Also:

```text
$ attini plan check/run/close -h      # same missing '<PLAN.md>'
$ attini agent -h                     # lists `plan`, `session` though irrelevant
$ attini plan -h                      # lists `session` though irrelevant
```

## Bugs

### 1. `plan <sub> -h` / `--help` fails with "missing argument"

Every `plan` subcommand (`create`/`check`/`ok`/`run`/`close`) reproduces this.

Root cause: the required positionals lack `example(...)`:

- `create`'s `<PROMPT>`: `noargs::arg("[PROMPT]")` with no `.example(...)`.
- `check`/`ok`/`run`/`close`'s `<PLAN.md>`: `noargs::arg("[PLAN.md]")` with no
  `.example(...)`.

In `../noargs/` (see `src/arg.rs`, `basics.rs`), `Arg::take` returns `Arg::Example`
in help mode only when `example` was set, otherwise `Arg::None`. `Arg::None` has
`is_present() == false`, so the later `taken.then(|a| a.value().parse())?` throws
`Error::MissingArg`. `basics.rs` explicitly states a required positional / option
must set `example(...)` so help mode renders a meaningful Usage / Example. This is
an attini misuse of noargs, not a noargs defect.

### 2. Sibling commands leak into rendered help

`agent -h` shows `plan` and `session`; `plan -h` shows `session`; `plan ok -h`
would show `run`/`close`. The `Usage: attini ... plan` literal `...` and the stray
`<COMMAND>` also appear here.

Root cause: dispatch structure in `src/main.rs`. `try_run_agent` / `try_run_plan`
match a command, then on `help_mode` return `Ok(None)`, so `run()` keeps probing the
remaining top-level commands (`plan`, `session`). Each probe calls `cmd("...")`
which records a `Cmd::None` via `with_record_cmd`. `HelpBuilder::new` keeps all
entries after the **last present** command as the subcommand block, so sibling
commands are treated as subcommands and printed.

`../noargs/examples/subcommands.rs` avoids this by returning `Ok(true)` even in help
mode (e.g. `try_run_hello`), so `try_run_hello() || try_run_sum() || try_run_echo()`
short-circuits and no sibling `Cmd::None` is recorded. attini does not follow that
pattern.

### 3. Literal `...` in `Usage: attini ... plan`

This is noargs rendering, not attini. `HelpBuilder::build_usage` inserts
`" ... {name}"` when a subcommand context is matched (`src/help.rs`). noargs' own
test `after_subcommands_help` expects `Usage: <APP_NAME> ... get`, so it is pinned
by upstream tests. Not fixable from attini alone; treat as upstream behavior.

## Planned fix

1. **Bug 1:** add `example(...)` to the required positionals:
   - `create` -> `noargs::arg("[PROMPT]").example("<PROMPT>")`
   - `check` / `ok` / `run` / `close` -> `.example("<PLAN.md>")`
   (small, low risk, 5 sites).
2. **Bug 2:** introduce a tri-state outcome (e.g. `CommandOutcome` =
   `NotHandled` / `Done` / `Exit(ExitCode)` / `Help`) and have each `try_run_*`
   handler and its subcommand helpers return `Help` on help mode so dispatch
   short-circuits once a command is matched. This fixes both the top-level and the
   subcommand-level leak (`plan ok -h` showing `run`/`close`).

## Cost / effort

Most changes are in `src/main.rs`. The `patch` tool requires distinct paths per call,
so a multi-edit refactor of one file needs several separately-approved patches. Bug
1 is trivial; Bug 2 is a moderate refactor touching the dispatch chain and every
`try_run_*` return site.

## How to resume

Start with Bug 1 (the 5 `example(...)` additions) and verify each `plan <sub> -h`.
Then do Bug 2 by aligning attini's dispatch with the `../noargs/examples/subcommands.rs`
short-circuit pattern. Bug 3 needs no attini change.
