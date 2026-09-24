# Default auto-approval for provably safe read-only commands

**Status:** Design settled; `git` auto-approval implemented
(`src/sansio/safe_command.rs`). The rule machinery it builds on is
implemented (`docs/design/permissions-file.md`,
`docs/design/read-approval.md`). The plain readers (`cat`, `grep`, `rg`,
...) are not yet on the allow-list — `safe_read_only` returns `NotSafe`
for every non-`git` program.

**Decisions so far**

- **On by default.** No opt-in flag; the behaviour fires for every
  no-rule `command` call. (A config knob that only *disables* it may be added
  later if an unexpected auto-approval needs to be auditable; it is not part
  of the first cut.)
- **Start with `git`.** It is the highest-usage family in development, so the
  `git` subcommand/flag analysis is built first; other readers (`cat`, `grep`,
  `rg`, ...) follow once the shape is proven. The argv analysis depth is
  settled by implementing it, not up front.
- **Reuse the `read` roots** for path checking, and reuse the existing
  canonicalisation (symlink handling included) so `cat link -> /etc/passwd` is
  caught.
- **A built-in approval records its own scope** (`scope: "builtin"`, distinct
  stderr/history wording) so it never looks like a human-written rule.

## The problem

The default per-tool-call flow (`Authorization::PerTool`) parks **every**
`command` call that matches no rule and asks the human. That is the right
default for anything that can change state, but it also parks the many calls
that are plainly harmless: `git status`, `git diff`, `grep`, `cat`, `ls`,
`rg`, `head`, `wc`, ... A session that spends most of its time reading pays a
human round-trip for each one, even though the model is only looking.

Today the remedy is a hand-written `args_prefix` rule
(`{'type':'command','allow':true,'args_prefix':['git','status']}`), one per
command family. That is explicit and correct, but it is boilerplate the human
re-types in every new workspace, and it is easy to write a rule that is *wider
than intended* (see "The `-C` / `--git-dir` trap", below).

## The idea

When a `command` call matches **no** rule, do not immediately park it. First
check it against a small, built-in **allow-list of commands that provably
cannot read outside their granted roots and cannot write, network, or spawn**.
If (and only if) the call is *for sure* in that class, auto-approve it exactly
as a built-in `allow` rule would, recording the same `tool_approval` decision.
Anything the check cannot prove safe stays `Pending` and parks as today.

This is a **default behaviour**, not a rule: it fires only when the human has
written no rule for that command. A hand-written rule always wins (it is
evaluated first; the built-in check is the fallthrough for `Pending`), so a
human can still deny or widen any particular command family.

The model is still told the truth in `CommandInvocation::definition`: the
description changes from "every call requires approval" to "read-only calls
your permissions cover may run without approval; anything else requires it".

## Design principles this must respect

The existing rule system already encodes the guarantees we need; this proposal
should not weaken them:

- **A read deny cannot be enforced** (`docs/design/permissions-file.md`,
  `read-approval.md`). The model can read any file it can *name* through
  `command` (`cat`, `grep`, ...). So the built-in allow-list must **never**
  let a command read a file outside the workspace (or outside a `read`-granted
  root) — otherwise it would silently create a read bypass with no rule to
  gate it. This is the single hardest constraint and it is why the check must
  be *conservative*, not *plausible*.
- **`args_prefix` is deliberately coarse** and its known weakness is the same
  one we must avoid here: a prefix matches anything that *starts with* the
  same tokens. `git branch -D foo` starts with `git branch`; a naive allow of
  `git` would also approve it. The built-in check must inspect the *arguments*,
  not just argv[0].
- **Attini's advice for `bash -c`** already says a shell string is opaque and
  must be approval-gated. The allow-list must not be a back door around that:
  `bash -c`, `sh -c`, `env`, `xargs`, ... are never auto-approved.

## What makes a command "provably safe"

A command is a candidate only if **all** of the following hold. The check is
intentionally hostile-input-shaped: if any part is not certain, it is not safe.

1. **Named, allow-listed program.** argv[0] (after the `PATH` resolution the
   OS will do, matched by basename) is one of a small fixed set. Anything not
   on the list — including `bash`, `sh`, `env`, `xargs`, `find` (with `-exec`),
   `tee`, `>`, editors, package managers, `git` subcommands other than the
   read ones — is not a candidate.
2. **No shell, no redirection, no chaining.** These are already absent by
   construction: `command` execs argv[0] directly with no shell
   (`CommandInvocation::argv`), so `|`, `>`, `&&`, `;`, globs are *literal
   arguments* the program receives, not operators. The check must not
   accidentally treat such an argument as harmless — but it also must not
   assume the program will ignore it (see per-command rules below).
3. **Path arguments stay inside the workspace (or a granted read root).**
   Every argument that the command would treat as a path must be canonicalised
   and checked against the same roots `read` uses. Arguments that are not paths
   (subcommand names, flags, literally-evaluated patterns) are handled
   per-command.
