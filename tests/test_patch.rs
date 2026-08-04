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
    ToolExecutor::new(root.path()).expect("executor")
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
fn preview_add_produces_hash_none_and_line_stats() {
    let root = TempRoot::new("preview-add");
    let (hashes, preview) = exec(&root)
        .preview_patch(&inv(vec![add("new.txt", "one\ntwo\n")]))
        .expect("preview ok");
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0].path, "new.txt");
    assert_eq!(hashes[0].sha256, None);
    assert_eq!(preview.added_lines, 2);
    assert_eq!(preview.removed_lines, 0);
    assert_eq!(preview.edit_count, 1);
    assert_eq!(preview.target_paths, vec!["new.txt".to_string()]);
}

#[test]
fn preview_update_returns_sha256_of_existing_file() {
    let root = TempRoot::new("preview-update");
    root.write("a.txt", b"hello\nworld\n");
    let (hashes, preview) = exec(&root)
        .preview_patch(&inv(vec![update("a.txt", "world", "rust")]))
        .expect("preview ok");
    assert!(hashes[0].sha256.is_some());
    assert_eq!(preview.removed_lines, 1);
    assert_eq!(preview.added_lines, 1);
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
