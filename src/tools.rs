//! Read-only workspace tool executor.
//!
//! Runs the [`ReadOnlyTool`] invocations the model emits, enforcing
//! the workspace boundary and the per-tool resource limits declared
//! in [`crate::sansio::agent`]. All filesystem I/O happens here; the
//! Sans I/O core is fed the resulting [`ToolOutcome`] and cannot
//! observe the executor's internal state.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use nojson::{DisplayJson, Json, JsonFormatter};

use crate::sansio::agent::{
    DEFAULT_LIST_MAX_ENTRIES, DEFAULT_SEARCH_MAX_RESULTS, READ_MAX_BYTES, ReadOnlyTool,
    ToolExecutionError, ToolOutcome,
};

/// Workspace-scoped executor for [`ReadOnlyTool`] invocations.
#[derive(Debug, Clone)]
pub struct ToolExecutor {
    root: PathBuf,
}

impl ToolExecutor {
    /// Create an executor rooted at `root`. The root is canonicalised
    /// so later boundary checks compare against a stable prefix; any
    /// entry whose canonical path is not a descendant of the root is
    /// rejected as [`ToolExecutionError::OutsideWorkspace`].
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().canonicalize()?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn execute(&self, tool: ReadOnlyTool) -> ToolOutcome {
        match tool {
            ReadOnlyTool::List {
                path,
                recursive,
                max_entries,
                include_hidden,
            } => self.execute_list(&path, recursive, max_entries, include_hidden),
            ReadOnlyTool::Read { path, line_range } => self.execute_read(&path, line_range),
            ReadOnlyTool::Search {
                pattern,
                path_prefix,
                case_sensitive,
                max_results,
            } => self.execute_search(
                &pattern,
                path_prefix.as_deref(),
                case_sensitive,
                max_results,
            ),
        }
    }

    fn execute_list(
        &self,
        rel_path: &str,
        recursive: bool,
        max_entries: usize,
        include_hidden: bool,
    ) -> ToolOutcome {
        let dir = match resolve_within(&self.root, rel_path) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::Err(e),
        };
        let cap = max_entries.min(DEFAULT_LIST_MAX_ENTRIES);
        let mut entries: Vec<ListEntry> = Vec::new();
        let mut truncated = false;
        let outcome = walk_list(
            &dir,
            &self.root,
            recursive,
            include_hidden,
            cap,
            &mut entries,
        );
        match outcome {
            WalkOutcome::Ok => {}
            WalkOutcome::Truncated => truncated = true,
            WalkOutcome::Err(e) => return ToolOutcome::Err(e),
        }
        ToolOutcome::Ok(
            Json(ListResult {
                entries: &entries,
                truncated,
            })
            .to_string(),
        )
    }

    fn execute_read(&self, rel_path: &str, line_range: Option<(usize, usize)>) -> ToolOutcome {
        let path = match resolve_within(&self.root, rel_path) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::Err(e),
        };
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return ToolOutcome::Err(ToolExecutionError::IoError(e.to_string())),
        };
        if metadata.is_dir() {
            return ToolOutcome::Err(ToolExecutionError::IoError(format!(
                "{rel_path}: is a directory"
            )));
        }
        let mut file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => return ToolOutcome::Err(ToolExecutionError::IoError(e.to_string())),
        };
        let mut buf = vec![0u8; READ_MAX_BYTES + 1];
        let read = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => return ToolOutcome::Err(ToolExecutionError::IoError(e.to_string())),
        };
        let mut truncated = read > READ_MAX_BYTES;
        let effective = read.min(READ_MAX_BYTES);
        buf.truncate(effective);
        if buf.contains(&0u8) {
            return ToolOutcome::Err(ToolExecutionError::Binary);
        }
        let text = match String::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => return ToolOutcome::Err(ToolExecutionError::NotUtf8),
        };
        let (slice, start_line, end_line) = match line_range {
            None => (text.as_str(), 1usize, text.lines().count().max(1)),
            Some((start, end)) => {
                if start == 0 || end < start {
                    return ToolOutcome::Err(ToolExecutionError::ArgumentsParseFailed(format!(
                        "invalid line_range: [{start}, {end}]"
                    )));
                }
                let (from_byte, to_byte, actual_end) = resolve_line_range(&text, start, end);
                (&text[from_byte..to_byte], start, actual_end)
            }
        };
        // If line_range excluded content past our read window, keep
        // truncated=true so the model knows more bytes existed on disk.
        if line_range.is_some() && !truncated && read == READ_MAX_BYTES {
            // Tie-break: read == cap could mean either "exactly fit"
            // or "cap reached with more available"; be conservative.
            truncated = true;
        }
        ToolOutcome::Ok(
            Json(ReadResult {
                content: slice,
                start_line,
                end_line,
                truncated,
            })
            .to_string(),
        )
    }

    fn execute_search(
        &self,
        pattern: &str,
        path_prefix: Option<&str>,
        case_sensitive: bool,
        max_results: usize,
    ) -> ToolOutcome {
        if pattern.is_empty() {
            return ToolOutcome::Err(ToolExecutionError::ArgumentsParseFailed(
                "pattern must not be empty".to_string(),
            ));
        }
        let base = match path_prefix {
            Some(p) => match resolve_within(&self.root, p) {
                Ok(p) => p,
                Err(e) => return ToolOutcome::Err(e),
            },
            None => self.root.clone(),
        };
        let cap = max_results.min(DEFAULT_SEARCH_MAX_RESULTS);
        let needle = if case_sensitive {
            pattern.to_string()
        } else {
            pattern.to_lowercase()
        };
        let mut matches: Vec<SearchMatch> = Vec::new();
        let mut skipped_binary: u64 = 0;
        let truncated = walk_search(
            &base,
            &self.root,
            &needle,
            case_sensitive,
            cap,
            &mut matches,
            &mut skipped_binary,
        );
        ToolOutcome::Ok(
            Json(SearchResult {
                matches: &matches,
                truncated,
                skipped_binary,
            })
            .to_string(),
        )
    }
}

fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf, ToolExecutionError> {
    // Reject absolute paths and paths that syntactically escape (`..`
    // beyond root) before touching the filesystem; canonicalize on
    // absolute paths would silently jump out of the workspace.
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(ToolExecutionError::OutsideWorkspace);
    }
    let joined = root.join(rel_path);
    let canon = joined
        .canonicalize()
        .map_err(|e| ToolExecutionError::IoError(format!("{rel}: {e}")))?;
    if !canon.starts_with(root) {
        return Err(ToolExecutionError::OutsideWorkspace);
    }
    Ok(canon)
}

enum WalkOutcome {
    Ok,
    Truncated,
    Err(ToolExecutionError),
}

fn walk_list(
    dir: &Path,
    root: &Path,
    recursive: bool,
    include_hidden: bool,
    cap: usize,
    out: &mut Vec<ListEntry>,
) -> WalkOutcome {
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let read = match fs::read_dir(&current) {
            Ok(r) => r,
            Err(e) => return WalkOutcome::Err(ToolExecutionError::IoError(e.to_string())),
        };
        let mut children: Vec<(PathBuf, fs::FileType, u64)> = Vec::new();
        for entry in read {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => return WalkOutcome::Err(ToolExecutionError::IoError(e.to_string())),
            };
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !include_hidden && name_str.starts_with('.') {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(e) => return WalkOutcome::Err(ToolExecutionError::IoError(e.to_string())),
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            children.push((entry.path(), file_type, size));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, file_type, size) in children {
            let kind = if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            out.push(ListEntry {
                path: rel,
                kind,
                size,
            });
            if out.len() >= cap {
                return WalkOutcome::Truncated;
            }
            if recursive && file_type.is_dir() {
                stack.push(path);
            }
        }
    }
    WalkOutcome::Ok
}

