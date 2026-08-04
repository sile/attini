//! Workspace tool executor.
//!
//! Runs the [`ReadOnlyTool`] and [`PatchInvocation`] tool calls the
//! model emits, enforcing the workspace boundary and the per-tool
//! resource limits declared in [`crate::sansio::agent`]. All
//! filesystem I/O happens here; the Sans I/O core is fed the
//! resulting [`ToolOutcome`] / [`PatchPreview`] and cannot observe
//! the executor's internal state.
//!
//! Command execution (asynchronous child processes) is delegated to
//! the [`command`] submodule so its tokio-only dependencies stay off
//! the read-only / patch paths.

pub mod command;

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use nojson::{DisplayJson, Json, JsonFormatter};
use sha2::{Digest, Sha256};

use crate::sansio::agent::{
    DEFAULT_LIST_MAX_ENTRIES, DEFAULT_SEARCH_MAX_RESULTS, PATCH_MAX_FILE_BYTES, PatchError,
    PatchInvocation, PatchPreview, PatchTool, PreviewHash, READ_MAX_BYTES, ReadOnlyTool,
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

    /// Read every target file (Update) and check every target does
    /// not exist (Add), then produce the SHA-256 hashes and diff
    /// summary that the [`AgentCore`](crate::sansio::agent::AgentCore)
    /// needs to enter approval mode. Does NOT write anything to the
    /// filesystem.
    pub fn preview_patch(
        &self,
        invocation: &PatchInvocation,
    ) -> Result<(Vec<PreviewHash>, PatchPreview), PatchError> {
        let mut hashes = Vec::with_capacity(invocation.edits.len());
        let mut added_lines: u64 = 0;
        let mut removed_lines: u64 = 0;
        let mut target_paths: Vec<String> = Vec::with_capacity(invocation.edits.len());
        for edit in &invocation.edits {
            target_paths.push(edit.path().to_string());
            match edit {
                PatchTool::Add { path, content } => {
                    let full = self.resolve_add_target(path)?;
                    if full.exists() {
                        return Err(PatchError::AddOnExistingFile { path: path.clone() });
                    }
                    hashes.push(PreviewHash {
                        path: path.clone(),
                        sha256: None,
                    });
                    added_lines += line_count(content);
                }
                PatchTool::Update {
                    path,
                    before,
                    after,
                } => {
                    let full = self.resolve_update_target(path)?;
                    let bytes = read_file_capped(&full, path)?;
                    let hash: [u8; 32] = Sha256::digest(&bytes).into();
                    hashes.push(PreviewHash {
                        path: path.clone(),
                        sha256: Some(hash),
                    });
                    let matches = count_occurrences(&bytes, before.as_bytes());
                    match matches {
                        0 => return Err(PatchError::NoMatch { path: path.clone() }),
                        1 => {
                            removed_lines += line_count(before);
                            added_lines += line_count(after);
                        }
                        n => {
                            return Err(PatchError::AmbiguousMatch {
                                path: path.clone(),
                                match_count: n as u64,
                            });
                        }
                    }
                }
            }
        }
        target_paths.sort();
        target_paths.dedup();
        let preview = PatchPreview {
            target_paths,
            added_lines,
            removed_lines,
            edit_count: invocation.edits.len() as u64,
        };
        Ok((hashes, preview))
    }

    /// Apply all edits atomically in two phases (see `0007` design):
    ///
    /// - **Phase 1**: verify each target is in the same state as it
    ///   was at preview time (SHA-256 for Update, non-existence for
    ///   Add), then write every new content to a per-target `.tmp`
    ///   file. Any failure here aborts and deletes every `.tmp` file
    ///   already written.
    /// - **Phase 2**: rename each `.tmp` into its target in order.
    ///   Failure in phase 2 is treated as a rare filesystem
    ///   inconsistency: the remaining `.tmp` files are cleaned up but
    ///   already-renamed targets are left in place (rollback would
    ///   require another read + write pass and is out of scope).
    pub fn apply_patch(
        &self,
        invocation: &PatchInvocation,
        preview_hashes: &[PreviewHash],
    ) -> Result<Vec<PathBuf>, PatchError> {
        struct Prepared {
            target: PathBuf,
            tmp: PathBuf,
        }
        let mut prepared: Vec<Prepared> = Vec::with_capacity(invocation.edits.len());
        for (edit, expected_hash) in invocation.edits.iter().zip(preview_hashes.iter()) {
            let step = || -> Result<Prepared, PatchError> {
                match edit {
                    PatchTool::Add { path, content } => {
                        let full = self.resolve_add_target(path)?;
                        if full.exists() {
                            return Err(PatchError::AddOnExistingFile { path: path.clone() });
                        }
                        let tmp = tmp_path_for(&full);
                        write_atomically(&tmp, content.as_bytes(), path)?;
                        Ok(Prepared { target: full, tmp })
                    }
                    PatchTool::Update {
                        path,
                        before,
                        after,
                    } => {
                        let full = self.resolve_update_target(path)?;
                        let bytes = read_file_capped(&full, path)?;
                        let hash: [u8; 32] = Sha256::digest(&bytes).into();
                        let expected = expected_hash
                            .sha256
                            .ok_or_else(|| PatchError::Conflict { path: path.clone() })?;
                        if expected != hash {
                            return Err(PatchError::Conflict { path: path.clone() });
                        }
                        let matches = count_occurrences(&bytes, before.as_bytes());
                        if matches == 0 {
                            return Err(PatchError::NoMatch { path: path.clone() });
                        }
                        if matches > 1 {
                            return Err(PatchError::AmbiguousMatch {
                                path: path.clone(),
                                match_count: matches as u64,
                            });
                        }
                        let new_bytes = replace_once(&bytes, before.as_bytes(), after.as_bytes());
                        if new_bytes.len() > PATCH_MAX_FILE_BYTES {
                            return Err(PatchError::FileTooLarge { path: path.clone() });
                        }
                        let tmp = tmp_path_for(&full);
                        write_atomically(&tmp, &new_bytes, path)?;
                        Ok(Prepared { target: full, tmp })
                    }
                }
            };
            match step() {
                Ok(p) => prepared.push(p),
                Err(e) => {
                    for p in &prepared {
                        let _ = fs::remove_file(&p.tmp);
                    }
                    return Err(e);
                }
            }
        }

        let mut applied: Vec<PathBuf> = Vec::with_capacity(prepared.len());
        for (idx, p) in prepared.iter().enumerate() {
            match fs::rename(&p.tmp, &p.target) {
                Ok(()) => applied.push(p.target.clone()),
                Err(e) => {
                    for rest in &prepared[idx..] {
                        let _ = fs::remove_file(&rest.tmp);
                    }
                    let path = display_path(&p.target);
                    // EXDEV = 18 on both Linux and macOS. std does
                    // not expose a portable `ErrorKind` for this at
                    // MSRV 1.93, so we probe the raw errno.
                    if e.raw_os_error() == Some(18) {
                        return Err(PatchError::CrossDeviceRename { path });
                    }
                    return Err(PatchError::IoError {
                        path,
                        message: e.to_string(),
                    });
                }
            }
        }
        Ok(applied)
    }

    fn resolve_add_target(&self, rel: &str) -> Result<PathBuf, PatchError> {
        // For Add, the file itself does not exist yet. Resolve the
        // parent directory with the read-only path resolver, then
        // append the final component.
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err(PatchError::OutsideWorkspace {
                path: rel.to_string(),
            });
        }
        let parent = rel_path
            .parent()
            .ok_or_else(|| PatchError::OutsideWorkspace {
                path: rel.to_string(),
            })?;
        let file_name = rel_path
            .file_name()
            .ok_or_else(|| PatchError::OutsideWorkspace {
                path: rel.to_string(),
            })?;
        // Parent may itself be "" (root of workspace). resolve_within
        // handles "." as workspace root; adapt "" the same way.
        let parent_str = if parent.as_os_str().is_empty() {
            "."
        } else {
            parent
                .to_str()
                .ok_or_else(|| PatchError::OutsideWorkspace {
                    path: rel.to_string(),
                })?
        };
        let parent_resolved = match resolve_within(&self.root, parent_str) {
            Ok(p) => p,
            Err(ToolExecutionError::OutsideWorkspace) => {
                return Err(PatchError::OutsideWorkspace {
                    path: rel.to_string(),
                });
            }
            Err(_) => {
                return Err(PatchError::ParentDirMissing {
                    path: rel.to_string(),
                });
            }
        };
        Ok(parent_resolved.join(file_name))
    }

    fn resolve_update_target(&self, rel: &str) -> Result<PathBuf, PatchError> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err(PatchError::OutsideWorkspace {
                path: rel.to_string(),
            });
        }
        match resolve_within(&self.root, rel) {
            Ok(p) => Ok(p),
            Err(ToolExecutionError::OutsideWorkspace) => Err(PatchError::OutsideWorkspace {
                path: rel.to_string(),
            }),
            Err(_) => Err(PatchError::UpdateOnMissingFile {
                path: rel.to_string(),
            }),
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
// patch helpers
// -----------------------------------------------------------------

fn read_file_capped(path: &Path, rel: &str) -> Result<Vec<u8>, PatchError> {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(PatchError::UpdateOnMissingFile {
                path: rel.to_string(),
            });
        }
        Err(e) => {
            return Err(PatchError::IoError {
                path: rel.to_string(),
                message: e.to_string(),
            });
        }
    };
    if metadata.is_dir() {
        return Err(PatchError::IoError {
            path: rel.to_string(),
            message: "is a directory".to_string(),
        });
    }
    if metadata.len() as usize > PATCH_MAX_FILE_BYTES {
        return Err(PatchError::FileTooLarge {
            path: rel.to_string(),
        });
    }
    fs::read(path).map_err(|e| PatchError::IoError {
        path: rel.to_string(),
        message: e.to_string(),
    })
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
}

