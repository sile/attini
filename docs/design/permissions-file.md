# The permissions file: format and evaluation

**Status:** Implemented (format + last-match-wins evaluation, plus `read` and
`write` enforcement). The `docs/deferred/permissions-json-editability.md` memo is
superseded by this document and has been removed. The `read` denial to approval
path is implemented (`docs/deferred/read-outside-workspace-approval.md`); the
`write` type is implemented (`docs/design/write-permission-type.md`).

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
# let the model write under src/ without prompting
{'type':'write','allow':true,'path':'src'}
# never touch generated output
{'type':'write','allow':false,'path':'dist'}
```

- `type` (string, required): one of `command`, `read`, or `write`. See
  `docs/design/write-permission-type.md` for the `write` type.
- `allow` (bool, required): `true` = allow, `false` = deny. There is no
  implicit/pending value: an omitted `allow` is an error, so a typo never
  silently turns a rule off.
- `command` adds `args_prefix` (array of strings, required). It matches when
  the rule's prefix is a token-wise prefix of the command's argv.
- `read` adds `path` (string, required). It matches when the requested path is
  the given path **or under it** -- recursive, on path-segment boundaries. See
  "Path matching" below.
- `write` adds `path` (string, required). Same recursive, segment-boundary
  matching as `read`, but it governs `patch` edit targets, not reads. See
  `docs/design/write-permission-type.md`.

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
[ workspace rules ] ++ [ session rules ]
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

### One-shot approval

`attini approve --grant oneshot` approves the pending call (which runs
immediately) and persists nothing. That is enough while approval and
execution stay in the same invocation, so there is no separate in-memory
layer in the evaluation chain. A third "oneshot prefix set" layer would only
matter if one invocation needed to pre-approve a prefix for several later
calls, which the single-call approval flow does not require.

## Error handling

- A whole-file read failure (missing file) is fine: that layer is empty.
- A whole-file read failure for another reason (permissions, IO) is reported
  and the layer is treated as empty.
- A malformed line is reported and **skipped**; the rest of the file still
  loads. Malformed is never silently ignored.

## What is implemented today

The JSONL format, the `command`/`read`/`write` types, the required `allow` field,
the recursive path matching, and last-match-wins evaluation over
`[workspace] ++ [session]` are all in place. Rules are hand-edited; the only
programmatic writer is `attini approve --grant` (commands and reads), which
appends a single line and leaves hand-written comments intact.

All kinds of rule are enforced:

- A `command` rule decides whether a `command` call auto-runs, auto-denies, or
  falls through to the per-call approval flow.
- A `read` rule is consulted before a `read`/`list`/`search` runs. A winning
  `allow:true` rule (or the workspace/granted roots) lets it through; a winning
  `allow:false` rule parks the call as an approval request rather than a silent
  success or a plain error. See `docs/deferred/read-outside-workspace-approval.md`.
- A `write` rule is consulted by the patch tool's write guard, before the
  git-tracking heuristic. `allow:true` permits the write (even untracked);
  `allow:false` refuses it (even tracked). See
  `docs/design/write-permission-type.md`.

## History output

Every `command` evaluation records, in the `tool_approval` session record, not
just the final decision but **every rule that matched** during the walk, in
evaluation order, each marked as adopted or not (`AutoDecidedBy` / `matches`).
This lets a reader reconstruct which layer's rule produced the final answer.

## Related

- `docs/design/tool-call.md` -- how an approval-requiring call is parked and
  resumed.
- `docs/deferred/read-outside-workspace-approval.md` -- how a `read` denial
  (outside the workspace, or a winning `allow:false` rule) becomes an approval
  request.
- `docs/design/write-permission-type.md` -- the `write` type.
