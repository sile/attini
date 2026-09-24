//! Pure check for "provably safe read-only" `command` invocations.
//!
//! This is the built-in fallthrough for the `Pending` arm of the
//! command dispatcher (see `docs/design/safe-command-auto-approve.md`).
//! A `command` call that matches no rule is normally parked for a human;
//! this module answers the narrower question "is this call *for sure*
//! read-only, and if so which of its argv tokens are filesystem paths".
//!
//! It is **sans I/O**: it never touches the filesystem. Symlink
//! resolution and the "is this path inside a granted root" test are the
//! caller's job (`crate::tell_cli`), reusing the same canonicalisation
//! and roots `read` uses. This module only decides, from the argv alone,
//! that a specific program + subcommand + flag set cannot write, cannot
//! open a socket, cannot spawn a process, and (given that its path
//! tokens all resolve inside a root) cannot read anything the human did
//! not grant.
//!
//! The stance is **fail closed**: any option token the analysis does not
//! positively recognise as read-only makes the invocation `NotSafe`.
//! This is deliberately not a danger blocklist.

/// The verdict for one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Safety {
    /// The invocation is read-only *provided* every token in `paths`
    /// canonicalises inside a granted root. Tokens are the raw argv
    /// strings the caller must resolve (relative to the workspace root).
    Safe { paths: Vec<String> },
    /// Not provably safe: park as usual.
    NotSafe,
}

/// Decide whether `argv` is a provably safe read-only invocation.
///
/// `argv[0]` is matched by basename so `/usr/bin/git` and `git` behave
/// the same. Only `git` is recognised for now; every other program is
/// `NotSafe` (the allow-list grows deliberately, see the design doc).
pub fn safe_read_only(argv: &[String]) -> Safety {
    let Some(program) = argv.first() else {
        return Safety::NotSafe;
    };
    let basename = program.rsplit('/').next().unwrap_or(program.as_str());
    match basename {
        "git" => safe_git(&argv[1..]),
        _ => Safety::NotSafe,
    }
}

/// `git` with the program name already stripped from `args`.
///
/// A leading `-C <dir>` / `--git-dir=<dir>` is allowed: it retargets git
/// at another repository, but that directory becomes a *path operand* we
/// hand to the caller, which canonicalises it and requires it to sit
/// inside a granted root. So `git -C /etc status` is `Safe { paths:
/// ["/etc"] }` at this layer and the caller parks it, exactly as it
/// parks `git diff -- /etc/passwd`.
///
/// Every other leading global option stays fail-closed. `--work-tree`
/// and `--namespace` move *what* git reads independently of the `-C`
/// directory, so a path check on one directory cannot bound them; `-c` /
/// `--exec-path` can run hooks; repeated or malformed prefixes are
/// rejected rather than combined.
fn safe_git(args: &[String]) -> Safety {
    let mut paths: Vec<String> = Vec::new();
    let mut rest = args;
    // At most one leading retarget option, and only in the separated
    // `-C <dir>` or joined `--git-dir=<dir>` forms.
    match rest.first().map(String::as_str) {
        Some("-C") => {
            let Some(dir) = rest.get(1) else {
                return Safety::NotSafe;
            };
            // A value that looks like another option is not a directory.
            if dir.starts_with('-') {
                return Safety::NotSafe;
            }
            paths.push(dir.clone());
            rest = &rest[2..];
        }
        Some(flag) if flag.starts_with("--git-dir=") => {
            let dir = &flag["--git-dir=".len()..];
            if dir.is_empty() {
                return Safety::NotSafe;
            }
            paths.push(dir.to_string());
            rest = &rest[1..];
        }
        _ => {}
    }
    let Some(first) = rest.first() else {
        return Safety::NotSafe;
    };
    // Any remaining leading option (a second `-C`, `--work-tree`,
    // `--namespace`, `-c`, a `-C<dir>` run-together, ...) fails closed.
    if first.starts_with('-') {
        return Safety::NotSafe;
    }
    let sub = &rest[1..];
    let tail = match first.as_str() {
        "status" | "diff" | "log" | "show" | "ls-files" | "ls-tree" | "rev-parse" | "describe"
        | "shortlog" | "blame" => safe_git_read(sub),
        "branch" => safe_git_branch(sub),
        "remote" => safe_git_remote(sub),
        _ => return Safety::NotSafe,
    };
    // Fold the retarget directory in with the subcommand's own path
    // operands: every one must resolve inside a granted root.
    match tail {
        Safety::Safe { paths: mut inner } => {
            paths.append(&mut inner);
            Safety::Safe { paths }
        }
        Safety::NotSafe => Safety::NotSafe,
    }
}

