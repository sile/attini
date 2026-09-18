# `args_exact` for command permission rules

**Status:** Deferred. The `command` rule type currently matches on
`args_prefix` only; an `args_exact` matcher is recorded here as a wanted
addition. The implemented shape is documented in
`docs/design/permissions-file.md`.

## What is wanted

A second matcher for `command` rules, alongside `args_prefix`:

```jsonl
# today: matches when these tokens are a prefix of argv
{'type':'command','allow':true,'args_prefix':['cargo','test']}
# wanted: matches only when argv is exactly these tokens
{'type':'command','allow':true,'args_exact':['git','status']}
```

- `args_prefix` keeps its current meaning: the rule's array is a token-wise
  prefix of the command's argv. `['cargo','test']` matches
  `cargo test`, `cargo test --workspace`, `cargo test foo`, etc.
- `args_exact` matches only when the rule's array **equals** argv, element for
  element, with no extra trailing tokens. `['git','status']` matches
  `git status` and nothing else -- not `git status --short`, not `git stash`.

Exactly one of the two must be present on a `command` rule, and a rule that
carries both is a load error (the two matchers are mutually exclusive).

## Why

The prefix matcher is deliberately coarse: it is there so a broad allow (or
deny) can be written in one line. Its weakness is that it also matches
anything that merely *starts with* the same tokens, which is sometimes wider
than intended:

- `{'allow':true,'args_prefix':['cargo','test']}` also allows
  `cargo test -- --nocapture`, `cargo test && rm -rf target`, and any other
  argv that begins with `cargo test`.
- A deny written as a prefix can be sidestepped by appending an argument, and a
  narrow allow cannot be written at all.

When the intent is "approve this exact command, nothing longer", there is no
way to say it today; the only options are a too-broad prefix or a prompt every
time. `args_exact` closes that gap for the common case of a fully-specified
command (a specific `git` subcommand, a specific one-shot script invocation).

## Why it is enforceable here

Exact matching is only meaningful because attini execs `argv[0]` **directly,
without a shell** (`CommandInvocation::argv`; see
`docs/design/tool-call.md`). There is no shell to re-split, expand, or chain
tokens, so the argv the matcher sees is exactly the argv the OS receives.
Pipes, redirects, and `&&` are not interpreted; the model that wants them must
pass `['bash','-c','...']` explicitly, which is itself just an argv to match
against. That keeps `args_exact` a real guarantee rather than a string trick.

The same shell caveat already limits `args_prefix`: a prefix rule on `git`
does not stop `bash -c '...'`, which is a different argv. `args_exact` inherits
the same honest boundary -- it governs the argv of the call, not everything the
resulting program might do.

## How to revive

Small and self-contained, touching the same three places a rule type always
does:

1. `src/sansio/permissions.rs`: add an `args_exact` field (or make the matcher
   an enum) to `Rule`, add the match arm in `rule_matches_argv`, and mirror the
   shape through `RuleMatch` / `AutoDecision` for history output.
2. `src/permissions.rs`: parse and validate `args_exact` in `parse_rule_line`
   (present exactly once, non-empty, no empty elements, mutually exclusive with
   `args_prefix`), and render it in `DisplayJson`.
3. `docs/design/permissions-file.md`: document the field next to `args_prefix`.

Revisit when a real session wants to pre-approve a fully-specified command
without also approving arbitrary extensions of it. Re-check field naming and
the mutual-exclusion rule against the format conventions in
`docs/design/permissions-file.md` at implementation time.
