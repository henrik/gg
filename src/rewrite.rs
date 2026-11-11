//! Rewrites one commit in the current branch (new message and/or new tree) while something else (e.g. Claude) may be committing in parallel.
//!
//! The target is pinned by hash when editing starts. Only once the edit is done do we read HEAD, recreate the target and replay every descendant up to HEAD as new commit objects in memory (`git merge-tree`, never touching the working tree), and then move HEAD with a compare-and-swap. If HEAD moved in the meantime, we redo it against the new HEAD. If the target itself was rewritten in the meantime, the change is carried over to its successor.
//!
//! When the replay changes HEAD's tree, the index and working tree are updated with a two-way `read-tree -m -u` while holding the index lock, so nobody can commit in between, and uncommitted changes to unrelated files are kept.

use crate::git::Repo;
use anyhow::{Result, anyhow, bail};
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

#[derive(Debug)]
pub struct Rewritten {
    pub hash: String,

    // Set when the target was rewritten by someone else while being edited, so the edit was carried over to its successor.
    pub followed: Option<Followed>,
}

#[derive(Debug)]
pub struct Followed {
    pub successor: String,
    pub already_there: bool,
}

// If the target was rewritten in the meantime (e.g. amended or rebased by someone else), the change is carried over to the commit it became, as long as that can be done without conflicts.
pub fn rewrite(repo: &Repo, target: &str, change: Change, base: Option<&str>) -> Result<Rewritten> {
    let short = &target[..target.len().min(7)];
    let committer = repo.run(&["var", "GIT_COMMITTER_IDENT"])?;
    let committer = Some(committer.trim());
    let edited = write_commit(repo, target, &parents(repo, target)?, change.tree, committer, change.message)?;

    for _ in 0..MAX_ATTEMPTS {
        let head = repo.rev_parse("HEAD")?;
        let current = if repo.is_ancestor(target, &head) { target.to_string() } else { successor(repo, target, &head, base)? };
        let current_short = &current[..7];
        if let Some(base) = base
            && repo.is_ancestor(&current, base)
        {
            bail!("{current_short} has been pushed to {base}");
        }

        let (new_target, followed) = if current == target {
            (edited.clone(), None)
        } else {
            let new_tree = match change.tree {
                None => None,
                Some(_) => Some(
                    cherry_pick_tree(repo, &edited, target, &current)
                        .map_err(|e| anyhow!("{short} was rewritten as {current_short} meanwhile, and your edits conflict with it: {e:#}"))?,
                ),
            };
            let message = match change.message {
                None => None,
                Some(message) => {
                    let current_message = repo.message(&current)?;
                    if current_message.trim_end() == message.trim_end() {
                        None
                    } else if current_message.trim_end() == repo.message(target)?.trim_end() {
                        Some(message)
                    } else {
                        bail!("{short} was rewritten as {current_short} meanwhile, with a different message than yours");
                    }
                }
            };

            if message.is_none() && new_tree.as_ref().is_none_or(|t| tree(repo, &current).is_ok_and(|c| &c == t)) {
                return Ok(Rewritten { hash: current.clone(), followed: Some(Followed { successor: current, already_there: true }) });
            }
            let new_target = write_commit(repo, &current, &parents(repo, &current)?, new_tree.as_deref(), committer, message)?;
            (new_target, Some(Followed { successor: current.clone(), already_there: false }))
        };
        let new_head = replay(repo, &current, &new_target, &head)?;
        if move_head(repo, &head, &new_head, &format!("gg: rewrite {current_short}"))? {
            return Ok(Rewritten { hash: new_target, followed });
        }
    }

    bail!("HEAD kept moving; gave up after {MAX_ATTEMPTS} attempts")
}

// The commit on the branch that `commit` became after being amended or rebased, recognised by having the same author and author date. Errors if there isn't exactly one.
pub fn successor(repo: &Repo, commit: &str, head: &str, base: Option<&str>) -> Result<String> {
    let short = &commit[..commit.len().min(7)];
    let format = ["--format=%H %an <%ae> %ad%x00%s", "--date=raw"];
    let wanted = repo.run(&["log", "-1", format[0], format[1], commit])?;
    let (_, wanted) = wanted.trim().split_once(' ').unwrap_or_default();
    let (wanted_author, _) = wanted.split_once('\0').unwrap_or_default();

    let mut args = vec!["log", format[0], format[1], head];
    let not_base = base.map(|b| format!("^{b}"));
    if let Some(not_base) = &not_base {
        args.push(not_base);
    }
    let out = repo.run(&args)?;
    let candidates: Vec<(&str, &str)> = out.lines().filter_map(|l| l.split_once(' ')).collect();
    let mut matches: Vec<&str> = candidates.iter().filter(|(_, c)| c.split('\0').next() == Some(wanted_author)).map(|(h, _)| *h).collect();

    // Several commits made in the same second can share an author date.
    if matches.len() > 1 {
        matches = candidates.iter().filter(|(_, c)| *c == wanted).map(|(h, _)| *h).collect();
    }

    match matches.as_slice() {
        [one] => Ok(one.to_string()),
        [] => bail!("{short} is no longer on this branch, and no commit looks like a rewritten version of it"),
        _ => bail!("{short} is no longer on this branch, and several commits look like rewritten versions of it"),
    }
}

