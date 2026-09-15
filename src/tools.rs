//! Workspace tool executor.
//!
//! Runs the [`ReadOnlyTool`] and [`PatchInvocation`] tool calls the
//! model emits, enforcing the workspace boundary and the per-tool
//! resource limits declared in [`crate::sansio::agent`]. All
//! filesystem I/O happens here; the Sans I/O core is fed the
//! resulting [`ToolOutcome`] / [`PatchPreview`] and cannot observe
//! the executor's internal state.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use nojson::{DisplayJson, Json, JsonFormatter};

use crate::sansio::agent::{
    DEFAULT_LIST_MAX_ENTRIES, DEFAULT_SEARCH_MAX_RESULTS, PATCH_MAX_FILE_BYTES, PatchError,
    PatchInvocation, PatchPreview, PatchTool, PreviewContent, READ_MAX_BYTES, ReadOnlyTool,
    ToolExecutionError, ToolOutcome,
};
use crate::sansio::permissions::{
    Authorization, Judgment, Rule, RuleScope, ScopedRules, evaluate_write,
};

/// Workspace-scoped executor for [`ReadOnlyTool`] invocations.
#[derive(Debug)]
pub struct ToolExecutor {
    /// Workspace root. Both read-only and patch tools use this as the
    /// primary boundary. Patch tool uses only this field.
    root: PathBuf,
    /// Additional read-only roots granted via `read` rules in
    /// `permissions.jsonl`. Read-only tools
    /// accept paths that canonicalise into any of these roots. Patch
    /// tool ignores this field entirely (write access to these paths
    /// is out of scope).
    extra_read_roots: Vec<PathBuf>,
    /// Session name (`.attini/{name}/`). Used by the patch tool's
    /// Layer 2 rule to identify this session's scratchpad directory.
    session_name: String,
    /// Git repository state captured at startup for the patch tool's
    /// Layer 3 (tracked files) and Layer 4 (not-in-repo) checks.
    git_state: GitState,
    /// `write` permission rules in increasing precedence (workspace
    /// layer, then session layer, flattened). Injected after
    /// construction by the caller once rules are loaded, so `patch`
    /// writes can be authorised by an explicit rule before falling
    /// back to the git-tracking heuristic. Empty by default: the
    /// executor then behaves exactly as before.
    write_rules: Vec<Rule>,
}

/// Git repository state as observed by `ToolExecutor::new`.
/// `Repo` carries the workspace-relative canonical paths of files
/// tracked by `git ls-files` at startup; `Add` success mutates the
/// set within the invocation so subsequent `Update` on the same
/// path is not rejected as untracked.
#[derive(Debug)]
enum GitState {
    Repo { tracked: RefCell<HashSet<PathBuf>> },
    NotARepo,
}

impl ToolExecutor {
    /// Create an executor rooted at `root` with additional read-only
    /// roots granted from `permissions.jsonl`. The
    /// workspace root is canonicalised so later boundary checks
    /// compare against a stable prefix; `extra_read_roots` are
    /// expected to already be canonicalised by the caller (paths
    /// that fail to canonicalise should be warned + skipped upstream).
    /// `session_name` names the session directory that hosts the
    /// scratchpad free-write zone for Layer 2.
    ///
    /// Also probes git state at startup for Layer 3/4 of the patch
    /// tool's write guards. When the workspace is inside a git
    /// working tree, `git ls-files` runs once and its output is
    /// normalised to workspace-relative canonical paths for O(1)
    /// tracked-file lookup during patch. When it is not, Layer 4
    /// takes over: writes outside scratchpad are refused, and a
    /// warning is emitted on stderr once at startup.
    pub fn new(
        root: impl AsRef<Path>,
        extra_read_roots: Vec<PathBuf>,
        session_name: String,
    ) -> io::Result<Self> {
        let root = root.as_ref().canonicalize()?;
        let git_state = probe_git_state(&root);
        Ok(Self {
            root,
            extra_read_roots,
            session_name,
            git_state,
            write_rules: Vec::new(),
        })
    }

