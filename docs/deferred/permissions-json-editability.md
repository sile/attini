# `permissions.json` readability for humans (deferred)

**Status:** Deferred. Not implemented. This memo records usability ideas about the
on-disk shape of `permissions.json`.

## Problem

`permissions.json` is the file humans edit to grant standing permissions (auto-approve
rules and extra read paths). Today it is written as a **single compact JSON line** by
`write_permissions_file` (`src/permissions.rs:407`), which serialises through
`Json(permissions).to_string()` — `nojson`'s `DisplayJson` emits compact JSON with no
pretty-printing.

A file with a few rules quickly becomes a wall of text on one line, e.g.:

```json
{"command_prefixes":[{"argv_prefix":["cargo","test"],"decision":"approve"},{"argv_prefix":["cargo","build"],"decision":"approve"}],"extra_read_paths":["../docs/","/opt/shared"]}
```

That is hard to read and hard to edit by hand: `argv_prefix` is an array, so a rule's
meaning is buried in the middle of a long line. It is also hard to review: a whole file's
change shows up as one line in `git diff`, so a review cannot see *which* rule changed.

The intent is the opposite: **the human should edit this file easily**, rather than
route every small change through a CLI subcommand (`attini session grant` /
`grant-read` / `approve --grant`). The file is the source of truth; the commands are a
convenience.

## Candidate improvements

These are independent; each addresses a different aspect of "hard to edit by hand."

### A. Pretty-print the output

Emit indented, multi-line JSON so each rule is on its own line and `git diff` shows a
per-rule change. Straightforward: wrap the existing `DisplayJson` output with indentation
(either a pretty formatter or a small post-serialise pass). Keeps the schema as a single
JSON object.

### B. Support JSONC (comments)

Accept `//` and `/* */` comments on read so a human can annotate rules ("this was granted
for the release build", "TODO: narrow this"). This means the reader
(`read_permissions_file`, `src/permissions.rs:313`) must tolerate comments, and writes
must decide whether to preserve them (hard) or drop them (simple, but then comments vanish
on the next grant — surprising).

### C. Switch the format to JSONL

Replace the single top-level object with **one JSON value per line**, i.e.
newline-delimited JSON (JSONL). Each rule (and each read path) is its own line, so:

- a rule never spans lines, and no line grows with the number of rules — this is the
  key property: unlike pretty-printed JSON, adding a rule adds a line rather than
  extending an existing one, so arrays never become "tall";
- `git diff` is naturally per-line, so a change is one added/removed line;
- comments are not needed to annotate: a line is a self-contained rule, and a plain
  comment line can be added by the human if the reader skips non-rule lines;
- for the CLI writer, appending a rule is an append, which is cheap and diff-friendly.

A sketch (one entry per line):

```jsonl
{"argv_prefix":["cargo","test"],"decision":"approve"}
{"argv_prefix":["cargo","build"],"decision":"approve"}
{"read_path":"../docs/"}
```

Open questions for C:

- **How to distinguish rule kinds.** The current object form has two named fields
  (`command_prefixes`, `extra_read_paths`). In JSONL there is no wrapping object, so each
  line needs a discriminator: either a `kind` field (`{"kind":"command_prefix", ...}`),
  or distinct shapes (`{"read_path": ...}` vs. `{"argv_prefix": ...}`) that the reader
  tells apart by which key is present. The latter reads better but needs a clear rule.
- **Migration / coexistence.** The reader already accepts the legacy top-level array and
  the object form. JSONL is a third accepted shape; decide whether to auto-migrate on
  write (like the legacy array does) or keep the formats separate permanently.
- **Field order within a line.** A single line still has an internal order; keep it
  stable so equivalent rules diff identically.
- **Failure granularity.** JSONL parses line by line, so a malformed line can be skipped
  with a warning (like the current per-entry skip) while the rest still loads. This is
  arguably better than one bad character failing the whole file.

## Comparison

| | A. Pretty-print | B. JSONC | C. JSONL |
|---|---|---|---|
| Per-rule diff in `git diff` | Better (rule on its own line) | Better | Best (one rule = one line) |
| Line grows with rule count | No, but arrays can become "tall" | No | **No** (adding a rule adds a line) |
| Human comments | No | Yes, but round-trip? | Via skipped comment lines (if the reader allows) |
| Schema churn | None | None | New top-level shape (third format) |
| Round-trip risk | Low | High (comments) | Low, but discriminator design needed |

C is the most aligned with "don't make the line or the array grow" — arrays like
`argv_prefix` stay short, and the file grows only by adding lines. The cost is that
JSONL drops the named top-level fields, so the reader needs a per-line discriminator and
a documented third accepted shape.

## Tension to resolve

- **Round-trip loses formatting.** The writer goes through the `Permissions` struct, so
  any comments or hand-formatting are lost on the next `grant` — exactly when the human
  just edited the file. This is the main risk for B and, to a lesser degree, for A; C
  sidesteps it for rule content but still drops hand-added blank lines or ordering on
  rewrite.
- **Don't over-invest.** A formatter that reflows the whole file on every write produces
  noisy diffs too. Stable field order and one element per line matter more than arbitrary
  formatting.
- **A `comment` field instead of JSONC.** If rules were self-describing (an explicit
  `comment` field on a rule), JSONC would not be needed and comments would survive
  round-trips as data. But a comment line in JSONL is simpler if the reader just skips
  non-rule lines.

## If revived

- Decide between the three directions first; they are not mutually exclusive, but each
  changes the file contract (A: format only, B: input grammar, C: top-level structure).
- If C is chosen: design the per-line discriminator, decide auto-migration vs. permanent
  coexistence, and pick a stable intra-line field order. Start with the reader accepting
  all three shapes (current object, legacy array, JSONL) and the writer emitting JSONL.
- If A is chosen: pick the exact style (indent width, one `argv_prefix` element per line,
  trailing newline) and confirm `git diff` stays readable.
- If B is chosen: settle "comments preserved" vs. "comments dropped on write" *before*
  implementing, and document it.
- Keep the reader tolerant of the current compact form and the legacy top-level array so
  existing files keep loading either way.
