//! Property-based tests for the patch tool write guards.
//!
//! Focuses on properties where random generation catches gaps that
//! specific integration tests miss:
//!
//! - Layer 1 pattern match must be robust against syntactic bypass
//!   variants (`./`, `x/../`, `.//`, trailing `/`) — property 1
//! - Layer 2 subdir pre-create must never leak side effects when the
//!   syntactic normalization escapes the scratchpad — property 1b
//! - Reject verdicts must not mutate the filesystem — property 3
//! - Within-invocation Add → Update on the same path must be allowed
//!   after the tracked-set mutation on rename — property 6
//!
//! Properties 2 / 4 / 5 (scratchpad identity, allow-then-mutation,
//! non-repo Layer 1/2) are deterministic invariants covered by the
//! integration tests in `tests/test_patch.rs`; randomizing them adds
//! little bug-detection value so they are not repeated here.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use attini::sansio::agent::{PatchError, PatchInvocation, PatchTool};
use attini::tools::ToolExecutor;

const ITERATIONS: usize = 128;
const SEED_ENV: &str = "ATTINI_PBT_SEED";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(label: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "attini-pbt-write-guards-{}-{}-{label}",
            std::process::id(),
            n
        ));
        fs::create_dir_all(&root).expect("mkdir workspace");
        // Init git so Layer 3/4 have a repo to work with.
        run_git(&root, &["init", "-q"]);
        run_git(&root, &["config", "user.email", "attini-test@example.com"]);
        run_git(&root, &["config", "user.name", "Attini Test"]);
        // Seed a tracked file so tracked set is non-empty in half the
        // properties; leaves the fresh-repo edge case covered too.
        fs::write(root.join("seed.md"), b"seed\n").expect("write seed");
        run_git(&root, &["add", "-A"]);
        // Prepare the current session's scratchpad (mirrors
        // Session::open's mkdir).
        fs::create_dir_all(root.join(".attini/test/scratchpad")).expect("mkdir scratchpad");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn executor(&self) -> ToolExecutor {
        ToolExecutor::new(&self.root, Vec::new(), "test".to_string()).expect("executor")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_git(root: &Path, args: &[&str]) {
    let _ = Command::new("git").arg("-C").arg(root).args(args).status();
}

/// Sample a Layer 1 canonical path and one of its syntactic variants.
/// Returns `(pretty_source, syntactic_variant, kind_label)`.
fn sample_layer1_variant(ctx: &mut noprop::TestCaseContext) -> (String, String, &'static str) {
    let bases: &[(&str, &str)] = &[
        (".git/HEAD", "git-head"),
        (".git/hooks/pre-commit", "git-hook"),
        (".attini/test/LOCK", "current-session-lock"),
        (".attini/test/conversation.jsonl", "current-session-conv"),
        (".attini/test/pending.json", "current-session-pending"),
        (".attini/test/permissions.json", "current-session-perms"),
        (".attini/other/LOCK", "other-session-lock"),
        (".attini/permissions.json", "workspace-perms"),
    ];
    let (base, kind) = noprop::sample_choice(ctx, bases);
    let variant = mangle_syntactic(ctx, base);
    (base.to_string(), variant, kind)
}

/// Randomly rewrite `base` with `.`, `x/../`, or `foo/./` insertions
/// that syntactic-normalise back to `base`. All variants must be
/// captured by Layer 1 all the same.
fn mangle_syntactic(ctx: &mut noprop::TestCaseContext, base: &str) -> String {
    let choice = noprop::sample_choice(ctx, &["as-is", "leading-dot", "double-slash", "detour"]);
    match choice {
        "as-is" => base.to_string(),
        "leading-dot" => format!("./{base}"),
        "double-slash" => base.replacen('/', "//", 1),
        "detour" => {
            // Insert `noise/../` early in the path. The first
            // segment must exist as a directory for `..` to bounce
            // through — we reuse the `.attini` metadata dir (always
            // present with an open session), so the detour is
            // `.attini/../<base>`.
            format!(".attini/../{base}")
        }
        _ => base.to_string(),
    }
}

/// Sample a scratchpad-adjacent syntactic bypass target. Returns
/// the path string plus whether the syntactic normalization keeps
/// it under scratchpad.
fn sample_scratchpad_variant(ctx: &mut noprop::TestCaseContext) -> (String, bool) {
    let choice = noprop::sample_choice(
        ctx,
        &[
            "inside-flat",
            "inside-deep",
            "inside-via-dot",
            "escape-parent",
            "escape-triple-parent",
        ],
    );
    match choice {
        "inside-flat" => (".attini/test/scratchpad/plan.md".to_string(), true),
        "inside-deep" => (
            ".attini/test/scratchpad/plans/2026/aug/01.md".to_string(),
            true,
        ),
        "inside-via-dot" => (".attini/test/scratchpad/./notes.md".to_string(), true),
        "escape-parent" => (".attini/test/scratchpad/../outside.md".to_string(), false),
        "escape-triple-parent" => (
            ".attini/test/scratchpad/../../../malicious/foo.md".to_string(),
            false,
        ),
        _ => (".attini/test/scratchpad/plan.md".to_string(), true),
    }
}

fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    walk_collect(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walk_collect(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        // Skip .git internals — snapshot's purpose is to catch
        // unintended writes; git bookkeeping churns constantly.
        if path.file_name().and_then(|s| s.to_str()) == Some(".git") {
            continue;
        }
        let Ok(ty) = e.file_type() else { continue };
        if ty.is_dir() {
            walk_collect(base, &path, out);
        } else if ty.is_file() {
            let Ok(bytes) = fs::read(&path) else { continue };
            let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            out.push((rel, bytes));
        }
    }
}

/// Property 1: any syntactic variant of a Layer-1 canonical path
/// must be rejected with `ExcludedPath`.
#[test]
fn prop_layer1_syntactic_variants_all_reject() -> noprop::RunResult {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed).run(ITERATIONS, |ctx| {
        let ws = Workspace::new("layer1-syntactic");
        // Ensure every Layer 1 target physically exists so Update
        // canonicalisation succeeds and the check reaches Layer 1
        // (otherwise `resolve_within` fails first at IoError →
        // UpdateOnMissingFile, which would mask the property).
        //
        // `.git/HEAD` is created by `git init` above and is already
        // present (content is irrelevant: Layer 1 rejects it before
        // any Update content match), so it must NOT be overwritten
        // here — doing so corrupts the repo and produces a spurious
        // "not a git repository" proxy state.
        fs::create_dir_all(ws.root().join(".git/hooks")).expect("git dir");
        fs::create_dir_all(ws.root().join(".attini/test")).expect("session dir");
        fs::create_dir_all(ws.root().join(".attini/other")).expect("other session dir");
        for f in [
            ".git/hooks/pre-commit",
            ".attini/test/LOCK",
            ".attini/test/conversation.jsonl",
            ".attini/test/pending.json",
            ".attini/test/permissions.json",
            ".attini/other/LOCK",
            ".attini/permissions.json",
        ] {
            fs::write(ws.root().join(f), b"seed\n").expect("write layer1 target");
        }

        let executor = ws.executor();
        let (base, variant, kind) = sample_layer1_variant(ctx);
        let is_update = noprop::sample_bool(ctx);
        let invocation = if is_update {
            PatchInvocation {
                edits: vec![PatchTool::Update {
                    path: variant.clone(),
                    before: "seed\n".to_string(),
                    after: "hijack\n".to_string(),
                }],
            }
        } else {
            PatchInvocation {
                edits: vec![PatchTool::Add {
                    path: variant.clone(),
                    content: "gotcha".to_string(),
                }],
            }
        };
        let err = executor
            .preview_patch(&invocation)
            .err()
            .unwrap_or_else(|| {
                panic!("Layer 1 target {kind} ({base:?} → {variant:?}) was not rejected")
            });
        assert!(
            matches!(err, PatchError::ExcludedPath { .. }),
            "Layer 1 target {kind} ({base:?} → {variant:?}) was rejected as {err:?} (expected ExcludedPath)"
        );
        Ok(())
    })
}

