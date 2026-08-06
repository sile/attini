//! Integration tests for `attini::skills` roots resolution and
//! discovery listing. Uses temp dirs as explicit roots so the tests
//! are hermetic with respect to `HOME` and the workspace `.attini/`.

use std::fs;
use std::path::PathBuf;

use attini::skills::{SKILL_MAX_BYTES, discover_all_in, resolve_skill_dir_in};

fn tempdir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("attini-skills-int-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn write_skill(root: &std::path::Path, name: &str, contents: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).expect("create skill dir");
    let skill_md = dir.join("SKILL.md");
    fs::write(&skill_md, contents).expect("write SKILL.md");
    dir
}

// -----------------------------------------------------------------
// resolve_skill_dir_in
// -----------------------------------------------------------------

#[test]
fn resolve_prefers_project_over_global_on_same_name() {
    let dir = tempdir("resolve_project_wins");
    let global = dir.join("global");
    let project = dir.join("project");
    let global_skill = write_skill(&global, "shared", "---\ndescription: from global\n---\nG");
    let project_skill = write_skill(&project, "shared", "---\ndescription: from project\n---\nP");
    let resolved = resolve_skill_dir_in(&[global.clone(), project.clone()], "shared")
        .expect("resolved to some skill");
    assert_eq!(resolved, project_skill);
    // Sanity: global path is not returned even though it exists.
    assert!(global_skill.exists());
}

#[test]
fn resolve_falls_back_to_global_when_project_missing_the_skill() {
    let dir = tempdir("resolve_global_fallback");
    let global = dir.join("global");
    let project = dir.join("project");
    let global_skill = write_skill(&global, "only-global", "---\ndescription: g\n---\nbody");
    // project root exists but has no matching skill
    fs::create_dir_all(&project).expect("mk project");
    let resolved =
        resolve_skill_dir_in(&[global.clone(), project.clone()], "only-global").expect("resolved");
    assert_eq!(resolved, global_skill);
}

#[test]
fn resolve_returns_none_when_no_root_has_the_skill() {
    let dir = tempdir("resolve_none");
    let global = dir.join("global");
    let project = dir.join("project");
    fs::create_dir_all(&global).expect("mk global");
    fs::create_dir_all(&project).expect("mk project");
    assert!(resolve_skill_dir_in(&[global, project], "missing").is_none());
}

#[test]
fn resolve_returns_none_when_all_roots_are_missing_dirs() {
    let dir = tempdir("resolve_missing_roots");
    let global = dir.join("does-not-exist-global");
    let project = dir.join("does-not-exist-project");
    assert!(resolve_skill_dir_in(&[global, project], "anything").is_none());
}

#[test]
fn resolve_ignores_a_skill_dir_that_lacks_skill_md() {
    let dir = tempdir("resolve_no_skillmd");
    let root = dir.join("root");
    let bad = root.join("bad");
    fs::create_dir_all(&bad).expect("mk bad dir");
    // No SKILL.md inside.
    assert!(resolve_skill_dir_in(&[root], "bad").is_none());
}

// -----------------------------------------------------------------
// discover_all_in
// -----------------------------------------------------------------

#[test]
fn discover_lists_all_skills_sorted_by_name_deduped_project_over_global() {
    let dir = tempdir("discover_dedup");
    let global = dir.join("global");
    let project = dir.join("project");
    write_skill(&global, "alpha", "---\ndescription: g-alpha\n---\nbody");
    write_skill(&global, "gamma", "---\ndescription: g-gamma\n---\nbody");
    write_skill(&global, "shared", "---\ndescription: g-shared\n---\nbody");
    write_skill(&project, "beta", "---\ndescription: p-beta\n---\nbody");
    write_skill(&project, "shared", "---\ndescription: p-shared\n---\nbody");
    let entries = discover_all_in(&[global, project]);
    let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, vec!["alpha", "beta", "gamma", "shared"]);
    // `shared` should carry the project description.
    let shared = entries.iter().find(|e| e.name == "shared").unwrap();
    assert_eq!(shared.description.as_deref(), Some("p-shared"));
}

#[test]
fn discover_marks_oversized_skills_and_still_lists_them() {
    let dir = tempdir("discover_oversized");
    let root = dir.join("root");
    let big = vec![b'a'; SKILL_MAX_BYTES + 1];
    write_skill(&root, "toobig", std::str::from_utf8(&big).unwrap());
    write_skill(&root, "small", "---\ndescription: ok\n---\nbody");
    let entries = discover_all_in(&[root]);
    assert_eq!(entries.len(), 2);
    let toobig = entries.iter().find(|e| e.name == "toobig").unwrap();
    assert!(toobig.oversized);
    assert!(!toobig.broken);
    assert_eq!(toobig.description, None);
    let small = entries.iter().find(|e| e.name == "small").unwrap();
    assert!(!small.oversized);
    assert!(!small.broken);
    assert_eq!(small.description.as_deref(), Some("ok"));
}

#[test]
fn discover_lists_skill_with_missing_description_as_none() {
    let dir = tempdir("discover_no_desc");
    let root = dir.join("root");
    write_skill(&root, "silent", "no frontmatter here");
    let entries = discover_all_in(&[root]);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].description, None);
    assert!(!entries[0].oversized);
    assert!(!entries[0].broken);
}

#[test]
fn discover_yields_empty_when_no_roots_exist() {
    let dir = tempdir("discover_empty_roots");
    let root = dir.join("missing");
    assert!(discover_all_in(&[root]).is_empty());
}

#[test]
fn discover_yields_empty_when_root_has_no_skill_dirs() {
    let dir = tempdir("discover_empty_root");
    let root = dir.join("root");
    fs::create_dir_all(&root).expect("mk root");
    assert!(discover_all_in(&[root]).is_empty());
}

#[test]
fn discover_skips_dirs_without_a_skill_md() {
    let dir = tempdir("discover_skip_no_skillmd");
    let root = dir.join("root");
    let bogus = root.join("bogus");
    fs::create_dir_all(&bogus).expect("mk bogus");
    // no SKILL.md
    write_skill(&root, "real", "---\ndescription: real\n---\nbody");
    let entries = discover_all_in(&[root]);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "real");
}