// Like `git rebase onto`, but in memory and moving HEAD as `rewrite` does, so there's no stash or half-done rebase for anyone to trip over. Commits whose changes are already in `onto` are dropped. Returns the new hash of each replayed commit (or for a dropped one, of what took its place).
pub fn rebase(repo: &Repo, onto: &str) -> Result<HashMap<String, String>> {
    let onto_hash = repo.rev_parse(onto)?;

    for _ in 0..MAX_ATTEMPTS {
        let head = repo.rev_parse("HEAD")?;
        if repo.is_ancestor(&onto_hash, &head) {
            return Ok(HashMap::new());
        }

        let committer = repo.run(&["var", "GIT_COMMITTER_IDENT"])?;
        let mut mapping = HashMap::new();
        let mut new_head = onto_hash.clone();
        for commit in repo.run(&["rev-list", "--reverse", "--topo-order", &format!("{onto_hash}..{head}")])?.lines() {
            let [old_parent] = parents(repo, commit)?.try_into().map_err(|_| anyhow!("Can't rebase merge commit {}", &commit[..7]))?;
            let new_tree = cherry_pick_tree(repo, commit, &old_parent, &new_head)?;
            let became_empty = new_tree == tree(repo, &new_head)? && tree(repo, &old_parent)? != tree(repo, commit)?;
            if !became_empty {
                new_head = write_commit(repo, commit, std::slice::from_ref(&new_head), Some(&new_tree), Some(committer.trim()), None)?;
            }
            mapping.insert(commit.to_string(), new_head.clone());
        }

        if move_head(repo, &head, &new_head, &format!("gg: rebase onto {onto}"))? {
            return Ok(mapping);
        }
    }

    bail!("HEAD kept moving; gave up after {MAX_ATTEMPTS} attempts")
}

// Swaps `commit` with its parent, replaying any later commits on top. Refuses unless both are unpushed, non-merge commits whose changes are independent: the swapped pair must end up with exactly the same tree, so the code at HEAD doesn't change. Returns the new hashes of `commit` and its parent.
pub fn swap_with_parent(repo: &Repo, commit: &str, base: Option<&str>) -> Result<(String, String)> {
    let short = &commit[..commit.len().min(7)];
    let [parent] = parents(repo, commit)?.try_into().map_err(|_| anyhow!("Can't move merge commit {short}"))?;
    let parent_short = &parent[..7];
    let [grandparent] = parents(repo, &parent)?
        .try_into()
        .map_err(|_| anyhow!("Can't move {short} past {parent_short}, which is a merge or the first commit"))?;

    for _ in 0..MAX_ATTEMPTS {
        let head = repo.rev_parse("HEAD")?;
        if !repo.is_ancestor(commit, &head) {
            bail!("{short} is no longer on this branch (rewritten or reset in the meantime?)");
        }
        if let Some(base) = base
            && repo.is_ancestor(&parent, base)
        {
            bail!("Can't move {short} past {parent_short}, which has been pushed to {base}");
        }

        let committer = repo.run(&["var", "GIT_COMMITTER_IDENT"])?;
        let committer = Some(committer.trim());
        let moved_tree = cherry_pick_tree(repo, commit, &parent, &grandparent)
            .map_err(|_| anyhow!("{short} depends on {parent_short}, so they can't be swapped"))?;
        let new_commit = write_commit(repo, commit, std::slice::from_ref(&grandparent), Some(&moved_tree), committer, None)?;
        let parent_tree = cherry_pick_tree(repo, &parent, &grandparent, &new_commit)
            .map_err(|_| anyhow!("{short} and {parent_short} change the same lines, so they can't be swapped"))?;
        if parent_tree != tree(repo, commit)? {
            bail!("Swapping {short} and {parent_short} would change the resulting code, so not doing it");
        }
        let new_parent = write_commit(repo, &parent, std::slice::from_ref(&new_commit), Some(&parent_tree), committer, None)?;

        let new_head = replay(repo, commit, &new_parent, &head)?;
        if move_head(repo, &head, &new_head, &format!("gg: swap {short} and {parent_short}"))? {
            return Ok((new_commit, new_parent));
        }
    }

    bail!("HEAD kept moving; gave up after {MAX_ATTEMPTS} attempts")
}