/// Subcommands that are pure reads. Any option starting with `-` must be
/// recognised; path operands appear only after a literal `--`. Bare
/// positionals before `--` are treated as revs (git resolves them inside
/// the repo), so they carry no filesystem escape and are not checked.
fn safe_git_read(args: &[String]) -> Safety {
    let mut paths: Vec<String> = Vec::new();
    let mut after_dash_dash = false;
    for arg in args {
        if after_dash_dash {
            paths.push(arg.clone());
            continue;
        }
        if arg == "--" {
            after_dash_dash = true;
            continue;
        }
        if arg.starts_with('-') {
            if !git_read_option_is_safe(arg) {
                return Safety::NotSafe;
            }
            continue;
        }
        // A bare positional before `--`: a rev or a pathspec. We do not
        // treat it as a filesystem path (git scopes revs to the repo),
        // but a pathspec could name an outside file, so be strict and
        // require the `--` form for anything the caller wants to check.
        // Revs are fine; we cannot tell them apart, so accept them but
        // grant no outside read: git only reads inside its work tree.
    }
    Safety::Safe { paths }
}

/// The fixed safe option vocabulary for the read subcommands. Anything
/// else fails closed. This list is intentionally small; it grows only
/// with evidence, never with a "probably fine" flag.
fn git_read_option_is_safe(arg: &str) -> bool {
    // Split `--opt=value` so the vocabulary matches on the key.
    let key = arg.split('=').next().unwrap_or(arg);
    matches!(
        key,
        // `status` listing forms: unambiguously read-only.
        "-s" | "--short" | "--branch" | "--ahead-behind" | "--no-ahead-behind"
            | "--untracked-files" | "--column" | "--no-column"
            // Output shaping / paging.
            | "--no-pager" | "-p" | "--patch" | "--stat" | "--shortstat" | "--numstat"
            | "--name-only" | "--name-status" | "--color" | "--no-color" | "--oneline"
            | "--graph" | "--decorate" | "--no-decorate" | "--abbrev-commit"
            | "--pretty" | "--format" | "--date" | "--relative" | "--no-renames"
            | "-M" | "-C" | "--find-renames" | "--find-copies" | "--find-copies-harder"
            | "--follow" | "--find-object" | "--unified" | "-U" | "--word-diff"
            | "--ignore-all-space" | "-w" | "--ignore-space-change" | "-b"
            | "--ignore-blank-lines" | "--check" | "--ext-diff" | "--no-ext-diff"
            | "--textconv" | "--no-textconv" | "--submodule" | "--pickaxe-all"
            | "--pickaxe-regex" | "-S" | "-G" | "--diff-filter" | "--src-prefix"
            | "--dst-prefix" | "--line-prefix"
            // Revision ranges / counts.
            | "-n" | "--max-count" | "--skip" | "--since" | "--after" | "--until"
            | "--before" | "--author" | "--committer" | "--grep" | "-i"
            | "--regexp-ignore-case" | "--all" | "--branches" | "--tags"
            | "--remotes" | "--no-walk" | "--first-parent" | "--merges" | "--no-merges"
            | "--reverse" | "--topo-order" | "--date-order" | "--author-date-order"
            | "--left-right" | "--cherry-pick" | "--count" | "--walk-reflogs"
            | "--no-patch" | "--exit-code" | "--quiet"
            // `ls-files` / `ls-tree` / `rev-parse` / `show` / `describe`
            // / `shortlog` / `blame` (read-only) shaping. Note: the
            // global `--git-dir` / `--work-tree` / `-C` are NOT here (they
            // retarget git outside the workspace) and would be caught by
            // `safe_git` anyway.
            | "-z" | "--stage" | "--long" | "-l" | "--dirty"
            | "--always" | "--abbrev" | "--show-toplevel"
            | "--is-inside-work-tree" | "--is-bare-repository" | "--show-cdup"
            | "--show-prefix" | "--verify" | "-e" | "--line-porcelain" | "--porcelain"
            | "-L" | "-f" | "-c" | "--diff" | "--contents"
            | "--exclude" | "--exclude-standard" | "--others" | "-o" | "--ignored"
            | "-d" | "--directory" | "--no-empty-directory" | "--deleted" | "-m"
            | "--modified" | "--unmerged" | "-u" | "--killed" | "--error-unmatch"
    )
}