4. **No write, network, or subprocess.** The allow-listed commands are chosen
   so that (in the invocation shape the check accepts) they cannot write a
   file, open a socket, or spawn a process. This is a property of the specific
   command *and flag set*, not of the program in general.

Only when all four hold do we approve.

### The `-C` / `--git-dir` trap (the motivating example)

`git` is the clearest case that *looks* safe and is not:

- `git -C ../elsewhere status` runs git against a **different working tree**,
  reading files outside our workspace. A naive "allow `git status`" (which
  matches argv `[git, status, ...]`) would only catch the non-`-C` form
  anyway, but a *global* "allow `git`" would let the model point git anywhere.
- `git --git-dir=... branch -D foo` deletes a branch — a **write** to a
  different repository — while still starting with `git`.
- `git branch -D` / `-d` / `-m` / `--delete` are mutations; `git branch -a` / a
  bare `git branch` are reads. The subcommand alone is not enough; the flags
  matter.

So `git` needs the most careful handling. The safe subset is something like:

- `git status`, `git diff` (without `-C`/`--git-dir`/`--work-tree`), `git log`,
  `git show`, `git branch` **only** with no `-D`/`-d`/`-m`/`--delete`/`-M`,
  `git ls-files`, `git ls-tree`, `git rev-parse`, `git remote -v`, ... — and
  **never** with a `-C`/`--git-dir`/`--work-tree` that escapes the workspace,
  and **never** with an `--output`/`-o` that writes.
- Every argument that looks like a path (`--file=`, a bare rev that resolves to
  a path, a `<path>` positional) must resolve inside the workspace or be a
  ref/pattern, not a filesystem path.

Because this is subtle, the conservative default is: **if a `git` invocation
carries any option the check does not positively recognise as read-only, it is
not safe.** Unknown flags fail closed.

### `grep` / `cat` and "do not read ungranted files"

The same rule applies to the plain readers:

- `cat FILE` / `head FILE` / `tail FILE` / `wc FILE` / `grep PATTERN FILE...`
  auto-approve only when **every** file argument canonicalises inside the
  workspace (or a granted read root). `cat ../secret` or `grep x /etc/passwd`
  stay `Pending` — this is the direct analogue of `read-approval.md`'s
  "outside the workspace -> park".
- A reader with **no** file argument that reads stdin (`cat`, `grep`, `sort`,
  ...) is safe only if it cannot be handed a here-string/redirect — and by
  construction it cannot, since there is no shell. `cat` with no args reads
  the (empty, non-interactive) stdin; that is safe, but the simplest rule is
  to require at least one in-workspace path and skip the no-arg form.
- `grep -r DIR` / `rg` / `find`: recursive walks stay inside the workspace
  when the root is inside it. `grep -r` outside is parked. `find` is *not* on
  the base list because `-exec`/`-ok`/`-delete`/`-fprint` turn it into a
  write/exec; only if those are absent *and* the path is inside could it be
  considered — the cheap answer is to exclude `find` entirely at first.
- `rg` with `--files-with-matches`, `-l`, etc. are fine; `rg --pre`/`--hostname-bin`
  spawn a subprocess and must fail closed. Again: unknown flag -> not safe.

### What is *not* on the list (and why)

- **`bash` / `sh` / `env` / `xargs` / `find -exec` / `sort -o` / `sed -i` /
  `awk` with `system()` / any `>` target**: they either execute arbitrary
  strings or write files. Never auto-approved; they keep today's prompt.
- **Anything that opens the network**: `curl`, `wget`, `git fetch`/`clone`/
  `push`/`pull`, `ssh`, `nc`. Excluded.
- **Anything path-scoped outside the workspace**: handled by rule 3 above for
  the readers that *are* allowed; the program itself is fine, the invocation
  is not.

## Where it hooks in

It splits into a pure half and an I/O half — no new rule type, no new
file format:

- **Pure** (`src/sansio/safe_command.rs`): `safe_read_only(argv) ->
  Safety`. It inspects the argv only, decides whether the program +
  subcommand + flag set is provably read-only, and returns any path
  operands the caller must resolve (`Safety::Safe { paths }`). It never
  touches the filesystem, so it stays in `sansio`. Every non-`git`
  program is `NotSafe` for now.
- **I/O** (`src/tell_cli.rs`): in the `Judgment::Pending` arm of
  `dispatch_command`, call the pure check; if it says safe, canonicalise
  each returned path and require it to resolve inside the workspace or
  a granted read root (the roots `read` already uses). Only then build an
  `AutoDecision` with `scope: RuleScope::BuiltIn`, no rule matches, and
  take the same path as `Judgment::AutoApprove`.

The built-in decision records `RuleScope::BuiltIn` (rendered as
`scope: "builtin"` in the `tool_approval` history / stderr), so a reader
can always tell a built-in auto-approval apart from a human rule. The
stderr line is `[command] auto-approve (built-in read-only): <cmd>`.