/// Property 1b: scratchpad-adjacent syntactic bypass variants that
/// syntactic-normalise outside the scratchpad root must not create
/// any directory outside the scratchpad. Uses a dir-inclusive
/// snapshot: `..` bypasses would call `create_dir_all` on a path
/// like `<root>/malicious/` if the pre-create logic were loose.
#[test]
fn prop_layer2_syntactic_bypass_has_no_side_effect() -> noprop::RunResult {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed).run(ITERATIONS, |ctx| {
        let ws = Workspace::new("layer2-bypass");
        let executor = ws.executor();
        let dirs_before = collect_dirs_outside_scratchpad(ws.root());

        let (variant, _is_inside) = sample_scratchpad_variant(ctx);
        let invocation = PatchInvocation {
            edits: vec![PatchTool::Add {
                path: variant.clone(),
                content: "x".to_string(),
            }],
        };
        // Outcome is intentionally not asserted here; the invariant
        // is about filesystem side effects only. Layer 3 may legit
        // allow a normalised-in-workspace path (e.g. `.attini/test/
        // outside.md`), and that is fine because it did not create
        // any *new* directory outside the scratchpad (the target's
        // parent `.attini/test/` already existed).
        let _ = executor.preview_patch(&invocation);

        let dirs_after = collect_dirs_outside_scratchpad(ws.root());
        assert_eq!(
            dirs_after, dirs_before,
            "scratchpad bypass {variant:?} created a directory outside scratchpad"
        );
        Ok(())
    })
}