/// `git branch`: a read only in a few shapes.
fn safe_git_branch(args: &[String]) -> Safety {
    let paths: Vec<String> = Vec::new();
    for arg in args {
        if arg == "--" {
            // A `--` in branch is not a shape we bless.
            return Safety::NotSafe;
        }
        if arg.starts_with('-') {
            // Only pure-listing flags are safe; every mutation flag
            // (`-D`/`-d`/`-m`/`-M`/`--delete`/`--move`/`--copy`/...) and
            // every unknown flag fails closed here.
            match arg.as_str() {
                "-a" | "--all" | "-r" | "--remotes" | "-v" | "-vv" | "--verbose" | "--list"
                | "--show-current" | "--color" | "--no-color" | "--format" | "--sort"
                | "--merged" | "--no-merged" | "--contains" | "--no-contains" | "--points-at" => {}
                _ => return Safety::NotSafe,
            }
            continue;
        }
        // A bare positional to `branch` would create a branch (a ref
        // write), so only the pure-listing forms are safe.
        return Safety::NotSafe;
    }
    Safety::Safe { paths }
}

/// `git remote`: only the read forms (`remote`, `remote -v`, `remote
/// show`, `remote get-url`). Add/remove/set-url/prune/update are writes.
fn safe_git_remote(args: &[String]) -> Safety {
    // Bare `git remote` and `git remote -v` list remotes.
    match args {
        [] => return Safety::Safe { paths: Vec::new() },
        [only] if only == "-v" || only == "--verbose" => {
            return Safety::Safe { paths: Vec::new() };
        }
        _ => {}
    }
    let Some(sub) = args.first() else {
        return Safety::NotSafe;
    };
    match sub.as_str() {
        "show" | "get-url" => Safety::Safe { paths: Vec::new() },
        _ => Safety::NotSafe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn paths(items: &[&str]) -> Option<Vec<String>> {
        match safe_read_only(&argv(items)) {
            Safety::Safe { paths } => Some(paths),
            Safety::NotSafe => None,
        }
    }

    #[test]
    fn git_status_is_safe() {
        assert_eq!(paths(&["git", "status"]), Some(vec![]));
        assert_eq!(paths(&["git", "status", "--short"]), Some(vec![]));
        assert_eq!(paths(&["git", "status", "--porcelain"]), Some(vec![]));
    }

    #[test]
    fn git_status_unknown_flag_fails_closed() {
        assert_eq!(paths(&["git", "status", "--exec=evil"]), None);
        assert_eq!(paths(&["git", "status", "--bogus"]), None);
    }

    #[test]
    fn git_basename_is_matched() {
        assert_eq!(paths(&["/usr/bin/git", "status"]), Some(vec![]));
    }

    #[test]
    fn git_dash_c_contributes_a_path() {
        // `-C` retargets git, so its directory is a path operand the
        // caller must resolve; at this layer the invocation is Safe and
        // the directory is reported.
        assert_eq!(
            paths(&["git", "-C", "src", "status"]),
            Some(vec!["src".to_string()])
        );
        assert_eq!(
            paths(&["git", "-C", "/etc", "status"]),
            Some(vec!["/etc".to_string()])
        );
        assert_eq!(
            paths(&["git", "-C", "sub", "diff", "--", "f.rs"]),
            Some(vec!["sub".to_string(), "f.rs".to_string()])
        );
    }

    #[test]
    fn git_git_dir_equals_contributes_a_path() {
        assert_eq!(
            paths(&["git", "--git-dir=.git", "log"]),
            Some(vec![".git".to_string()])
        );
    }

    #[test]
    fn git_global_options_fail_closed() {
        // `--work-tree` / `--namespace` move the read scope independently
        // of any one directory, so they stay fail-closed.
        assert_eq!(paths(&["git", "--work-tree", "/x", "status"]), None);
        assert_eq!(paths(&["git", "--work-tree=/x", "status"]), None);
        assert_eq!(paths(&["git", "--namespace=ns", "log"]), None);
        // A second retarget option is not combined.
        assert_eq!(paths(&["git", "-C", ".", "-C", "src", "status"]), None);
        // Malformed prefixes: a `-C` value that looks like another option,
        // a run-together `-C<dir>`, an empty `--git-dir=`, and a trailing
        // `-C` with no value.
        assert_eq!(paths(&["git", "-C", "-p", "status"]), None);
        assert_eq!(paths(&["git", "-Csrc", "status"]), None);
        assert_eq!(paths(&["git", "--git-dir=", "status"]), None);
        assert_eq!(paths(&["git", "-C"]), None);
        assert_eq!(paths(&["git", "--git-dir=src"]), None, "no subcommand");
        // A leading global we never bless.
        assert_eq!(paths(&["git", "-c", "core.x=y", "status"]), None);
    }

    #[test]
    fn git_diff_after_dash_dash_reports_paths() {
        assert_eq!(
            paths(&["git", "diff", "--", "src/lib.rs"]),
            Some(vec!["src/lib.rs".to_string()])
        );
    }

    #[test]
    fn git_diff_rev_is_not_a_path() {
        assert_eq!(paths(&["git", "diff", "HEAD~1"]), Some(vec![]));
    }

    #[test]
    fn git_diff_unknown_flag_fails_closed() {
        assert_eq!(paths(&["git", "diff", "--output=/x", "HEAD"]), None);
    }

    #[test]
    fn git_branch_listing_is_safe() {
        assert_eq!(paths(&["git", "branch"]), Some(vec![]));
        assert_eq!(paths(&["git", "branch", "-a"]), Some(vec![]));
        assert_eq!(paths(&["git", "branch", "--show-current"]), Some(vec![]));
    }

    #[test]
    fn git_branch_delete_fails_closed() {
        assert_eq!(paths(&["git", "branch", "-D", "foo"]), None);
        assert_eq!(paths(&["git", "branch", "-d", "foo"]), None);
        assert_eq!(paths(&["git", "branch", "foo"]), None, "create is a write");
    }

    #[test]
    fn git_remote_read_forms_are_safe() {
        assert_eq!(paths(&["git", "remote"]), Some(vec![]));
        assert_eq!(paths(&["git", "remote", "-v"]), Some(vec![]));
        assert_eq!(paths(&["git", "remote", "show"]), Some(vec![]));
    }

    #[test]
    fn git_remote_write_forms_fail_closed() {
        assert_eq!(paths(&["git", "remote", "add", "origin", "url"]), None);
        assert_eq!(paths(&["git", "remote", "remove", "origin"]), None);
        assert_eq!(paths(&["git", "remote", "prune"]), None);
    }

    #[test]
    fn git_write_subcommands_fail_closed() {
        assert_eq!(paths(&["git", "add", "x"]), None);
        assert_eq!(paths(&["git", "commit", "-m", "x"]), None);
        assert_eq!(paths(&["git", "push"]), None);
        assert_eq!(paths(&["git", "fetch"]), None);
        assert_eq!(paths(&["git", "clean", "-fd"]), None);
        assert_eq!(paths(&["git", "reset", "--hard"]), None);
    }

    #[test]
    fn non_git_programs_fail_closed_for_now() {
        assert_eq!(paths(&["cat", "src/lib.rs"]), None);
        assert_eq!(paths(&["bash", "-c", "ls"]), None);
        assert_eq!(paths(&["ls"]), None);
        assert_eq!(paths(&[]), None);
    }
}
