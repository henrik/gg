//! Rewrites one commit in the current branch (new message and/or new tree) while something else (e.g. Claude) may be committing in parallel.
//!
//! The target is pinned by hash when editing starts. Only once the edit is done do we read HEAD, recreate the target and replay every descendant up to HEAD as new commit objects in memory (`git merge-tree`, never touching the working tree), and then move HEAD with a compare-and-swap. If HEAD moved in the meantime, we redo it against the new HEAD.
//!
//! When the replay changes HEAD's tree, the index and working tree are updated with a two-way `read-tree -m -u` while holding the index lock, so nobody can commit in between, and uncommitted changes to unrelated files are kept.

use crate::git::Repo;
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: usize = 20;

#[derive(Default)]
pub struct Change<'a> {
    pub message: Option<&'a str>,
    pub tree: Option<&'a str>,
}

// Returns the hash of the rewritten commit.
pub fn rewrite(repo: &Repo, target: &str, change: Change, base: Option<&str>) -> Result<String> {
    let short = &target[..target.len().min(7)];

    for _ in 0..MAX_ATTEMPTS {
        let head = repo.rev_parse("HEAD")?;
        if !repo.is_ancestor(target, &head) {
            bail!("{short} is no longer on this branch (rewritten or reset in the meantime?)");
        }
        if let Some(base) = base
            && repo.is_ancestor(target, base)
        {
            bail!("{short} has been pushed to {base}");
        }

        let committer = repo.run(&["var", "GIT_COMMITTER_IDENT"])?;
        let new_target = write_commit(
            repo,
            target,
            &parents(repo, target)?,
            change.tree,
            Some(committer.trim()),
            change.message,
        )?;
        let new_head = replay(repo, target, &new_target, &head)?;

        let moved = if tree(repo, &head)? == tree(repo, &new_head)? {
            update_head(repo, &head, &new_head, short)
        } else {
            update_head_and_worktree(repo, &head, &new_head, short)?
        };
        if moved {
            return Ok(new_target);
        }
    }

    bail!("HEAD kept moving; gave up after {MAX_ATTEMPTS} attempts")
}

// Recreates the commits in old_target..head on top of new_target. Returns the new head.
fn replay(repo: &Repo, old_target: &str, new_target: &str, head: &str) -> Result<String> {
    let mut mapping = HashMap::from([(old_target.to_string(), new_target.to_string())]);
    let range = format!("{old_target}..{head}");

    for commit in repo.run(&["rev-list", "--reverse", "--topo-order", &range])?.lines() {
        let old_parents = parents(repo, commit)?;
        let new_parents: Vec<String> =
            old_parents.iter().map(|p| mapping.get(p).cloned().unwrap_or_else(|| p.clone())).collect();

        let mut trees_unchanged = true;
        for (old, new) in old_parents.iter().zip(&new_parents) {
            trees_unchanged &= old == new || tree(repo, old)? == tree(repo, new)?;
        }

        let new_tree = if trees_unchanged {
            None
        } else if let [old_parent] = old_parents.as_slice() {
            Some(cherry_pick_tree(repo, commit, old_parent, &new_parents[0])?)
        } else {
            bail!("Can't replay merge commit {} after changing an earlier commit", &commit[..7]);
        };

        let new_commit = write_commit(repo, commit, &new_parents, new_tree.as_deref(), None, None)?;
        mapping.insert(commit.to_string(), new_commit);
    }

    Ok(mapping[head].clone())
}