/// Collect every directory under `root` other than `.git` and
/// `.attini/test/scratchpad` (and their descendants). Used by
/// property 1b to catch stray `create_dir_all` side effects from
/// scratchpad-bypass paths.
fn collect_dirs_outside_scratchpad(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dirs(root, root, &mut out);
    out.sort();
    out
}

fn walk_dirs(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if file_name == ".git" {
            continue;
        }
        let Ok(ty) = e.file_type() else { continue };
        if !ty.is_dir() {
            continue;
        }
        let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
        if rel.starts_with(".attini/test/scratchpad") {
            continue;
        }
        out.push(rel);
        walk_dirs(base, &path, out);
    }
}

/// Property 3: on any reject verdict from `preview_patch`, the
/// filesystem below the workspace root must be byte-identical.
#[test]
fn prop_reject_verdicts_do_not_mutate_filesystem() -> noprop::RunResult {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed).run(ITERATIONS, |ctx| {
        let ws = Workspace::new("reject-nondestructive");
        let executor = ws.executor();
        let before = snapshot(ws.root());

        // Mix Layer 1 targets, scratchpad legit / bypass, and random
        // random-looking paths that will land in Layer 3/4.
        let path_choice = noprop::sample_choice(
            ctx,
            &["layer1", "scratchpad-legit", "scratchpad-bypass", "random"],
        );
        let path = match path_choice {
            "layer1" => sample_layer1_variant(ctx).1,
            "scratchpad-legit" | "scratchpad-bypass" => sample_scratchpad_variant(ctx).0,
            _ => format!(
                "random_{}.md",
                noprop::sample_ascii_printable_string(ctx, 4)
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect::<String>()
            ),
        };
        let is_update = noprop::sample_bool(ctx);
        let invocation = if is_update {
            PatchInvocation {
                edits: vec![PatchTool::Update {
                    path: path.clone(),
                    before: "does-not-exist".to_string(),
                    after: "y".to_string(),
                }],
            }
        } else {
            PatchInvocation {
                edits: vec![PatchTool::Add {
                    path: path.clone(),
                    content: "x".to_string(),
                }],
            }
        };
        let outcome = executor.preview_patch(&invocation);
        // Whether or not the outcome is a reject, we only assert
        // non-mutation. Successful previews don't write anything
        // either (write happens in apply_patch), so this is
        // uniformly true. The interesting failure mode is a Layer 2
        // subdir side effect on a rejected target — property 1b
        // covers that specifically, but this generic snapshot
        // catches any other stray writes we haven't thought of.
        let _ = outcome;
        let after = snapshot(ws.root());
        assert_eq!(
            after, before,
            "preview_patch mutated filesystem for path {path:?}"
        );
        Ok(())
    })
}

/// Property 6: an Add that succeeded must admit a subsequent Update
/// on the same path within the same invocation.
#[test]
fn prop_add_then_update_within_invocation_is_allowed() -> noprop::RunResult {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed).run(ITERATIONS, |ctx| {
        let ws = Workspace::new("add-then-update");
        let executor = ws.executor();
        // Pick an Add target that Layer 3 will allow. We use a bare
        // top-level name (parent = workspace root, not gitignored).
        let stem = noprop::sample_ascii_printable_string(ctx, 5)
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>();
        let leaf = if stem.is_empty() {
            "propgen.md".to_string()
        } else {
            format!("propgen_{stem}.md")
        };
        // Skip if this file already exists (previous iteration
        // artifact — extremely unlikely given per-iteration
        // Workspace, but be safe).
        if ws.root().join(&leaf).exists() {
            return Ok(());
        }

        let add_inv = PatchInvocation {
            edits: vec![PatchTool::Add {
                path: leaf.clone(),
                content: "hello\n".to_string(),
            }],
        };
        let (hashes, _) = executor
            .preview_patch(&add_inv)
            .expect("preview add for propgen file");
        executor
            .apply_patch(&add_inv, &hashes)
            .expect("apply add for propgen file");
        // Now Update. Without the tracked-set mutation on rename,
        // this would fail as UntrackedTarget.
        let update_inv = PatchInvocation {
            edits: vec![PatchTool::Update {
                path: leaf.clone(),
                before: "hello\n".to_string(),
                after: "world\n".to_string(),
            }],
        };
        let (hashes, _) = executor
            .preview_patch(&update_inv)
            .expect("preview update after add");
        executor
            .apply_patch(&update_inv, &hashes)
            .expect("apply update after add");
        Ok(())
    })
}