fn walk_search(
    base: &Path,
    root: &Path,
    needle: &str,
    case_sensitive: bool,
    cap: usize,
    out: &mut Vec<SearchMatch>,
    skipped_binary: &mut u64,
) -> bool {
    let mut stack: Vec<PathBuf> = vec![base.to_path_buf()];
    while let Some(current) = stack.pop() {
        let meta = match fs::symlink_metadata(&current) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_file() {
            match scan_file(
                &current,
                root,
                needle,
                case_sensitive,
                cap,
                out,
                skipped_binary,
            ) {
                ScanOutcome::Truncated => return true,
                ScanOutcome::Continue => {}
            }
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        let read = match fs::read_dir(&current) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut children: Vec<PathBuf> = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            children.push(entry.path());
        }
        children.sort();
        // Push in reverse so alphabetical order pops first.
        for path in children.into_iter().rev() {
            stack.push(path);
        }
    }
    false
}

enum ScanOutcome {
    Continue,
    Truncated,
}

fn scan_file(
    path: &Path,
    root: &Path,
    needle: &str,
    case_sensitive: bool,
    cap: usize,
    out: &mut Vec<SearchMatch>,
    skipped_binary: &mut u64,
) -> ScanOutcome {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return ScanOutcome::Continue,
    };
    if bytes.contains(&0u8) {
        *skipped_binary += 1;
        return ScanOutcome::Continue;
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => {
            *skipped_binary += 1;
            return ScanOutcome::Continue;
        }
    };
    let rel = path
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    for (line_idx, line) in text.lines().enumerate() {
        let hit = if case_sensitive {
            line.contains(needle)
        } else {
            line.to_lowercase().contains(needle)
        };
        if hit {
            out.push(SearchMatch {
                path: rel.clone(),
                line: (line_idx as u64) + 1,
                snippet: line.chars().take(200).collect(),
            });
            if out.len() >= cap {
                return ScanOutcome::Truncated;
            }
        }
    }
    ScanOutcome::Continue
}

fn resolve_line_range(text: &str, start: usize, end: usize) -> (usize, usize, usize) {
    let mut cursor = 0usize;
    let mut from_byte = None;
    let mut to_byte = text.len();
    let mut actual_end = 0usize;
    for (idx, line) in text.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        if line_no == start {
            from_byte = Some(cursor);
        }
        if line_no >= start && line_no <= end {
            actual_end = line_no;
        }
        if line_no == end {
            to_byte = cursor + line.len();
        }
        cursor += line.len();
    }
    let from = from_byte.unwrap_or(text.len());
    if actual_end == 0 {
        actual_end = start.saturating_sub(1);
    }
    (from, to_byte.min(text.len()), actual_end)
}

// -----------------------------------------------------------------
// JSON payloads
// -----------------------------------------------------------------

struct ListResult<'a> {
    entries: &'a [ListEntry],
    truncated: bool,
}

impl DisplayJson for ListResult<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("entries", self.entries)?;
            f.member("truncated", self.truncated)
        })
    }
}

struct ListEntry {
    path: String,
    kind: &'static str,
    size: u64,
}

impl DisplayJson for ListEntry {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("path", &self.path)?;
            f.member("kind", self.kind)?;
            f.member("size", self.size)
        })
    }
}

struct ReadResult<'a> {
    content: &'a str,
    start_line: usize,
    end_line: usize,
    truncated: bool,
}

impl DisplayJson for ReadResult<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("content", self.content)?;
            f.member("start_line", self.start_line)?;
            f.member("end_line", self.end_line)?;
            f.member("truncated", self.truncated)
        })
    }
}

struct SearchResult<'a> {
    matches: &'a [SearchMatch],
    truncated: bool,
    skipped_binary: u64,
}

impl DisplayJson for SearchResult<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("matches", self.matches)?;
            f.member("truncated", self.truncated)?;
            f.member("skipped_binary", self.skipped_binary)
        })
    }
}

struct SearchMatch {
    path: String,
    line: u64,
    snippet: String,
}

impl DisplayJson for SearchMatch {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("path", &self.path)?;
            f.member("line", self.line)?;
            f.member("snippet", &self.snippet)
        })
    }
}