// Returns false if HEAD moved in the meantime, meaning we should try again.
fn move_head(repo: &Repo, old: &str, new: &str, message: &str) -> Result<bool> {
    if tree(repo, old)? == tree(repo, new)? {
        Ok(update_head(repo, old, new, message))
    } else {
        update_head_and_worktree(repo, old, new, message)
    }
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
            bail!("{} would conflict in {}", &commit[..7], files.join(", "))
        }
        _ => bail!("git merge-tree failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

fn update_head(repo: &Repo, old: &str, new: &str, message: &str) -> bool {
    repo.ok(&["update-ref", "-m", message, "HEAD", new, old])
}

fn update_head_and_worktree(repo: &Repo, old: &str, new: &str, message: &str) -> Result<bool> {
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

    if !update_head(repo, old, new, message) {
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
        repo.run(&["commit", "-q", "-m", message, &next_date()]).unwrap();
        repo.rev_parse("HEAD").unwrap()
    }

    // Distinct author dates, since that's how rewritten commits are recognised.
    fn next_date() -> String {
        static SECONDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_700_000_000);
        format!("--date=@{}", SECONDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
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
        let new_target = rewrite(&repo, &target, Change { message: Some(message), ..Default::default() }, None).unwrap().hash;

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

        assert!(error.to_string().contains("would conflict in f"), "{error}");
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
    fn carries_amend_over_to_commit_amended_elsewhere() {
        let repo = repo("followamend");
        commit(&repo, "a", "a", "A");
        let target = commit(&repo, "f", "one\ntwo\nthree\nfour\n", "B");
        commit(&repo, "c", "c", "C");

        // Simulates Claude amending B while the user is editing it.
        let elsewhere = rewrite(&repo, &target, Change { tree: Some(&tree_with(&repo, &target, "f", "one\ntwo\nthree\nFOUR\n")), ..Default::default() }, None)
            .unwrap()
            .hash;

        let new_tree = tree_with(&repo, &target, "f", "ONE\ntwo\nthree\nfour\n");
        let rewritten = rewrite(&repo, &target, Change { tree: Some(&new_tree), ..Default::default() }, None).unwrap();

        let followed = rewritten.followed.unwrap();
        assert_eq!(followed.successor, elsewhere);
        assert!(!followed.already_there);
        assert_eq!(log(&repo), "C\nB\nA\n");
        assert_eq!(repo.run(&["show", "HEAD~:f"]).unwrap(), "ONE\ntwo\nthree\nFOUR\n");
        assert_eq!(read(&repo, "f"), "ONE\ntwo\nthree\nFOUR\n");
    }

    #[test]
    fn carries_reword_over_to_commit_amended_elsewhere() {
        let repo = repo("followreword");
        let target = commit(&repo, "f", "one\n", "A");
        repo.run(&["commit", "-q", "--amend", "--no-edit", "--allow-empty"]).unwrap();
        commit(&repo, "f", "two\n", "B");

        rewrite(&repo, &target, Change { message: Some("A reworded\n"), ..Default::default() }, None).unwrap();
        assert_eq!(log(&repo), "B\nA reworded\n");
    }

    #[test]
    fn notices_when_commit_amended_elsewhere_already_has_the_edits() {
        let repo = repo("followsame");
        let target = commit(&repo, "f", "one\n", "A");
        let new_tree = tree_with(&repo, &target, "f", "uno\n");
        std::fs::write(repo.dir.join("f"), "uno\n").unwrap();
        repo.run(&["commit", "-q", "-a", "--amend", "--no-edit"]).unwrap();
        let elsewhere = repo.rev_parse("HEAD").unwrap();

        let rewritten = rewrite(&repo, &target, Change { tree: Some(&new_tree), ..Default::default() }, None).unwrap();

        assert!(rewritten.followed.unwrap().already_there);
        assert_eq!(repo.rev_parse("HEAD").unwrap(), elsewhere);
    }

    #[test]
    fn refuses_to_carry_over_conflicting_edits() {
        let repo = repo("followconflict");
        let target = commit(&repo, "f", "one\n", "A");
        std::fs::write(repo.dir.join("f"), "uno\n").unwrap();
        repo.run(&["commit", "-q", "-a", "--amend", "--no-edit"]).unwrap();
        let elsewhere = repo.rev_parse("HEAD").unwrap();

        let new_tree = tree_with(&repo, &target, "f", "ett\n");
        let error = rewrite(&repo, &target, Change { tree: Some(&new_tree), ..Default::default() }, None).unwrap_err();

        assert!(error.to_string().contains("rewritten as"), "{error}");
        assert_eq!(repo.rev_parse("HEAD").unwrap(), elsewhere);
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

    #[test]
    fn rebases_onto_upstream_keeping_uncommitted_changes_and_dropping_commits_already_upstream() {
        let repo = repo("rebase");
        let base = commit(&repo, "a", "a", "A");
        commit(&repo, "b", "b", "B");
        let mine = commit(&repo, "c", "c", "C");
        let head = commit(&repo, "d", "d", "D");

        repo.run(&["checkout", "-q", &base]).unwrap();
        commit(&repo, "c", "c", "C upstream");
        let upstream = commit(&repo, "e", "e", "E upstream");
        repo.run(&["update-ref", "refs/remotes/origin/main", &upstream]).unwrap();
        repo.run(&["checkout", "-q", "-B", "main", &head]).unwrap();
        std::fs::write(repo.dir.join("a"), "uncommitted").unwrap();

        let mapping = rebase(&repo, "origin/main").unwrap();

        assert_eq!(log(&repo), "D\nB\nE upstream\nC upstream\nA\n");
        assert_eq!(mapping[&mine], repo.rev_parse("HEAD~").unwrap());
        assert_eq!(read(&repo, "e"), "e");
        assert_eq!(read(&repo, "a"), "uncommitted");
        assert_eq!(repo.run(&["status", "--porcelain"]).unwrap(), " M a\n");
    }

    #[test]
    fn swaps_independent_commits_keeping_later_ones() {
        let repo = repo("swap");
        commit(&repo, "a", "a", "A");
        commit(&repo, "b", "b", "B");
        let c = commit(&repo, "c", "c", "C");
        commit(&repo, "d", "d", "D");
        let tree_before = tree(&repo, "HEAD").unwrap();

        let (new_c, _) = swap_with_parent(&repo, &c, None).unwrap();

        assert_eq!(log(&repo), "D\nB\nC\nA\n");
        assert_eq!(repo.rev_parse("HEAD~2").unwrap(), new_c);
        assert_eq!(tree(&repo, "HEAD").unwrap(), tree_before);
    }

    #[test]
    fn refuses_to_swap_dependent_commits() {
        let repo = repo("swap-dependent");
        commit(&repo, "a", "a", "A");
        commit(&repo, "f", "one\n", "B");
        let c = commit(&repo, "f", "two\n", "C");

        let error = swap_with_parent(&repo, &c, None).unwrap_err();

        assert!(error.to_string().contains("depends on"), "{error}");
        assert_eq!(repo.rev_parse("HEAD").unwrap(), c);
    }

    #[test]
    fn refuses_to_swap_with_pushed_commit() {
        let repo = repo("swap-pushed");
        commit(&repo, "a", "a", "A");
        let b = commit(&repo, "b", "b", "B");
        repo.run(&["update-ref", "refs/remotes/origin/main", &b]).unwrap();
        let c = commit(&repo, "c", "c", "C");

        assert!(swap_with_parent(&repo, &c, Some("origin/main")).is_err());
        assert_eq!(repo.rev_parse("HEAD").unwrap(), c);
    }

    #[test]
    fn refuses_rebase_that_conflicts() {
        let repo = repo("rebase-conflict");
        let base = commit(&repo, "f", "one\n", "A");
        let head = commit(&repo, "f", "two\n", "B");
        repo.run(&["checkout", "-q", &base]).unwrap();
        let upstream = commit(&repo, "f", "uno\n", "A upstream");
        repo.run(&["update-ref", "refs/remotes/origin/main", &upstream]).unwrap();
        repo.run(&["checkout", "-q", "-B", "main", &head]).unwrap();

        let error = rebase(&repo, "origin/main").unwrap_err();

        assert!(error.to_string().contains("would conflict in f"), "{error}");
        assert_eq!(repo.rev_parse("HEAD").unwrap(), head);
        assert_eq!(read(&repo, "f"), "two\n");
    }
}