    /// Inject the flattened `write` permission rules (workspace layer
    /// first, then session layer). Called by the caller after both
    /// layers are loaded. When empty, patch writes fall back to the
    /// git-tracking heuristic (Layer 3/4).
    pub fn set_write_rules(&mut self, write_rules: Vec<Rule>) {
        self.write_rules = write_rules;
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn extra_read_roots(&self) -> &[PathBuf] {
        &self.extra_read_roots
    }

    pub fn execute(&self, tool: ReadOnlyTool) -> ToolOutcome {
        self.execute_with_roots(tool, &self.extra_read_roots)
    }

    /// Execute a single read-only tool with an additional one-shot read
    /// root appended to the executor's own roots. Used to honour a
    /// human's approval to read one path outside the workspace for a
    /// single call: the extra root lives only for this invocation and is
    /// never persisted.
    pub fn execute_with_extra_read_root(&self, tool: ReadOnlyTool, extra: PathBuf) -> ToolOutcome {
        let mut roots = self.extra_read_roots.clone();
        roots.push(extra);
        self.execute_with_roots(tool, &roots)
    }

    fn execute_with_roots(&self, tool: ReadOnlyTool, roots: &[PathBuf]) -> ToolOutcome {
        match tool {
            ReadOnlyTool::List {
                path,
                recursive,
                max_entries,
                include_hidden,
            } => self.execute_list(&path, recursive, max_entries, include_hidden, roots),
            ReadOnlyTool::Read { path, line_range } => self.execute_read(&path, line_range, roots),
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
                roots,
            ),
        }
    }

