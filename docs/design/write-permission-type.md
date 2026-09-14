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

1. **Layer 1** -- hardcoded reject of runtime-critical paths
   (`.git/**`, `.attini/*/{LOCK,conversation.jsonl,pending.json,permissions.jsonl}`,
   `.attini/permissions.jsonl`). A `write` rule cannot override this.
2. **Layer 2** -- this session's scratchpad always writes without a prompt.
   A `write` rule is not reached here.
3. **`write` rules** -- last-match-wins over `[workspace] ++ [session]`:
   - a winning `allow:true` rule permits the write, *including* an untracked
     `Update` or an `Add`, and even inside a non-git workspace;
   - a winning `allow:false` rule refuses the write, *including* a tracked
     `Update` that the heuristic would otherwise allow.
4. **Git-tracking heuristic** -- only when no `write` rule matches:
   tracked `Update` allowed, untracked `Update` refused (`UntrackedTarget`),
   `Add` refused when the parent is gitignored, and a non-git workspace refuses
   everything (`NotInGitRepo`).

So a `write` rule *narrows or widens within the workspace*; it never escapes
the workspace boundary (Layer 0 / `resolve_within`) or overrides Layer 1.

## Approval vs. writability

There are two independent decisions for a patch:

- **Writability** -- enforced by the executor's Layer 3 (`write` rules, then
  git tracking). If the path is not writable at all, `preview_patch` returns an
error (`ExcludedPath` for a deny rule, `UntrackedTarget` etc. otherwise).
- **Whether to prompt** -- the dispatch layer (`dispatch_patch_unapproved`)
  auto-applies only when every edit is either git-tracked or covered by a
  winning `allow:true` `write` rule; a `allow:false` rule forces approval.

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
- **Leaving the workspace.** Layer 0 / `resolve_within` is not overridable.