fn cherry_pick_tree(repo: &Repo, commit: &str, old_parent: &str, new_parent: &str) -> Result<String> {
    let base = format!("--merge-base={old_parent}");
    let out = repo.command().args(["merge-tree", "--write-tree", "--name-only", &base, new_parent, commit]).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let tree = lines.next().unwrap_or("").to_string();

    match out.status.code() {
        Some(0) => Ok(tree),
        Some(1) => {
            let files: Vec<&str> = lines.take_while(|l| !l.is_empty()).collect();
            bail!("Conflicts with later commit {} in {}", &commit[..7], files.join(", "))
        }
        _ => bail!("git merge-tree failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

fn update_head(repo: &Repo, old: &str, new: &str, short: &str) -> bool {
    repo.ok(&["update-ref", "-m", &format!("gg: rewrite {short}"), "HEAD", new, old])
}

// Returns false if HEAD moved before we got the lock, meaning we should try again.
fn update_head_and_worktree(repo: &Repo, old: &str, new: &str, short: &str) -> Result<bool> {
    let index = repo.git_path("index")?;
    let _lock = IndexLock::acquire(&index)?;
    if repo.rev_parse("HEAD")? != old {
        return Ok(false);
    }

    // Git locks $GIT_INDEX_FILE.lock, so working on a copy lets us hold the real lock throughout.
    let scratch = repo.git_path("gg-index")?;
    if index.exists() {
        std::fs::copy(&index, &scratch)?;
    }
    if let Err(e) = repo.run_with_index(&scratch, &["read-tree", "-m", "-u", old, new]) {
        let _ = std::fs::remove_file(&scratch);
        bail!("Uncommitted changes are in the way: {e}");
    }

    if !update_head(repo, old, new, short) {
        repo.run_with_index(&scratch, &["read-tree", "-m", "-u", new, old])?;
        std::fs::remove_file(&scratch)?;
        return Ok(false);
    }
    std::fs::rename(&scratch, &index)?;
    Ok(true)
}

struct IndexLock(PathBuf);

impl IndexLock {
    fn acquire(index: &std::path::Path) -> Result<Self> {
        let path = index.with_extension("lock");
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && started.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => bail!("Could not lock {}: {e}", path.display()),
            }
        }
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn parents(repo: &Repo, commit: &str) -> Result<Vec<String>> {
    let out = repo.run(&["rev-list", "--parents", "-n1", commit])?;
    Ok(out.split_whitespace().skip(1).map(String::from).collect())
}

fn tree(repo: &Repo, commit: &str) -> Result<String> {
    Ok(repo.run(&["rev-parse", &format!("{commit}^{{tree}}")])?.trim().to_string())
}

// Writes a copy of `commit` with the given changes, keeping everything else byte for byte.
fn write_commit(
    repo: &Repo,
    commit: &str,
    new_parents: &[String],
    new_tree: Option<&str>,
    new_committer: Option<&str>,
    new_message: Option<&str>,
) -> Result<String> {
    let raw = repo.run_bytes(&["cat-file", "commit", commit], None)?;
    let split = raw.windows(2).position(|w| w == b"\n\n").map(|i| i + 1).unwrap_or(raw.len());
    let (headers, body) = raw.split_at(split);
    let body = body.strip_prefix(b"\n").unwrap_or(body);

    let mut out = Vec::with_capacity(raw.len());
    let mut skipping_signature = false;
    for line in headers.split_inclusive(|&b| b == b'\n') {
        if skipping_signature && line.starts_with(b" ") {
            continue;
        }
        skipping_signature = false;

        // Signatures would no longer be valid.
        if line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 ") {
            skipping_signature = true;
        } else if line.starts_with(b"parent ") {
        } else if line.starts_with(b"tree ") {
            match new_tree {
                Some(tree) => out.extend_from_slice(format!("tree {tree}\n").as_bytes()),
                None => out.extend_from_slice(line),
            }
            for parent in new_parents {
                out.extend_from_slice(format!("parent {parent}\n").as_bytes());
            }
        } else if let Some(committer) = new_committer
            && line.starts_with(b"committer ")
        {
            out.extend_from_slice(format!("committer {committer}\n").as_bytes());
        } else {
            out.extend_from_slice(line);
        }
    }

    out.push(b'\n');
    out.extend_from_slice(new_message.map(str::as_bytes).unwrap_or(body));

    let hash = repo.run_bytes(&["hash-object", "-t", "commit", "-w", "--stdin"], Some(&out))?;
    Ok(String::from_utf8_lossy(&hash).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("gg-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo { dir };
        repo.run(&["init", "-q", "-b", "main"]).unwrap();
        repo.run(&["config", "user.name", "Test"]).unwrap();
        repo.run(&["config", "user.email", "test@example.com"]).unwrap();
        repo
    }

    fn commit(repo: &Repo, file: &str, content: &str, message: &str) -> String {
        std::fs::write(repo.dir.join(file), content).unwrap();
        repo.run(&["add", file]).unwrap();
        repo.run(&["commit", "-q", "-m", message]).unwrap();
        repo.rev_parse("HEAD").unwrap()
    }

    fn log(repo: &Repo) -> String {
        repo.run(&["log", "--format=%s"]).unwrap()
    }

    fn read(repo: &Repo, file: &str) -> String {
        std::fs::read_to_string(repo.dir.join(file)).unwrap()
    }

    // A tree like `commit`'s, but with `file` set to `content`.
    fn tree_with(repo: &Repo, commit: &str, file: &str, content: &str) -> String {
        let index = repo.dir.join(".git/test-index");
        repo.run_with_index(&index, &["read-tree", commit]).unwrap();
        let blob = repo.run_bytes(&["hash-object", "-w", "--stdin"], Some(content.as_bytes())).unwrap();
        let blob = String::from_utf8(blob).unwrap();
        repo.run_with_index(&index, &["update-index", "--add", "--cacheinfo", &format!("100644,{},{file}", blob.trim())])
            .unwrap();
        repo.run_with_index(&index, &["write-tree"]).unwrap().trim().to_string()
    }

    #[test]
    fn rewords_target_and_keeps_commits_added_after_editing_started() {
        let repo = repo("reword");
        commit(&repo, "a", "a", "A");
        let target = commit(&repo, "b", "b", "B");
        commit(&repo, "c", "c", "C");

        // Simulates another process working while the user is in the editor.
        std::fs::write(repo.dir.join("dirty"), "uncommitted").unwrap();
        commit(&repo, "d", "d", "D");

        let message = "B reworded\n\nWith body.\n";
        let new_target = rewrite(&repo, &target, Change { message: Some(message), ..Default::default() }, None).unwrap();

        assert_eq!(log(&repo), "D\nC\nB reworded\nA\n");
        assert_eq!(repo.message(&new_target).unwrap(), "B reworded\n\nWith body.\n\n");
        assert_eq!(read(&repo, "dirty"), "uncommitted");
        assert_eq!(repo.run(&["status", "--porcelain"]).unwrap(), "?? dirty\n");
    }

    #[test]
    fn amends_earlier_commit_and_updates_worktree_keeping_unrelated_changes() {
        let repo = repo("amend");
        commit(&repo, "a", "a", "A");
        let target = commit(&repo, "f", "one\ntwo\nthree\nfour\nfive\n", "B");
        commit(&repo, "f", "one\ntwo\nthree\nfour\nfive changed later\n", "C");

        std::fs::write(repo.dir.join("a"), "uncommitted").unwrap();
        commit(&repo, "d", "d", "D");

        let new_tree = tree_with(&repo, &target, "f", "ONE\ntwo\nthree\nfour\nfive\n");
        rewrite(&repo, &target, Change { tree: Some(&new_tree), ..Default::default() }, None).unwrap();

        assert_eq!(log(&repo), "D\nC\nB\nA\n");
        assert_eq!(repo.run(&["show", "HEAD~2:f"]).unwrap(), "ONE\ntwo\nthree\nfour\nfive\n");
        assert_eq!(read(&repo, "f"), "ONE\ntwo\nthree\nfour\nfive changed later\n");
        assert_eq!(read(&repo, "a"), "uncommitted");
        assert_eq!(repo.run(&["status", "--porcelain"]).unwrap(), " M a\n");
    }

    #[test]
    fn refuses_amend_that_conflicts_with_later_commit() {
        let repo = repo("conflict");
        let target = commit(&repo, "f", "one\n", "A");
        let head = commit(&repo, "f", "two\n", "B");

        let new_tree = tree_with(&repo, &target, "f", "uno\n");
        let error = rewrite(&repo, &target, Change { tree: Some(&new_tree), ..Default::default() }, None).unwrap_err();

        assert!(error.to_string().contains("Conflicts with later commit"), "{error}");
        assert_eq!(repo.rev_parse("HEAD").unwrap(), head);
        assert_eq!(read(&repo, "f"), "two\n");
    }

    #[test]
    fn refuses_amend_when_uncommitted_changes_are_in_the_way() {
        let repo = repo("dirty");
        let target = commit(&repo, "f", "one\n", "A");
        let head = commit(&repo, "g", "g\n", "B");
        std::fs::write(repo.dir.join("f"), "uncommitted\n").unwrap();

        let new_tree = tree_with(&repo, &target, "f", "uno\n");
        assert!(rewrite(&repo, &target, Change { tree: Some(&new_tree), ..Default::default() }, None).is_err());

        assert_eq!(repo.rev_parse("HEAD").unwrap(), head);
        assert_eq!(read(&repo, "f"), "uncommitted\n");
        assert!(!repo.dir.join(".git/index.lock").exists());
    }

    #[test]
    fn refuses_when_target_is_gone() {
        let repo = repo("gone");
        commit(&repo, "a", "a", "A");
        let target = commit(&repo, "b", "b", "B");
        repo.run(&["commit", "-q", "--amend", "-m", "B amended elsewhere"]).unwrap();

        assert!(rewrite(&repo, &target, Change { message: Some("B reworded\n"), ..Default::default() }, None).is_err());
        assert_eq!(log(&repo), "B amended elsewhere\nA\n");
    }

    #[test]
    fn refuses_pushed_commits() {
        let repo = repo("pushed");
        let target = commit(&repo, "a", "a", "A");
        repo.run(&["update-ref", "refs/remotes/origin/main", &target]).unwrap();
        commit(&repo, "b", "b", "B");

        let change = Change { message: Some("A reworded\n"), ..Default::default() };
        assert!(rewrite(&repo, &target, change, Some("origin/main")).is_err());
    }
}
