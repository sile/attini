//! Integration tests for `attini::tools::ToolExecutor` patch flow.
//!
//! Each test builds a scratch workspace under the system temp dir,
//! then exercises preview_patch + apply_patch against real files.
//! Filesystem mocks are not used.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use attini::sansio::agent::{PatchError, PatchInvocation, PatchTool};
use attini::tools::ToolExecutor;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("attini-patch-{}-{}-{label}", std::process::id(), n));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, rel: &str, contents: &[u8]) {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&full, contents).expect("write file");
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.path.join(rel)).expect("read file")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn exec(root: &TempRoot) -> ToolExecutor {
    // Patch tool is now gated by git-tracking checks. Initialise the
    // TempRoot as a git repo and `git add -A` any pre-existing files
    // so Layer 3 sees them as tracked; anything the test writes after
    // this call is Add-time under a non-ignored parent, which Layer 3
    // also allows.
    git_init_and_add_all(root.path());
    ToolExecutor::new(root.path(), Vec::new(), "test".to_string()).expect("executor")
}

fn git_init_and_add_all(root: &Path) {
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["init", "-q"])
        .status();
    // Set a local identity so `git add` does not fail even if the
    // environment lacks a global user.email / user.name.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "user.email", "attini-test@example.com"])
        .status();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "user.name", "Attini Test"])
        .status();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["add", "-A"])
        .status();
}

fn add(path: &str, content: &str) -> PatchTool {
    PatchTool::Add {
        path: path.to_string(),
        content: content.to_string(),
    }
}

fn update(path: &str, before: &str, after: &str) -> PatchTool {
    PatchTool::Update {
        path: path.to_string(),
        before: before.to_string(),
        after: after.to_string(),
    }
}

fn inv(edits: Vec<PatchTool>) -> PatchInvocation {
    PatchInvocation { edits }
}

// -----------------------------------------------------------------
// preview_patch
// -----------------------------------------------------------------

#[test]
fn preview_add_produces_content_none_and_line_stats() {
    let root = TempRoot::new("preview-add");
    let (snapshots, preview) = exec(&root)
        .preview_patch(&inv(vec![add("new.txt", "one\ntwo\n")]))
        .expect("preview ok");
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].path, "new.txt");
    assert_eq!(snapshots[0].content, None);
    assert_eq!(preview.added_lines, 2);
    assert_eq!(preview.removed_lines, 0);
    assert_eq!(preview.edit_count, 1);
    assert_eq!(preview.target_paths, vec!["new.txt".to_string()]);
}

#[test]
fn preview_update_returns_content_of_existing_file() {
    let root = TempRoot::new("preview-update");
    root.write("a.txt", b"hello\nworld\n");
    let (snapshots, preview) = exec(&root)
        .preview_patch(&inv(vec![update("a.txt", "world", "rust")]))
        .expect("preview ok");
    assert_eq!(
        snapshots[0].content.as_deref(),
        Some(b"hello\nworld\n".as_slice())
    );
    assert_eq!(preview.removed_lines, 1);
    assert_eq!(preview.added_lines, 1);
}

#[test]
fn preview_update_on_tracked_file_marks_auto_approve() {
    let root = TempRoot::new("preview-auto-approve-tracked");
    root.write("a.txt", b"hello\nworld\n");
    let (_, preview) = exec(&root)
        .preview_patch(&inv(vec![update("a.txt", "world", "rust")]))
        .expect("preview ok");
    assert!(preview.auto_approve, "tracked update should auto-approve");
}

#[test]
fn preview_add_marks_not_auto_approve() {
    let root = TempRoot::new("preview-auto-approve-add");
    let (_, preview) = exec(&root)
        .preview_patch(&inv(vec![add("new.txt", "one\ntwo\n")]))
        .expect("preview ok");
    assert!(!preview.auto_approve, "add must require approval");
}