fn replace_once(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if let Some(pos) = position(haystack, needle) {
        let mut out = Vec::with_capacity(haystack.len() - needle.len() + replacement.len());
        out.extend_from_slice(&haystack[..pos]);
        out.extend_from_slice(replacement);
        out.extend_from_slice(&haystack[pos + needle.len()..]);
        out
    } else {
        haystack.to_vec()
    }
}

fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

fn line_count(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let base = text.matches('\n').count() as u64;
    // Trailing content without a `\n` also counts as one line so that
    // a single-line addition without a final newline shows +1.
    if text.ends_with('\n') { base } else { base + 1 }
}

fn tmp_path_for(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let counter = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut buf = target.as_os_str().to_os_string();
    buf.push(format!(".attini-tmp-{pid}-{counter}"));
    PathBuf::from(buf)
}

static TMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn write_atomically(tmp: &Path, bytes: &[u8], rel: &str) -> Result<(), PatchError> {
    let mut file = fs::File::create(tmp).map_err(|e| PatchError::IoError {
        path: rel.to_string(),
        message: e.to_string(),
    })?;
    file.write_all(bytes).map_err(|e| PatchError::IoError {
        path: rel.to_string(),
        message: e.to_string(),
    })?;
    file.sync_all().map_err(|e| PatchError::IoError {
        path: rel.to_string(),
        message: e.to_string(),
    })?;
    Ok(())
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
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
