# The permissions file: format and evaluation

**Status:** Design. This document records the settled shape of the
permissions file (`.attini/permissions.jsonl` and
`.attini/<NAME>/permissions.jsonl`) and the rule-evaluation model. It is the
target that the JSONL migration, the override-chain evaluation, and the
history output are implemented against.

## Why change it

The old on-disk shape was a single JSON object on one line
(`{command_prefixes:[...],extra_read_paths:[...]}`). Two problems:

- **Humans cannot read or edit it.** All rules crammed onto one line make
  `git diff` useless at line granularity, and hand-editing is error-prone.
- **Grant round-trips lose shape.** Writes always re-serialise the whole
  object, so a hand-edited file is rewritten into the compact form.

The project preference is that session state lives in files a human can open
and edit directly ("if you break it, recreate it"). The permissions file
should honour that, not hide behind `grant` subcommands.

## Format

The file is **JSONL**: one JSON object per line, one rule per line. This makes
adding a rule a one-line diff and keeps each rule self-contained.

- Lines beginning with `#` are comments and are ignored.
- Blank lines are ignored.
- Any other line must parse as a JSON object or it is an error for that file
  (the rule is reported and skipped; see "Error handling").

The schema does **not** carry a `scope` field. The layer a rule belongs to is
expressed by **which file it lives in**:

| Layer | File |
|---|---|
| workspace | `.attini/permissions.jsonl` |
| session | `.attini/<NAME>/permissions.jsonl` |
| oneshot | in-memory only, never written to disk |

## Rule shape

Field order is fixed: `type` -> `allow` -> type-specific. The examples below
use single quotes around strings to stay readable in this Markdown; the real
file uses JSON double quotes.

```jsonl
# allow cargo test
{'type':'command','allow':true,'args_prefix':['cargo','test']}
# deny destructive rm
{'type':'command','allow':false,'args_prefix':['rm']}
# read outside the workspace
{'type':'read','allow':true,'path':'../docs/'}
# but not this directory
{'type':'read','allow':false,'path':'secret/'}
```

- `type` (string, required): one of `command` or `read`. (`write` is a
  future addition; see `docs/deferred/write-permission-type.md`.)
- `allow` (bool, required): `true` = allow, `false` = deny. There is no
  implicit/pending value: an omitted `allow` is an error, so a typo never
  silently turns a rule off.
- `command` adds `args_prefix` (array of strings, required). It matches when
  the rule's prefix is a token-wise prefix of the command's argv.
- `read` adds `path` (string, required). It matches when the requested path is
  the given path **or under it** -- recursive, on path-segment boundaries. See
  "Path matching" below.

### Path matching (recursive, not string prefix)

A string prefix would let `foo/bar` match `foo/barbaz`. `read` rules match on
**path segments**: the rule's `path` matches the requested path itself and
everything underneath it, but not a sibling that merely shares a string prefix.

```
path foo/bar  ->  foo/bar      match
                  foo/bar/x    match
                  foo/bar/y/z  match
                  foo/barbaz   no match
```

This mirrors `Path::starts_with`, which compares components rather than raw
bytes.

## Evaluation: last match wins

Rules are evaluated as one ordered list, built by concatenation:

```
[ workspace rules ] ++ [ session rules ] ++ [ oneshot rules ]
```

Earlier layers are weaker; later layers override them. Evaluation walks the
list and keeps the decision of the **last** rule that matches. If no rule
matches, the outcome is `Pending` (ask the human).

This replaces the old model, where `deny` was checked across all scopes first
and short-circuited (so `deny` always beat `allow`). Under last-match-wins,
`allow` and `deny` obey the same rule, and precedence is purely positional:

- a workspace `allow` can be overridden by a session `deny`, and
- a workspace `deny` can be overridden by a session `allow`.

The second is intentional: a local layer winning over a broader one is the
whole point of having layers, and a session-scoped exception to a workspace
denial is a normal operation.

### The oneshot layer

`oneshot` is an in-memory set of prefixes approved **during the current
invocation** (via `attini approve --grant oneshot`). It is never written to
disk, so it disappears when the process exits. It sits last in the chain, so
it can override both file layers for the remainder of that one invocation.

## Error handling

- A whole-file read failure (missing file) is fine: that layer is empty.
- A whole-file read failure for another reason (permissions, IO) is reported
  and the layer is treated as empty.
- A malformed line is reported and **skipped**; the rest of the file still
  loads. Malformed is never silently ignored.

## History output

Every `command` evaluation records, in the `tool_approval` session record, not
just the final decision but **every rule that matched** during the walk, in
evaluation order, each marked as adopted or not. This lets a reader reconstruct
which layer's rule produced the final answer, instead of seeing only the
outcome. (Details of the record shape live with `AutoDecision` /
`AutoDecidedBy`.)

## Related

- `docs/design/tool-call.md` -- how an approval-requiring call is parked and
  resumed.
- `docs/deferred/read-outside-workspace-approval.md` -- promoting a
  `read` denial into an approval request.
- `docs/deferred/write-permission-type.md` -- adding a `write` type.