#[test]
fn preview_mixed_tracked_update_and_add_marks_not_auto_approve() {
    let root = TempRoot::new("preview-auto-approve-mixed");
    root.write("a.txt", b"hello\n");
    let (_, preview) = exec(&root)
        .preview_patch(&inv(vec![
            update("a.txt", "hello", "hi"),
            add("new.txt", "x\n"),
        ]))
        .expect("preview ok");
    assert!(
        !preview.auto_approve,
        "a patch containing an add must require approval"
    );
}

#[test]
fn preview_update_rejects_no_match() {
    let root = TempRoot::new("preview-nomatch");
    root.write("a.txt", b"hello");
    let err = exec(&root)
        .preview_patch(&inv(vec![update("a.txt", "missing", "x")]))
        .expect_err("no match rejected");
    assert!(matches!(err, PatchError::NoMatch { .. }));
}

#[test]
fn preview_update_rejects_ambiguous_match() {
    let root = TempRoot::new("preview-ambig");
    root.write("a.txt", b"foofoo");
    let err = exec(&root)
        .preview_patch(&inv(vec![update("a.txt", "foo", "bar")]))
        .expect_err("ambiguous rejected");
    match err {
        PatchError::AmbiguousMatch { match_count, .. } => assert_eq!(match_count, 2),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn preview_add_on_existing_rejects() {
    let root = TempRoot::new("preview-add-exists");
    root.write("a.txt", b"existing");
    let err = exec(&root)
        .preview_patch(&inv(vec![add("a.txt", "new")]))
        .expect_err("already exists rejected");
    assert!(matches!(err, PatchError::AddOnExistingFile { .. }));
}

#[test]
fn preview_update_on_missing_rejects() {
    let root = TempRoot::new("preview-update-missing");
    let err = exec(&root)
        .preview_patch(&inv(vec![update("nope.txt", "a", "b")]))
        .expect_err("missing rejected");
    assert!(matches!(err, PatchError::UpdateOnMissingFile { .. }));
}

#[test]
fn preview_outside_workspace_rejects() {
    let root = TempRoot::new("preview-outside");
    let err = exec(&root)
        .preview_patch(&inv(vec![add("../escape.txt", "x")]))
        .expect_err("outside rejected");
    assert!(matches!(err, PatchError::OutsideWorkspace { .. }));
}

// -----------------------------------------------------------------
// apply_patch (happy paths)
// -----------------------------------------------------------------

#[test]
fn apply_add_writes_new_file() {
    let root = TempRoot::new("apply-add");
    let invocation = inv(vec![add("greet.txt", "hi\n")]);
    let (hashes, _) = exec(&root).preview_patch(&invocation).unwrap();
    let applied = exec(&root)
        .apply_patch(&invocation, &hashes)
        .expect("apply ok");
    assert_eq!(applied.len(), 1);
    assert_eq!(root.read("greet.txt"), b"hi\n");
}

#[test]
fn apply_update_replaces_unique_substring() {
    let root = TempRoot::new("apply-update");
    root.write("a.txt", b"prefix TODO suffix\n");
    let invocation = inv(vec![update("a.txt", "TODO", "DONE")]);
    let (hashes, _) = exec(&root).preview_patch(&invocation).unwrap();
    exec(&root)
        .apply_patch(&invocation, &hashes)
        .expect("apply");
    assert_eq!(root.read("a.txt"), b"prefix DONE suffix\n");
}

#[test]
fn apply_multiple_edits_across_files() {
    let root = TempRoot::new("apply-multi");
    root.write("a.txt", b"aa");
    let invocation = inv(vec![add("b.txt", "bb"), update("a.txt", "aa", "AA")]);
    let (hashes, _) = exec(&root).preview_patch(&invocation).unwrap();
    exec(&root)
        .apply_patch(&invocation, &hashes)
        .expect("apply");
    assert_eq!(root.read("a.txt"), b"AA");
    assert_eq!(root.read("b.txt"), b"bb");
}

// -----------------------------------------------------------------
// apply_patch (failure paths)
// -----------------------------------------------------------------

#[test]
fn apply_detects_conflict_when_file_changed_after_preview() {
    let root = TempRoot::new("apply-conflict");
    root.write("a.txt", b"orig\n");
    let invocation = inv(vec![update("a.txt", "orig", "new")]);
    let (hashes, _) = exec(&root).preview_patch(&invocation).unwrap();
    // Simulate a concurrent write between preview and apply.
    root.write("a.txt", b"tampered\n");
    let err = exec(&root)
        .apply_patch(&invocation, &hashes)
        .expect_err("conflict");
    assert!(matches!(err, PatchError::Conflict { .. }));
    // Original tampered file is untouched.
    assert_eq!(root.read("a.txt"), b"tampered\n");
}

#[test]
fn apply_atomically_rejects_all_when_one_edit_fails() {
    let root = TempRoot::new("apply-atomic");
    root.write("a.txt", b"orig");
    // First edit is fine; second update targets a file that vanished.
    let invocation = inv(vec![add("new.txt", "hi"), update("gone.txt", "x", "y")]);
    // preview_patch itself fails because gone.txt doesn't exist;
    // to test the phase-1 rollback of tmp files, arrange the failure
    // to happen inside apply_patch after preview succeeded:
    // remove the target between preview and apply.
    root.write("gone.txt", b"x");
    let (hashes, _) = exec(&root).preview_patch(&invocation).unwrap();
    // Delete it so apply's re-read fails.
    fs::remove_file(root.path().join("gone.txt")).unwrap();
    let err = exec(&root)
        .apply_patch(&invocation, &hashes)
        .expect_err("second edit fails");
    assert!(matches!(err, PatchError::UpdateOnMissingFile { .. }));
    // Even though the first (add) edit's tmp file was written, the
    // target new.txt must not exist because phase 1 aborted before
    // any rename.
    assert!(!root.path().join("new.txt").exists());
    // The tmp file must also be cleaned up.
    let leftovers: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("attini-tmp"))
        .collect();
    assert!(leftovers.is_empty(), "tmp files leaked: {leftovers:?}");
}

#[test]
fn apply_add_on_existing_rejects_at_phase1() {
    let root = TempRoot::new("apply-add-race");
    let invocation = inv(vec![add("racy.txt", "hi")]);
    let (hashes, _) = exec(&root).preview_patch(&invocation).unwrap();
    // Someone else created the file between preview and apply.
    root.write("racy.txt", b"other");
    let err = exec(&root)
        .apply_patch(&invocation, &hashes)
        .expect_err("add on existing");
    assert!(matches!(err, PatchError::AddOnExistingFile { .. }));
    assert_eq!(root.read("racy.txt"), b"other");
}

// -----------------------------------------------------------------
// patch tool must ignore extra_read_roots
// -----------------------------------------------------------------

#[test]
fn patch_add_absolute_path_inside_extra_read_root_is_rejected() {
    // extra_read_roots grant read-only access. Patch (write) must
    // NOT resolve them — write to an absolute path outside the
    // workspace has to fail even if the target lives under an
    // extra_read_root.
    let root = TempRoot::new("patch-abs-extra-root");
    let extra = TempRoot::new("patch-abs-extra-source");
    let extra_canon = extra.path().canonicalize().expect("canon");
    let executor = ToolExecutor::new(root.path(), vec![extra_canon.clone()], "test".to_string())
        .expect("executor with extras");
    let target = extra
        .path()
        .join("hijack.md")
        .to_string_lossy()
        .into_owned();
    let invocation = inv(vec![add(&target, "gotcha")]);
    let err = executor
        .preview_patch(&invocation)
        .expect_err("absolute add must be rejected");
    assert!(
        matches!(err, PatchError::OutsideWorkspace { .. }),
        "expected OutsideWorkspace, got {err:?}"
    );
    assert!(
        !extra.path().join("hijack.md").exists(),
        "patch must not have created the file"
    );
}

#[test]
fn patch_update_relative_traversal_into_extra_read_root_is_rejected() {
    // A relative path with `..` that lands under an extra_read_root
    // must also be rejected for write. resolve_within stays
    // single-root; extra_read_roots do not open a write path.
    let root = TempRoot::new("patch-traversal-workspace");
    let extra_parent = TempRoot::new("patch-traversal-parent");
    extra_parent.write("victim.md", b"original\n");
    let extra_canon = extra_parent.path().canonicalize().expect("canon");
    let executor =
        ToolExecutor::new(root.path(), vec![extra_canon], "test".to_string()).expect("executor");
    // Construct a workspace-relative path that resolves outside root.
    let workspace_canon = root.path().canonicalize().expect("workspace canon");
    let rel_to_victim = pathdiff_naive(&workspace_canon, &extra_parent.path().join("victim.md"));
    let invocation = inv(vec![update(&rel_to_victim, "original\n", "hijacked\n")]);
    let err = executor
        .preview_patch(&invocation)
        .expect_err("traversal update must be rejected");
    assert!(
        matches!(err, PatchError::OutsideWorkspace { .. }),
        "expected OutsideWorkspace, got {err:?}"
    );
    assert_eq!(extra_parent.read("victim.md"), b"original\n");
}

// -----------------------------------------------------------------
// patch write guards (Layer 1-4)
// -----------------------------------------------------------------

/// Build an executor whose workspace is initialised as a git repo,
/// with pre-existing files at `initial_tracked` staged so they enter
/// the tracked set. Both a session-local scratchpad (`test` session,
/// mirroring the test helper above) and any `.attini/{other}/` dirs
/// requested by the caller are created after `git add -A` so they
/// stay untracked (matches the runtime shape where `.attini/` is
/// gitignored).
fn exec_with_git(root: &TempRoot, other_sessions: &[&str]) -> ToolExecutor {
    git_init_and_add_all(root.path());
    // Create scratchpad (SessionPaths equivalent) after git init.
    fs::create_dir_all(root.path().join(".attini/test/scratchpad")).expect("mkdir scratchpad");
    for other in other_sessions {
        fs::create_dir_all(root.path().join(format!(".attini/{other}"))).expect("mkdir other");
    }
    ToolExecutor::new(root.path(), Vec::new(), "test".to_string()).expect("executor")
}

#[test]
fn layer1_rejects_git_metadata_add_bypasses_gitignore_check() {
    // Regression for F1: `.git/hooks/new-hook` is not gitignored
    // (git manages the .git dir specially), so Layer 3's
    // check-ignore says "not ignored" and would allow Add. Layer 1
    // must catch it first.
    let root = TempRoot::new("layer1-git-add");
    let executor = exec_with_git(&root, &[]);
    fs::create_dir_all(root.path().join(".git/hooks")).expect("mkdir hooks");
    let err = executor
        .preview_patch(&inv(vec![add(".git/hooks/pre-commit", "#!/bin/sh")]))
        .expect_err("reject");
    assert!(
        matches!(err, PatchError::ExcludedPath { .. }),
        "got {err:?}"
    );
}

#[test]
fn layer1_rejects_syntactic_bypass_variants_of_git_metadata() {
    // `./.git/HEAD` and `x/../.git/HEAD` canonicalise to the same
    // workspace-relative path as `.git/HEAD`. Layer 1 matches on
    // canonical form and must reject all three.
    let root = TempRoot::new("layer1-syntactic");
    // `.git/HEAD` is created by `git init` inside `exec_with_git`,
    // so do NOT write it here: the Update's `before` is irrelevant
    // because Layer 1 rejects before any content match, and writing
    // git's HEAD corrupts the repo (spurious "not a git
    // repository").
    // `noise-dir/` is a real directory so `..` can traverse through
    // the intermediate component (a file would make canonicalise
    // fail with ENOTDIR → UpdateOnMissingFile).
    fs::create_dir_all(root.path().join("noise-dir")).expect("mkdir noise-dir");
    let executor = exec_with_git(&root, &[]);
    for variant in [".git/HEAD", "./.git/HEAD", "noise-dir/../.git/HEAD"] {
        let err = executor
            .preview_patch(&inv(vec![update(variant, "ref\n", "hijack\n")]))
            .expect_err("reject");
        assert!(
            matches!(err, PatchError::ExcludedPath { .. }),
            "variant {variant:?} did not hit Layer 1: {err:?}"
        );
    }
}

#[test]
fn layer1_rejects_other_session_runtime_state() {
    // I1: `main` session's agent must not touch `work` session's
    // LOCK / conversation.jsonl even if the target somehow becomes
    // git-tracked.
    let root = TempRoot::new("layer1-other-session");
    root.write(".attini/work/LOCK", b"{\"pid\":123}");
    let executor = exec_with_git(&root, &[]);
    let err = executor
        .preview_patch(&inv(vec![update(
            ".attini/work/LOCK",
            "{\"pid\":123}",
            "x",
        )]))
        .expect_err("reject");
    assert!(
        matches!(err, PatchError::ExcludedPath { .. }),
        "got {err:?}"
    );
}

#[test]
fn layer1_rejects_workspace_permissions() {
    let root = TempRoot::new("layer1-workspace-scoped");
    root.write(".attini/permissions.json", b"{\"command_prefixes\":[]}");
    let executor = exec_with_git(&root, &[]);
    let err = executor
        .preview_patch(&inv(vec![update(
            ".attini/permissions.json",
            "{\"command_prefixes\":[]}",
            "hijack",
        )]))
        .expect_err(".attini/permissions.json");
    assert!(matches!(err, PatchError::ExcludedPath { .. }), "{err:?}");
}

#[test]
fn layer2_allows_scratchpad_add_with_subdir_auto_create() {
    let root = TempRoot::new("layer2-scratchpad-subdir");
    let executor = exec_with_git(&root, &[]);
    executor
        .preview_patch(&inv(vec![add(
            ".attini/test/scratchpad/plans/2026-08-05.md",
            "# plan\n",
        )]))
        .expect("scratchpad subdir Add ok");
}

#[test]
fn layer2_syntactic_bypass_does_not_leak_side_effect_outside_scratchpad() {
    // Regression for N2-I1: `.attini/test/scratchpad/../../../malicious/foo.md`
    // syntactically normalises to `malicious/foo.md`, so the Layer 2
    // pre-create MUST NOT run (no `malicious/` directory created)
    // and the Add falls through to Layer 3.
    let root = TempRoot::new("layer2-syntactic-bypass");
    let executor = exec_with_git(&root, &[]);
    let outcome = executor.preview_patch(&inv(vec![add(
        ".attini/test/scratchpad/../../../malicious/foo.md",
        "gotcha",
    )]));
    let err = outcome.expect_err("bypass must be rejected");
    // Either OutsideWorkspace or NotInGitRepo / IgnoredParent — the
    // point is the side effect must not have created a directory.
    assert!(
        !root.path().join("malicious").exists(),
        "Layer 2 pre-create leaked outside scratchpad ({err:?})"
    );
}

#[test]
fn layer2_other_session_scratchpad_is_not_layer2() {
    // Layer 2 covers only the current session's scratchpad. Other
    // session's scratchpad falls through to Layer 3 (which sees the
    // path as untracked → reject on Update, gitignored parent →
    // reject on Add if `.attini/` is gitignored, else allow on Add).
    let root = TempRoot::new("layer2-other-session-scratchpad");
    fs::create_dir_all(root.path().join(".attini/other/scratchpad"))
        .expect("mkdir other scratchpad");
    let executor = exec_with_git(&root, &["other"]);
    root.write(".attini/other/scratchpad/notes.md", b"prev\n");
    let err = executor
        .preview_patch(&inv(vec![update(
            ".attini/other/scratchpad/notes.md",
            "prev\n",
            "hijack",
        )]))
        .expect_err("other session scratchpad must not be Layer 2");
    // File was created after git add, so tracked set does not
    // contain it → UntrackedTarget for Update.
    assert!(
        matches!(err, PatchError::UntrackedTarget { .. }),
        "got {err:?}"
    );
}

#[test]
fn layer3_update_rejects_untracked_file() {
    let root = TempRoot::new("layer3-untracked");
    let executor = exec_with_git(&root, &[]);
    // Create AFTER git init → untracked.
    root.write("untracked.md", b"before\n");
    let err = executor
        .preview_patch(&inv(vec![update("untracked.md", "before\n", "after\n")]))
        .expect_err("reject");
    assert!(
        matches!(err, PatchError::UntrackedTarget { .. }),
        "got {err:?}"
    );
}

#[test]
fn layer3_update_allows_tracked_file() {
    let root = TempRoot::new("layer3-tracked");
    root.write("tracked.md", b"before\n");
    let executor = exec_with_git(&root, &[]); // git add -A picks up tracked.md
    executor
        .preview_patch(&inv(vec![update("tracked.md", "before\n", "after\n")]))
        .expect("tracked Update ok");
}

#[test]
fn layer3_add_rejects_when_parent_is_gitignored() {
    let root = TempRoot::new("layer3-ignored-parent");
    root.write(".gitignore", b"ignored/\n");
    root.write("ignored/keep", b"placeholder"); // ensure dir exists
    let executor = exec_with_git(&root, &[]);
    let err = executor
        .preview_patch(&inv(vec![add("ignored/new.md", "x")]))
        .expect_err("reject");
    assert!(
        matches!(err, PatchError::IgnoredParent { .. }),
        "got {err:?}"
    );
}

#[test]
fn layer3_in_invocation_add_then_update_is_allowed() {
    // After a successful Add, the target enters the tracked set for
    // the rest of the invocation so a follow-up Update on the same
    // path is not rejected as untracked. Verified by doing an Add
    // via apply_patch (Session::open would not be called in this
    // test but the tracked set mutation is done in apply_patch).
    let root = TempRoot::new("layer3-add-then-update");
    let executor = exec_with_git(&root, &[]);
    let add_inv = inv(vec![add("new.md", "hello\n")]);
    let (hashes, _) = executor.preview_patch(&add_inv).expect("preview add");
    executor.apply_patch(&add_inv, &hashes).expect("apply add");
    // Now Update the just-added file. Would fail as UntrackedTarget
    // if the tracked set were only initialised at startup.
    let update_inv = inv(vec![update("new.md", "hello\n", "world\n")]);
    let (hashes, _) = executor.preview_patch(&update_inv).expect("preview update");
    executor
        .apply_patch(&update_inv, &hashes)
        .expect("apply update");
    assert_eq!(root.read("new.md"), b"world\n");
}

#[test]
fn layer4_not_in_git_repo_rejects_layer3_writes_but_allows_scratchpad() {
    // No git init here — Layer 4 fallback kicks in.
    let root = TempRoot::new("layer4-not-a-repo");
    fs::create_dir_all(root.path().join(".attini/test/scratchpad")).expect("mkdir scratchpad");
    let executor =
        ToolExecutor::new(root.path(), Vec::new(), "test".to_string()).expect("executor");
    // Any non-scratchpad Update / Add is rejected.
    root.write("outside.md", b"pre\n");
    let err = executor
        .preview_patch(&inv(vec![update("outside.md", "pre\n", "post\n")]))
        .expect_err("reject");
    assert!(
        matches!(err, PatchError::NotInGitRepo { .. }),
        "got {err:?}"
    );
    // Scratchpad still writeable (Layer 2 is git-independent).
    executor
        .preview_patch(&inv(vec![add(".attini/test/scratchpad/note.md", "hi")]))
        .expect("scratchpad ok even without git");
}

/// Best-effort relative path constructor for the traversal test.
/// Not for production use — assumes both inputs live under the same
/// system temp dir so a simple `..` chain suffices.
fn pathdiff_naive(from: &Path, to: &Path) -> String {
    let from_components: Vec<_> = from.components().collect();
    let to_components: Vec<_> = to.components().collect();
    let common = from_components
        .iter()
        .zip(to_components.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = String::new();
    for _ in common..from_components.len() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str("..");
    }
    for c in &to_components[common..] {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&c.as_os_str().to_string_lossy());
    }
    out
}
