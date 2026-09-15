# The `write` permission type

**Status:** Implemented. The `write` rule type is part of the permissions file
described in `docs/design/permissions-file.md`, alongside `command` and `read`.

## The idea

`write` expresses patch/write access the same way `read` expresses read
access: a declarative rule in `permissions.jsonl`.

```jsonl
# let the model write under src/ without prompting
{'type':'write','allow':true,'path':'src'}
# never touch generated output
{'type':'write','allow':false,'path':'dist'}
```

`write` reuses the same shape as `read`: `type` + `allow` + `path`, with the
same **recursive, segment-boundary** matching (see "Path matching" in
`docs/design/permissions-file.md`).

## Where it is consulted

A `write` rule is evaluated inside the patch tool's write guard
(`ToolExecutor::check_patch_write`, `src/tools.rs`), **before** the git-tracking
heuristic. The guard order is:

1. **Working-tree status** -- the write guard is only reached once a target
   is resolved. A target that is *outside the workspace* skips Layers 1-2 and
   the git heuristic (they are workspace-relative concepts) and goes straight
   to step 3, using the **absolute canonical path** for rule matching (outside
   `write` rules are written as absolute paths, mirroring `read`). With no
   matching rule it parks for one-shot approval.
2. **Layer 1** (in-workspace targets only) -- hardcoded reject of
   runtime-critical paths (`.git/**`,
   `.attini/*/{LOCK,conversation.jsonl,pending.json,permissions.jsonl}`,
   `.attini/permissions.jsonl`). A `write` rule cannot override this.
3. **Layer 2** -- this session's scratchpad always writes without a prompt.
   A `write` rule is not reached here.
4. **`write` rules** -- last-match-wins over `[workspace] ++ [session]`:
   - a winning `allow:true` rule permits the write, *including* an untracked
     `Update`, an `Add`, a target outside the workspace, and even a non-git
     workspace;
   - a winning `allow:false` rule refuses the write, *including* a tracked
     `Update` that the heuristic would otherwise allow.
5. **Git-tracking heuristic** (in-workspace targets only) -- only when no
   `write` rule matches: a tracked `Update` is auto-approved; an untracked
   `Update` and any `Add` in a git repo are writable but require approval (the
   preview carries a `not_revertible` reason); a gitignored parent refuses the
   `Add` outright (`IgnoredParent`); and a non-git workspace makes every
   non-scratchpad write require approval. None of these are hard errors except
   `IgnoredParent`.

So a `write` rule *narrows or widens* patch access. Inside the workspace it
goes through Layers 1-2 and the git heuristic; outside, an absolute-path rule
is the only thing that auto-approves a write (otherwise the human is asked).
Layer 1 still gates in-workspace writes, but an outside target is never a
runtime-critical path and is not blocked by it.

## Approval vs. writability

There are two independent decisions for a patch:

- **Writability** -- enforced by the executor's Layer 3 (`write` rules, then
  git tracking). The only *hard* rejections left are `ExcludedPath` (Layer 1
  protected path or a `write` deny rule) and `IgnoredParent` (Add into a
  gitignored region). An untracked `Update`, a non-git workspace, or a plain
  `Add` are all *writable*; they just require approval.
- **Whether to prompt** -- `preview_patch` returns `auto_approve = true` only
  when every edit is git-tracked or covered by a winning `allow:true` `write`
  rule. Otherwise the preview is parked for approval and carries a
  `not_revertible` reason (rendered as a `NOTE:` line in the preview) so the
  human knows `git checkout` cannot undo it.

A `write` `allow:true` rule makes a non-tracked path both writable and
auto-approved. A `write` `allow:false` rule makes a tracked path neither
writable nor auto-approved.

## Relationship to `patch-grant-scope.md`

This is the declarative file-rule shape. `docs/design/patch-grant-scope.md`
records the imperative `--grant` analogue scoped to patches. They are two shapes
of the same idea; both are implemented.

## Not in scope

- **Create vs. update distinction.** A `write` rule applies to both `Add` and
  `Update` alike (the "middle ground": `allow:true` under `src` may create a new
  file). Distinguishing them is not implemented.
- **Delete.** The patch tool has no delete edit, so there is nothing to govern.
- **Leaving the workspace.** Both absolute paths and relative paths that
  escape with `..` are accepted (mirroring `read`). With no matching rule the
  write is parked for one-shot approval; `attini approve --grant` persists an
  absolute-path `write` rule. See `docs/design/patch-grant-scope.md` for the
  imperative side, and `docs/design/read-approval.md` for the shared
  approve/grant shape.