    /// Read every target file (Update) and check every target does
    /// not exist (Add), then produce the byte snapshots and diff
    /// summary that the [`AgentCore`](crate::sansio::agent::AgentCore)
    /// needs to enter approval mode. Does NOT write anything to the
    /// filesystem.
    pub fn preview_patch(
        &self,
        invocation: &PatchInvocation,
    ) -> Result<(Vec<PreviewContent>, PatchPreview), PatchError> {
        let mut snapshots = Vec::with_capacity(invocation.edits.len());
        let mut added_lines: u64 = 0;
        let mut removed_lines: u64 = 0;
        let mut target_paths: Vec<String> = Vec::with_capacity(invocation.edits.len());
        let mut auto_approve = true;
        let mut not_revertible: Option<String> = None;
        for edit in &invocation.edits {
            target_paths.push(edit.path().to_string());
            match edit {
                PatchTool::Add { path, content } => {
                    // A brand-new file is not yet under git control, so
                    // creating it cannot be auto-approved.
                    auto_approve = false;
                    let (full, guard) = self.resolve_add_target(path)?;
                    if let WriteGuard::NeedsApproval { reason } = guard {
                        not_revertible.get_or_insert(reason);
                    }
                    if full.exists() {
                        return Err(PatchError::AddOnExistingFile { path: path.clone() });
                    }
                    snapshots.push(PreviewContent {
                        path: path.clone(),
                        content: None,
                    });
                    added_lines += line_count(content);
                }
                PatchTool::Update {
                    path,
                    before,
                    after,
                } => {
                    let (full, guard) = self.resolve_update_target(path)?;
                    // A git-tracked Update (or one covered by a `write
                    // allow:true` rule) is safe to auto-approve. A
                    // `NeedsApproval` guard means the change is not
                    // revertible with `git checkout`, so the human must
                    // see it first.
                    if let WriteGuard::NeedsApproval { reason } = guard {
                        auto_approve = false;
                        not_revertible.get_or_insert(reason);
                    }
                    let bytes = read_file_capped(&full, path)?;
                    snapshots.push(PreviewContent {
                        path: path.clone(),
                        content: Some(bytes.clone()),
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
            auto_approve,
            not_revertible,
        };
        Ok((snapshots, preview))
    }

    /// Apply all edits atomically in two phases (see `0007` design):
    ///
    /// - **Phase 1**: verify each target is in the same state as it
    ///   was at preview time (exact byte equality for Update,
    ///   non-existence for Add), then write every new content to a
    ///   per-target `.tmp` file. Any failure here aborts and deletes
    ///   every `.tmp` file already written.
    /// - **Phase 2**: rename each `.tmp` into its target in order.
    ///   Failure in phase 2 is treated as a rare filesystem
    ///   inconsistency: the remaining `.tmp` files are cleaned up but
    ///   already-renamed targets are left in place (rollback would
    ///   require another read + write pass and is out of scope).
    pub fn apply_patch(
        &self,
        invocation: &PatchInvocation,
        preview_content: &[PreviewContent],
    ) -> Result<Vec<PathBuf>, PatchError> {
        struct Prepared {
            target: PathBuf,
            tmp: PathBuf,
        }
        let mut prepared: Vec<Prepared> = Vec::with_capacity(invocation.edits.len());
        for (edit, snapshot) in invocation.edits.iter().zip(preview_content.iter()) {
            let step = || -> Result<Prepared, PatchError> {
                match edit {
                    PatchTool::Add { path, content } => {
                        let (full, _) = self.resolve_add_target(path)?;
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
                        let (full, _) = self.resolve_update_target(path)?;
                        let bytes = read_file_capped(&full, path)?;
                        let expected = snapshot
                            .content
                            .as_deref()
                            .ok_or_else(|| PatchError::Conflict { path: path.clone() })?;
                        if expected != bytes.as_slice() {
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
                Ok(()) => {
                    // Layer 3 in-invocation tracking: a successful
                    // rename means the target now exists on disk. For
                    // Adds this admits the just-created file into the
                    // tracked set so a follow-up Update within the
                    // same invocation is not rejected. For Updates the
                    // insert is a no-op (already tracked).
                    self.mark_added(&p.target);
                    applied.push(p.target.clone());
                }
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

    fn resolve_add_target(&self, rel: &str) -> Result<(PathBuf, WriteGuard), PatchError> {
        // For Add, the file itself does not exist yet. Resolve the
        // parent directory (which must exist), then append the final
        // component. Unlike `resolve_within`, absolute paths and
        // relative paths that escape the workspace with `..` are
        // accepted; an outside target is parked for approval by
        // `check_patch_write` rather than refused here.
        let rel_path = Path::new(rel);
        let parent = rel_path
            .parent()
            .ok_or_else(|| PatchError::ParentDirMissing {
                path: rel.to_string(),
            })?;
        let file_name = rel_path
            .file_name()
            .ok_or_else(|| PatchError::ParentDirMissing {
                path: rel.to_string(),
            })?;
        // Layer 2 subdir pre-create (chicken-and-egg resolution):
        // if the target textually lives under this session's
        // scratchpad, create any missing parent directories before
        // the parent is canonicalised. Layer 2 is re-verified in
        // canonical form after resolution so symlink-based escapes
        // still fail. Paths whose syntactic normalisation does not
        // stay under scratchpad get no pre-create side effect.
        self.maybe_pre_create_scratchpad_parent(rel_path)?;
        // Parent may itself be "" (root of workspace). `"."`
        // expresses that for both workspace-relative and absolute
        // resolution (an absolute path always has a parent).
        let parent_str = if parent.as_os_str().is_empty() {
            "."
        } else {
            parent
                .to_str()
                .ok_or_else(|| PatchError::ParentDirMissing {
                    path: rel.to_string(),
                })?
        };
        let parent_resolved = resolve_for_patch(&self.root, parent_str).map_err(|_| {
            PatchError::ParentDirMissing {
                path: rel.to_string(),
            }
        })?;
        let target = parent_resolved.join(file_name);
        let guard = self.check_patch_write(&target, rel, PatchOp::Add)?;
        Ok((target, guard))
    }

    fn resolve_update_target(&self, rel: &str) -> Result<(PathBuf, WriteGuard), PatchError> {
        // Unlike `resolve_within`, this accepts absolute paths and
        // relative paths that escape the workspace with `..`. The
        // target is canonicalised whether or not it lands inside the
        // workspace; an outside target is not refused outright but
        // routes to `check_patch_write`, which parks it for approval.
        let resolved = match resolve_for_patch(&self.root, rel) {
            Ok(p) => p,
            Err(_) => {
                return Err(PatchError::UpdateOnMissingFile {
                    path: rel.to_string(),
                });
            }
        };
        let guard = self.check_patch_write(&resolved, rel, PatchOp::Update)?;
        Ok((resolved, guard))
    }

    /// If `rel_path` syntactically normalises to a location under this
    /// session's scratchpad, create any missing parent directories.
    /// Otherwise do nothing (no side effect, no error). Called from
    /// `resolve_add_target` before the parent is canonicalised, so a
    /// missing scratchpad parent does not fail resolution.
    fn maybe_pre_create_scratchpad_parent(&self, rel_path: &Path) -> Result<(), PatchError> {
        let sp_rel = Path::new(".attini")
            .join(&self.session_name)
            .join("scratchpad");
        let joined = Path::new(".").join(rel_path);
        let Some(normalised) = syntactic_normalize(&joined) else {
            return Ok(());
        };
        if !normalised.starts_with(&sp_rel) {
            return Ok(());
        }
        let Some(parent_rel) = normalised.parent() else {
            return Ok(());
        };
        let parent_abs = self.root.join(parent_rel);
        fs::create_dir_all(&parent_abs).map_err(|e| PatchError::IoError {
            path: rel_path.to_string_lossy().into_owned(),
            message: e.to_string(),
        })?;
        Ok(())
    }

    fn execute_list(
        &self,
        rel_path: &str,
        recursive: bool,
        max_entries: usize,
        include_hidden: bool,
        roots: &[PathBuf],
    ) -> ToolOutcome {
        let dir = match resolve_within_any(&self.root, roots, rel_path) {
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

    fn execute_read(
        &self,
        rel_path: &str,
        line_range: Option<(usize, usize)>,
        roots: &[PathBuf],
    ) -> ToolOutcome {
        let path = match resolve_within_any(&self.root, roots, rel_path) {
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
        roots: &[PathBuf],
    ) -> ToolOutcome {
        if pattern.is_empty() {
            return ToolOutcome::Err(ToolExecutionError::ArgumentsParseFailed(
                "pattern must not be empty".to_string(),
            ));
        }
        let base = match path_prefix {
            Some(p) => match resolve_within_any(&self.root, roots, p) {
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

// -------------------------------------------------------------------
// Patch tool write guards: four-layer check.
//
// Layer 1: hardcoded runtime-critical always-reject (canonical-form)
// Layer 2: scratchpad always-allow (with subdir auto-create)
// Layer 3: git tracking check (Update: tracked; Add: parent not ignored)
// Layer 4: not-in-git-repo fallback (all Layer-3 candidates reject)
// -------------------------------------------------------------------

/// Probe whether `root` is inside a git working tree. If so, capture
/// the tracked-file set as workspace-relative canonical paths.
fn probe_git_state(root: &Path) -> GitState {
    let toplevel = match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) if out.status.success() => out,
        _ => {
            eprintln!(
                "attini: workspace is not a git repository; patch tool will refuse writes outside scratchpad. Consider `git init` for full patch access."
            );
            return GitState::NotARepo;
        }
    };
    // Sanity: repo toplevel must exist and canonicalise. If it does,
    // we still keep the tracked set even when root is a subdir of the
    // repo — Layer 3 only cares whether the target path is tracked.
    let toplevel_str = String::from_utf8_lossy(&toplevel.stdout);
    if toplevel_str.trim().is_empty() {
        eprintln!(
            "attini: `git rev-parse --show-toplevel` returned empty; patch tool will refuse writes outside scratchpad."
        );
        return GitState::NotARepo;
    }
    let ls_out = match Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) if out.status.success() => out,
        Ok(_) | Err(_) => {
            eprintln!(
                "attini: `git ls-files` failed; patch tool will treat all files as untracked."
            );
            return GitState::Repo {
                tracked: RefCell::new(HashSet::new()),
            };
        }
    };
    let mut tracked: HashSet<PathBuf> = HashSet::new();
    for line in String::from_utf8_lossy(&ls_out.stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let joined = root.join(line);
        let canon = match joined.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Ok(rel) = canon.strip_prefix(root) {
            tracked.insert(rel.to_path_buf());
        }
    }
    GitState::Repo {
        tracked: RefCell::new(tracked),
    }
}

/// Given a canonical absolute path known to be under `root`, return
/// the workspace-relative canonical form. Returns `None` if the
/// caller passed a path outside `root`.
fn workspace_relative_canonical(canon: &Path, root: &Path) -> Option<PathBuf> {
    canon.strip_prefix(root).ok().map(|p| p.to_path_buf())
}

/// Layer 1 pattern match on a workspace-relative canonical path.
/// Returns `Some(reason)` describing why the path is runtime-critical,
/// or `None` if it is not covered by Layer 1.
fn layer1_reject_reason(rel: &Path) -> Option<&'static str> {
    let comps: Vec<Component<'_>> = rel.components().collect();
    let seg = |i: usize| -> Option<&str> {
        comps.get(i).and_then(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
    };
    match seg(0)? {
        ".git" => Some("git metadata"),
        ".attini" => {
            let name1 = seg(1)?;
            if comps.len() == 2 {
                match name1 {
                    "permissions.jsonl" => Some("workspace permissions"),
                    _ => None,
                }
            } else if comps.len() == 3 {
                match seg(2)? {
                    "LOCK" => Some("session runtime state"),
                    "conversation.jsonl" => Some("session runtime state"),
                    "pending.json" => Some("session runtime state"),
                    "permissions.jsonl" => Some("session permissions"),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Textually resolve `.` and `..` components in `path` without
/// touching the filesystem. Returns `None` if `..` would rise above
/// the root (leading `..` on a relative path with no ancestors to
/// pop).
fn syntactic_normalize(path: &Path) -> Option<PathBuf> {
    let mut out: Vec<Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {
                    // Cannot pop the root; a `..` here stays as-is
                    // (matches POSIX behaviour for `/../`).
                }
                _ => {
                    out.push(Component::ParentDir);
                }
            },
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for c in out {
        buf.push(c.as_os_str());
    }
    Some(buf)
}

/// Absolute canonical prefix of this session's scratchpad. Callers
/// use this to check whether a target path is Layer-2 eligible.
fn scratchpad_root(root: &Path, session_name: &str) -> PathBuf {
    root.join(".attini").join(session_name).join("scratchpad")
}

impl ToolExecutor {
    /// Check whether an already-canonical absolute `canon` path is
    /// allowed for patch write. Called after `resolve_for_patch` has
    /// canonicalised the target, which may lie inside or outside the
    /// workspace. Returns a `WriteGuard` (allow, or park for approval);
    /// hard rejections come back as the specific `PatchError` variant
    /// the caller should surface.
    fn check_patch_write(
        &self,
        canon: &Path,
        _rel_hint: &str,
        op: PatchOp,
    ) -> Result<WriteGuard, PatchError> {
        // Explicit `write` rules take precedence over the git-tracking
        // heuristic. A winning `allow:true` rule permits the write even
        // when Layer 3 would reject it (untracked / not-a-repo); a
        // winning `allow:false` rule refuses it even when git tracking
        // would allow. With no matching rule, fall through to Layer 3/4.
        // The rules are held flattened; the scope tag is only cosmetic
        // for the decision record, so a single workspace-labelled layer
        // is enough for matching here.
        let layers: [ScopedRules<'_>; 1] = [(RuleScope::Workspace, self.write_rules.as_slice())];
        let Some(rel) = workspace_relative_canonical(canon, &self.root) else {
            // Target is outside the workspace. Layer 1 (runtime-critical
            // `.git`/`.attini` paths) and the git-tracking heuristic are
            // both workspace-relative concepts and do not apply. An
            // explicit `write` rule may still match, using the absolute
            // canonical path (outside-workspace write rules are written
            // as absolute paths, mirroring `read` rules). Otherwise the
            // write is parked for one-shot human approval.
            return match evaluate_write(&layers, canon, &Authorization::PerTool) {
                Judgment::AutoApprove(_) => Ok(WriteGuard::Allow),
                Judgment::AutoDeny(_) => Err(PatchError::ExcludedPath {
                    path: canon.to_string_lossy().into_owned(),
                    reason: "denied by a write rule".to_string(),
                }),
                Judgment::Pending => Ok(WriteGuard::NeedsApproval {
                    reason: "the target is outside the workspace".to_string(),
                }),
            };
        };
        // Layer 1
        if let Some(reason) = layer1_reject_reason(&rel) {
            return Err(PatchError::ExcludedPath {
                path: display_workspace_relative(&rel),
                reason: reason.to_string(),
            });
        }
        // Layer 2 (canonical re-verification): if canon lives under
        // this session's canonical scratchpad, allow unconditionally.
        // Callers must have already ensured any needed subdir was
        // created in the Add path (see resolve_add_target).
        if let Ok(sp_canon) = scratchpad_root(&self.root, &self.session_name).canonicalize()
            && canon.starts_with(&sp_canon)
        {
            return Ok(WriteGuard::Allow);
        }
        match evaluate_write(&layers, &rel, &Authorization::PerTool) {
            Judgment::AutoApprove(_) => return Ok(WriteGuard::Allow),
            Judgment::AutoDeny(_) => {
                return Err(PatchError::ExcludedPath {
                    path: display_workspace_relative(&rel),
                    reason: "denied by a write rule".to_string(),
                });
            }
            Judgment::Pending => {}
        }
        // Layer 3 / 4. These no longer hard-reject the write; instead
        // they ask for human approval (decision D1). The `IgnoredParent`
        // case stays a hard rejection: a gitignored region is an
        // explicit user declaration that the path is out-of-scope, not
        // merely "irrecoverable with git checkout".
        match &self.git_state {
            GitState::NotARepo => Ok(WriteGuard::NeedsApproval {
                reason: "the workspace is not a git repository, so this change cannot be \
                         reverted with `git checkout`"
                    .to_string(),
            }),
            GitState::Repo { tracked } => match op {
                PatchOp::Update => {
                    if tracked.borrow().contains(&rel) {
                        Ok(WriteGuard::Allow)
                    } else {
                        Ok(WriteGuard::NeedsApproval {
                            reason: "the target is not tracked by git, so this change cannot \
                                     be reverted with `git checkout`"
                                .to_string(),
                        })
                    }
                }
                PatchOp::Add => {
                    let parent_rel = rel.parent().unwrap_or(Path::new(""));
                    if is_gitignored(&self.root, parent_rel) {
                        Err(PatchError::IgnoredParent {
                            path: display_workspace_relative(&rel),
                        })
                    } else {
                        Ok(WriteGuard::Allow)
                    }
                }
            },
        }
    }

    /// Record a successful Add so subsequent Update on the same path
    /// within this invocation is not rejected as untracked.
    fn mark_added(&self, canon: &Path) {
        let GitState::Repo { tracked } = &self.git_state else {
            return;
        };
        if let Some(rel) = workspace_relative_canonical(canon, &self.root) {
            tracked.borrow_mut().insert(rel);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchOp {
    Add,
    Update,
}

/// Outcome of the patch write guard (`check_patch_write`). `Allow`
/// means the write may proceed without an approval prompt (a git-
/// tracked Update, a Layer-2 scratchpad path, or a `write allow:true`
/// rule). `NeedsApproval` means the write must be previewed for the
/// human, carrying a short reason that explains why the change cannot
/// be reverted with `git checkout`. Hard rejections (Layer 1 protected
/// paths, `write allow:false` rules, gitignored parents, workspace
/// escapes) are returned as `Err(PatchError)` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteGuard {
    Allow,
    NeedsApproval { reason: String },
}

fn display_workspace_relative(rel: &Path) -> String {
    rel.to_string_lossy().into_owned()
}

/// Run `git -C <root> check-ignore --quiet <candidate>`. Exit 0 means
/// the candidate is ignored; anything else (including "not ignored",
/// missing git, or the candidate being empty) means not ignored so
/// far as we can tell.
fn is_gitignored(root: &Path, candidate: &Path) -> bool {
    let target = if candidate.as_os_str().is_empty() {
        Path::new(".")
    } else {
        candidate
    };
    match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "--quiet"])
        .arg(target)
        .status()
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Patch-tool path resolver. Unlike `resolve_within`, it accepts
/// absolute paths and relative paths that escape the workspace with
/// `..`. The result is canonicalised whether or not it lands inside
/// the workspace; the caller (`check_patch_write`) decides whether an
/// outside target is parked for approval. A path that cannot be
/// canonicalised (e.g. the file does not exist) is an error.
fn resolve_for_patch(root: &Path, rel: &str) -> Result<PathBuf, ToolExecutionError> {
    let rel_path = Path::new(rel);
    let candidate = if rel_path.is_absolute() {
        rel_path.to_path_buf()
    } else {
        root.join(rel_path)
    };
    candidate
        .canonicalize()
        .map_err(|e| ToolExecutionError::IoError(format!("{rel}: {e}")))
}

/// Read-only resolver that accepts paths under `workspace_root` or
/// any of the caller-granted `extra_read_roots`.
///
/// - Relative `input` is always joined against `workspace_root` only
///   (not against the extra roots) so path traversal semantics stay
///   consistent with what the model already knows.
/// - Absolute `input` is canonicalised directly and accepted if it
///   ends up inside any of the roots (workspace or extra).
///
/// Patch (write) resolution lives in `resolve_for_patch`, which accepts
/// any canonicalisable path and lets `check_patch_write` decide whether
/// an outside target parks for approval.
fn resolve_within_any(
    workspace_root: &Path,
    extra_read_roots: &[PathBuf],
    input: &str,
) -> Result<PathBuf, ToolExecutionError> {
    let input_path = Path::new(input);
    let candidate = if input_path.is_absolute() {
        input_path.to_path_buf()
    } else {
        workspace_root.join(input_path)
    };
    let canon = candidate
        .canonicalize()
        .map_err(|e| ToolExecutionError::IoError(format!("{input}: {e}")))?;
    if canon.starts_with(workspace_root) {
        return Ok(canon);
    }
    for extra in extra_read_roots {
        if canon.starts_with(extra) {
            return Ok(canon);
        }
    }
    Err(ToolExecutionError::OutsideWorkspace)
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
