//! Integration tests for `attini::tools::ToolExecutor`.
//!
//! Each test builds a small workspace under the system temp dir, runs
//! the executor against it, and inspects the returned `ToolOutcome`.
//! No mocks or stubs — the executor touches the real filesystem.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use attini::sansio::agent::{
    DEFAULT_LIST_MAX_ENTRIES, DEFAULT_SEARCH_MAX_RESULTS, READ_MAX_BYTES, ReadOnlyTool,
    ToolExecutionError, ToolOutcome,
};
use attini::tools::ToolExecutor;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("attini-tools-{}-{}-{label}", std::process::id(), n));
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
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn executor(root: &TempRoot) -> ToolExecutor {
    ToolExecutor::new(root.path(), Vec::new(), "test".to_string()).expect("executor")
}

fn ok_body(outcome: ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Ok(s) => s,
        ToolOutcome::Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

fn err_kind(outcome: ToolOutcome) -> ToolExecutionError {
    match outcome {
        ToolOutcome::Ok(s) => panic!("expected Err, got Ok({s})"),
        ToolOutcome::Err(e) => e,
    }
}

// -----------------------------------------------------------------
// list
// -----------------------------------------------------------------

#[test]
fn list_flat_returns_entries_sorted_by_path() {
    let root = TempRoot::new("list-flat");
    root.write("b.txt", b"");
    root.write("a.txt", b"");
    root.write("nested/x.txt", b"");
    let body = ok_body(executor(&root).execute(ReadOnlyTool::List {
        path: ".".to_string(),
        recursive: false,
        max_entries: DEFAULT_LIST_MAX_ENTRIES,
        include_hidden: false,
    }));
    // Non-recursive: just top-level entries.
    assert!(body.contains(r#""path":"a.txt""#), "body was: {body}");
    assert!(body.contains(r#""path":"b.txt""#));
    assert!(body.contains(r#""path":"nested""#));
    assert!(body.contains(r#""kind":"dir""#));
    assert!(body.contains(r#""truncated":false"#));
}

#[test]
fn list_recursive_descends_into_directories() {
    let root = TempRoot::new("list-recursive");
    root.write("a.txt", b"a");
    root.write("sub/b.txt", b"bb");
    let body = ok_body(executor(&root).execute(ReadOnlyTool::List {
        path: ".".to_string(),
        recursive: true,
        max_entries: DEFAULT_LIST_MAX_ENTRIES,
        include_hidden: false,
    }));
    assert!(body.contains(r#""path":"sub/b.txt""#), "body was: {body}");
}

#[test]
fn list_hides_dotfiles_unless_include_hidden() {
    let root = TempRoot::new("list-hidden");
    root.write(".secret", b"");
    root.write("visible", b"");
    let visible = ok_body(executor(&root).execute(ReadOnlyTool::List {
        path: ".".to_string(),
        recursive: false,
        max_entries: DEFAULT_LIST_MAX_ENTRIES,
        include_hidden: false,
    }));
    assert!(!visible.contains(".secret"), "body was: {visible}");
    let with_hidden = ok_body(executor(&root).execute(ReadOnlyTool::List {
        path: ".".to_string(),
        recursive: false,
        max_entries: DEFAULT_LIST_MAX_ENTRIES,
        include_hidden: true,
    }));
    assert!(with_hidden.contains(".secret"), "body was: {with_hidden}");
}

#[test]
fn list_marks_truncated_when_max_entries_hit() {
    let root = TempRoot::new("list-trunc");
    for i in 0..10 {
        root.write(&format!("f{i:02}.txt"), b"");
    }
    let body = ok_body(executor(&root).execute(ReadOnlyTool::List {
        path: ".".to_string(),
        recursive: false,
        max_entries: 3,
        include_hidden: false,
    }));
    assert!(body.contains(r#""truncated":true"#), "body was: {body}");
    assert!(body.contains(r#""path":"f00.txt""#));
    assert!(!body.contains(r#""path":"f09.txt""#));
}

#[test]
fn list_absolute_path_is_rejected() {
    let root = TempRoot::new("list-abs");
    let err = err_kind(executor(&root).execute(ReadOnlyTool::List {
        path: "/etc".to_string(),
        recursive: false,
        max_entries: DEFAULT_LIST_MAX_ENTRIES,
        include_hidden: false,
    }));
    assert_eq!(err, ToolExecutionError::OutsideWorkspace);
}

#[test]
fn list_traversal_beyond_root_is_rejected() {
    let root = TempRoot::new("list-esc");
    let err = err_kind(executor(&root).execute(ReadOnlyTool::List {
        path: "../..".to_string(),
        recursive: false,
        max_entries: DEFAULT_LIST_MAX_ENTRIES,
        include_hidden: false,
    }));
    assert_eq!(err, ToolExecutionError::OutsideWorkspace);
}

// -----------------------------------------------------------------
// read
// -----------------------------------------------------------------

#[test]
fn read_whole_file_returns_content_and_line_bounds() {
    let root = TempRoot::new("read-whole");
    root.write("a.txt", b"first\nsecond\nthird\n");
    let body = ok_body(executor(&root).execute(ReadOnlyTool::Read {
        path: "a.txt".to_string(),
        line_range: None,
    }));
    assert!(
        body.contains(r#""content":"first\nsecond\nthird\n""#),
        "body: {body}"
    );
    assert!(body.contains(r#""start_line":1"#));
    assert!(body.contains(r#""end_line":3"#));
    assert!(body.contains(r#""truncated":false"#));
}

#[test]
fn read_with_line_range_returns_requested_slice() {
    let root = TempRoot::new("read-range");
    root.write("a.txt", b"one\ntwo\nthree\nfour\n");
    let body = ok_body(executor(&root).execute(ReadOnlyTool::Read {
        path: "a.txt".to_string(),
        line_range: Some((2, 3)),
    }));
    assert!(body.contains(r#""content":"two\nthree\n""#), "body: {body}");
    assert!(body.contains(r#""start_line":2"#));
    assert!(body.contains(r#""end_line":3"#));
}

#[test]
fn read_truncates_over_1_mib() {
    let root = TempRoot::new("read-trunc");
    let big = vec![b'a'; READ_MAX_BYTES + 100];
    root.write("big.txt", &big);
    let body = ok_body(executor(&root).execute(ReadOnlyTool::Read {
        path: "big.txt".to_string(),
        line_range: None,
    }));
    assert!(
        body.contains(r#""truncated":true"#),
        "body prefix: {}",
        &body[..80]
    );
}

#[test]
fn read_binary_file_returns_binary_error() {
    let root = TempRoot::new("read-binary");
    root.write("bin", &[0u8, 1, 2, 3]);
    let err = err_kind(executor(&root).execute(ReadOnlyTool::Read {
        path: "bin".to_string(),
        line_range: None,
    }));
    assert_eq!(err, ToolExecutionError::Binary);
}

#[test]
fn read_non_utf8_file_returns_not_utf8_error() {
    let root = TempRoot::new("read-non-utf8");
    // Invalid UTF-8 continuation byte, no NUL.
    root.write("bad.txt", &[0xC3, 0x28, b'\n']);
    let err = err_kind(executor(&root).execute(ReadOnlyTool::Read {
        path: "bad.txt".to_string(),
        line_range: None,
    }));
    assert_eq!(err, ToolExecutionError::NotUtf8);
}

#[test]
fn read_directory_returns_io_error() {
    let root = TempRoot::new("read-dir");
    root.write("sub/inside.txt", b"x");
    let err = err_kind(executor(&root).execute(ReadOnlyTool::Read {
        path: "sub".to_string(),
        line_range: None,
    }));
    assert!(matches!(err, ToolExecutionError::IoError(_)), "got {err:?}");
}

#[test]
fn read_missing_file_returns_io_error() {
    let root = TempRoot::new("read-missing");
    let err = err_kind(executor(&root).execute(ReadOnlyTool::Read {
        path: "does-not-exist.txt".to_string(),
        line_range: None,
    }));
    assert!(matches!(err, ToolExecutionError::IoError(_)), "got {err:?}");
}

#[test]
fn read_absolute_path_is_rejected() {
    let root = TempRoot::new("read-abs");
    let err = err_kind(executor(&root).execute(ReadOnlyTool::Read {
        path: "/etc/passwd".to_string(),
        line_range: None,
    }));
    assert_eq!(err, ToolExecutionError::OutsideWorkspace);
}

// -----------------------------------------------------------------
// search
// -----------------------------------------------------------------

#[test]
fn search_finds_literal_substring_case_insensitive_by_default() {
    let root = TempRoot::new("search-ci");
    root.write("a.txt", b"has a TODO here\nand nothing else\n");
    root.write("b.txt", b"todo lowercase\n");
    let body = ok_body(executor(&root).execute(ReadOnlyTool::Search {
        pattern: "todo".to_string(),
        path_prefix: None,
        case_sensitive: false,
        max_results: DEFAULT_SEARCH_MAX_RESULTS,
    }));
    assert!(body.contains(r#""path":"a.txt""#), "body: {body}");
    assert!(body.contains(r#""path":"b.txt""#));
    assert!(body.contains(r#""line":1"#));
}

#[test]
fn search_case_sensitive_respects_exact_case() {
    let root = TempRoot::new("search-cs");
    root.write("a.txt", b"todo\nTODO\n");
    let body = ok_body(executor(&root).execute(ReadOnlyTool::Search {
        pattern: "TODO".to_string(),
        path_prefix: None,
        case_sensitive: true,
        max_results: DEFAULT_SEARCH_MAX_RESULTS,
    }));
    // Only the second line matches.
    assert!(body.contains(r#""line":2"#), "body: {body}");
    // The first line should not appear as a match.
    assert!(!body.contains(r#""line":1"#));
}

#[test]
fn search_skips_binary_files_and_reports_count() {
    let root = TempRoot::new("search-skip-binary");
    root.write("code.txt", b"foo bar\n");
    root.write("blob.bin", &[b'f', b'o', b'o', 0, b'!']);
    let body = ok_body(executor(&root).execute(ReadOnlyTool::Search {
        pattern: "foo".to_string(),
        path_prefix: None,
        case_sensitive: false,
        max_results: DEFAULT_SEARCH_MAX_RESULTS,
    }));
    assert!(body.contains(r#""skipped_binary":1"#), "body: {body}");
    assert!(body.contains(r#""path":"code.txt""#));
    assert!(!body.contains(r#""path":"blob.bin""#));
}

#[test]
fn search_marks_truncated_when_max_results_hit() {
    let root = TempRoot::new("search-trunc");
    let mut text = String::new();
    for i in 0..10 {
        text.push_str(&format!("hit {i}\n"));
    }
    root.write("a.txt", text.as_bytes());
    let body = ok_body(executor(&root).execute(ReadOnlyTool::Search {
        pattern: "hit".to_string(),
        path_prefix: None,
        case_sensitive: false,
        max_results: 3,
    }));
    assert!(body.contains(r#""truncated":true"#), "body: {body}");
}

#[test]
fn search_empty_pattern_is_rejected() {
    let root = TempRoot::new("search-empty");
    let err = err_kind(executor(&root).execute(ReadOnlyTool::Search {
        pattern: String::new(),
        path_prefix: None,
        case_sensitive: false,
        max_results: DEFAULT_SEARCH_MAX_RESULTS,
    }));
    assert!(matches!(err, ToolExecutionError::ArgumentsParseFailed(_)));
}

#[test]
fn search_absolute_prefix_is_rejected() {
    let root = TempRoot::new("search-abs");
    let err = err_kind(executor(&root).execute(ReadOnlyTool::Search {
        pattern: "x".to_string(),
        path_prefix: Some("/etc".to_string()),
        case_sensitive: false,
        max_results: DEFAULT_SEARCH_MAX_RESULTS,
    }));
    assert_eq!(err, ToolExecutionError::OutsideWorkspace);
}

// -----------------------------------------------------------------
// extra_read_roots (0037)
// -----------------------------------------------------------------

/// Build an executor whose extra_read_roots list contains the given
/// TempRoots' canonical paths. Both roots must already exist.
fn executor_with_extras(root: &TempRoot, extras: &[&TempRoot]) -> ToolExecutor {
    let extras = extras
        .iter()
        .map(|r| r.path().canonicalize().expect("canonicalise extra"))
        .collect();
    ToolExecutor::new(root.path(), extras, "test".to_string()).expect("executor")
}

#[test]
fn read_accepts_absolute_path_inside_extra_read_root() {
    let root = TempRoot::new("extra-read-root-target");
    let extra = TempRoot::new("extra-read-root-source");
    extra.write("notes.md", b"external content\n");
    let target = extra.path().join("notes.md").canonicalize().expect("canon");
    let body = ok_body(
        executor_with_extras(&root, &[&extra]).execute(ReadOnlyTool::Read {
            path: target.to_string_lossy().into_owned(),
            line_range: None,
        }),
    );
    assert!(body.contains("external content"));
}

#[test]
fn read_rejects_absolute_path_when_no_extra_read_root_covers_it() {
    let root = TempRoot::new("no-extras-abs");
    root.write("inside.md", b"hi\n");
    let err = err_kind(executor(&root).execute(ReadOnlyTool::Read {
        path: "/etc/hosts".to_string(),
        line_range: None,
    }));
    assert_eq!(err, ToolExecutionError::OutsideWorkspace);
}

#[test]
fn list_absolute_extra_read_root_returns_absolute_entry_paths() {
    // list results for extra_read_root subtrees use absolute paths
    // (strip_prefix is only tried against workspace root). Model
    // then feeds them back to read/search as-is.
    let root = TempRoot::new("list-abs-fallback");
    let extra = TempRoot::new("list-abs-source");
    extra.write("a.txt", b"a");
    let body = ok_body(
        executor_with_extras(&root, &[&extra]).execute(ReadOnlyTool::List {
            path: extra.path().to_string_lossy().into_owned(),
            recursive: false,
            max_entries: DEFAULT_LIST_MAX_ENTRIES,
            include_hidden: false,
        }),
    );
    let expected = extra.path().canonicalize().expect("canon").join("a.txt");
    assert!(
        body.contains(&expected.to_string_lossy().into_owned()),
        "body did not contain absolute path {}: {body}",
        expected.display()
    );
}

#[test]
fn workspace_relative_paths_still_resolve_when_extras_are_present() {
    let root = TempRoot::new("relative-under-extras");
    let extra = TempRoot::new("relative-under-extras-extra");
    root.write("inside.md", b"workspace content\n");
    let body = ok_body(
        executor_with_extras(&root, &[&extra]).execute(ReadOnlyTool::Read {
            path: "inside.md".to_string(),
            line_range: None,
        }),
    );
    assert!(body.contains("workspace content"));
}

#[test]
fn relative_path_is_never_joined_against_extra_read_root() {
    // A relative "notes.md" must not resolve against extra roots
    // even if the file exists there. Only workspace_root.join(rel)
    // is tried, so this must return an IO error, not the extra
    // file's content.
    let root = TempRoot::new("relative-safety-root");
    let extra = TempRoot::new("relative-safety-extra");
    extra.write("notes.md", b"external\n");
    let outcome = executor_with_extras(&root, &[&extra]).execute(ReadOnlyTool::Read {
        path: "notes.md".to_string(),
        line_range: None,
    });
    let err = err_kind(outcome);
    // "notes.md" does not exist under workspace_root, so canonicalize
    // returns an IO error. The point is it did not silently pick up
    // extra/notes.md.
    assert!(matches!(err, ToolExecutionError::IoError(_)));
}