## Candidate allow-list (first cut)

Deliberately small. Every program listed must be shown to satisfy rules 1–4
above in the invocation shapes the check accepts.

Only the `git` row is implemented so far; the rest are the intended
first cut and `safe_read_only` returns `NotSafe` for them today.

| Program | Safe shapes | Fail-closed on |
|---|---|---|
| `git` **(implemented)** | `status`, `diff`, `log`, `show`, `ls-files`, `ls-tree`, `rev-parse`, `describe`, `shortlog`, `blame`; `branch` only with pure-listing flags (`-a`/`-r`/`-v`/`--list`/`--show-current`/...); `remote`/`remote -v`/`remote show`/`remote get-url`; only tokens after a literal `--` are path-checked | any leading global option (`-C`, `--git-dir`, `--work-tree`, `-c`, `--exec-path`, `--namespace`); `branch -D/-d/-m/-M/--delete/--move/...` or any positional; `remote add/remove/prune/...`; any unknown flag; an outside/non-existent path |
| `ls`, `cat`, `head`, `tail`, `wc`, `file`, `stat`, `nl` | in-workspace path args | any outside path; no arg (reads stdin) |
| `grep`, `rg` | in-workspace path/`-r` root; pattern | outside path; `--pre`/`--hostname-bin`; unknown flag |
| `diff`, `cmp` | in-workspace paths | outside path |
| `pwd`, `whoami`, `date`, `echo` | no path semantics | (echo only; not a file write) |

`find`, `sed`, `awk`, `sort`, `uniq`, `tr`, `cut`, `env`, `bash`, `sh`,
`xargs`, `tee`, `cp`, `mv`, `rm`, `mkdir`, `touch`, `chmod`, `curl`, `wget`,
and every package/build tool are **absent** in the first cut; they may be added
later only with a per-command argument analysis, never a bare prefix.

## Settled and open points

Settled:

1. **On by default** (see Decisions, above).
2. **Reuse the executor's roots** (`ToolExecutor::root` + `extra_read_roots`)
   so a `read` grant also unlocks the auto-approved readers on that path;
   otherwise the built-in approval and the `read` tool would disagree about
   what is reachable.
3. **Canonicalise paths and handle symlinks** (`cat link` where
   `link -> /etc/passwd` must park; a non-existent target must not spuriously
   approve). Same machinery `read`-approval already uses.
4. **Distinct wording.** The existing auto-approve line is
   `[command] auto-approve via <scope> rule '<prefix>': <cmd>`. A built-in
   approval gets its own honest line, e.g.
   `[command] auto-approve (built-in read-only): <cmd>`, so it never looks like
   a human wrote a rule.

Still open (settled by implementing):

5. **How much argv analysis is enough?** The `git` table is the test case: the
   more flags we enumerate, the more likely we miss one. The design stance is
   "recognise a fixed safe flag vocabulary, fail closed on anything else"
   rather than "blocklist dangerous flags"; the exact vocabulary is filled in
   as the `git` implementation lands.
6. **Interaction with the tool-batching note.** Nothing structural changes;
   a built-in approval is still an auto-executed call, so the existing
   ordering rules apply unchanged.

## Why draft this before implementing

The attraction is a real ergonomics win, but the safety argument is subtle and
the failure mode is bad: a built-in auto-approval that is *almost* safe reads
a file the human meant to keep out (the read-deny problem), or silently lets
`git branch -D` delete something (the `-C`/flag problem). Nailing the shape —
which programs, which flags, fail-closed stance, where it records itself — is
the whole design. The code after that is small.

## Implementation order

1. ~~**`git`** (highest usage in development): land the safe
   subcommand/flag vocabulary and the fail-closed handling of
   `-C`/`--git-dir`/`--work-tree`.~~ **Done**
   (`src/sansio/safe_command.rs`). `-C` is deliberately out of scope for
   now: any leading global option fails closed (parks), rather than
   being resolved against the roots.
2. **Plain readers**: `cat`/`head`/`tail`/`wc`/`grep`/`rg` with the
   in-workspace-path rule.
3. **Remaining first-cut set**: `ls`/`diff`/`cmp`/`pwd`/`date`/... as needed.
4. **Config disable knob**, only if the auditability need materialises.
5. **`-C` handling** (later): resolve `git -C <dir>` against the roots
   instead of failing closed.

## Related

- `docs/design/permissions-file.md` -- the rule format and last-match-wins.
- `docs/design/read-approval.md` -- why a read deny cannot be enforced (the
  constraint that forces this check to be conservative).
- `docs/design/tool-call.md` -- how a parked call is resumed.
- `docs/design/intentionally-not-supported.md` -- item 7 records the removal
  of the old `readonly`/`network` rule attributes, a *prior* attempt at
  "run some commands unattended"; this proposal must argue why a built-in
  allow-list is different (no mode, no attribute, fail-closed by default).
