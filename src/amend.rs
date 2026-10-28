//! Amending file contents of any commit on the branch.
//!
//! The non-blocking variant materialises the commit's changed files (as of that commit) in a scratch directory, lets the user edit them there, and builds a new tree from the result for `rewrite`. The real repo is only touched at the very end.

use crate::git::Repo;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

pub struct ChangedFile {
    pub path: String,
    pub first_line: usize,
}

pub struct Scratch {
    pub commit: String,
    pub root: PathBuf,
    pub files: Vec<ChangedFile>,
}

impl Scratch {
    pub fn files_dir(&self) -> PathBuf {
        self.root.join("files")
    }

    pub fn prepare(repo: &Repo, commit: &str) -> Result<Self> {
        let root = std::env::temp_dir().join(format!("gg-amend-{}-{}", std::process::id(), &commit[..7]));
        let _ = std::fs::remove_dir_all(&root);
        let modes = modes(repo, commit)?;
        let mut files = changed_files(repo, commit)?;
        files.retain(|f| modes.get(&f.path).is_some_and(|m| m.starts_with("100")));
        let scratch = Self { commit: commit.to_string(), root, files };

        for file in &scratch.files {
            let content = repo.run_bytes(&["cat-file", "blob", &format!("{commit}:{}", file.path)], None)?;
            let dest = scratch.files_dir().join(&file.path);
            std::fs::create_dir_all(dest.parent().unwrap())?;
            std::fs::write(&dest, content)?;
        }
        std::fs::create_dir_all(scratch.files_dir())?;
        Ok(scratch)
    }

    // Returns None if nothing changed.
    pub fn build_tree(&self, repo: &Repo) -> Result<Option<String>> {
        let index = self.root.join("index");
        let _ = std::fs::remove_file(&index);
        repo.run_with_index(&index, &["read-tree", &self.commit])?;
        let modes = modes(repo, &self.commit)?;

        let dir = self.files_dir();
        let mut present = Vec::new();
        walk(&dir, &mut present)?;
        for file in &present {
            let path = file.strip_prefix(&dir)?.to_string_lossy().into_owned();
            let blob = repo.run(&["hash-object", "-w", "--", &file.to_string_lossy()])?;
            let mode = modes.get(&path).cloned().unwrap_or_else(|| {
                use std::os::unix::fs::PermissionsExt;
                let executable = std::fs::metadata(file).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
                if executable { "100755" } else { "100644" }.to_string()
            });
            repo.run_with_index(&index, &["update-index", "--add", "--cacheinfo", &format!("{mode},{},{path}", blob.trim())])?;
        }
        for file in &self.files {
            if !dir.join(&file.path).exists() && modes.contains_key(&file.path) {
                repo.run_with_index(&index, &["update-index", "--force-remove", "--", &file.path])?;
            }
        }

        let tree = repo.run_with_index(&index, &["write-tree"])?.trim().to_string();
        let old_tree = repo.run(&["rev-parse", &format!("{}^{{tree}}", self.commit)])?;
        Ok((tree != old_tree.trim()).then_some(tree))
    }

    pub fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// The commit's patch in a form `git apply` accepts regardless of the user's diff config.
pub fn patch(repo: &Repo, commit: &str) -> Result<String> {
    repo.run(&[
        "show",
        "--format=",
        "--patch",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        commit,
    ])
}

pub fn same_ignoring_trailing_whitespace(a: &str, b: &str) -> bool {
    a.trim_end().lines().map(str::trim_end).eq(b.trim_end().lines().map(str::trim_end))
}

// Applies an edited version of the commit's patch to its parent, like `git add -e`.
pub fn tree_from_patch(repo: &Repo, commit: &str, patch: &str, scratch_dir: &Path) -> Result<String> {
    if repo.run(&["rev-list", "--parents", "-n1", commit])?.split_whitespace().count() > 2 {
        bail!("can't edit the diff of a merge commit");
    }
    let index = scratch_dir.join("index");
    let _ = std::fs::remove_file(&index);
    match repo.rev_parse(&format!("{commit}^")) {
        Ok(parent) => repo.run_with_index(&index, &["read-tree", &parent])?,
        Err(_) => repo.run_with_index(&index, &["read-tree", "--empty"])?,
    };

    let patch_path = scratch_dir.join("edited.patch");
    std::fs::write(&patch_path, format!("{}\n", patch.trim_end()))?;
    repo.run_with_index(&index, &["apply", "--cached", "--recount", "--ignore-whitespace", &patch_path.to_string_lossy()])
        .context("the edited diff doesn't apply")?;
    Ok(repo.run_with_index(&index, &["write-tree"])?.trim().to_string())
}

// Files the commit adds or modifies (not deletes), with the first changed line of each.
pub fn changed_files(repo: &Repo, commit: &str) -> Result<Vec<ChangedFile>> {
    let out = repo.run(&[
        "-c",
        "core.quotePath=false",
        "show",
        "--format=",
        "--no-color",
        "--no-ext-diff",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "-U0",
        commit,
    ])?;

    let mut files: Vec<ChangedFile> = Vec::new();
    let mut awaiting_hunk = false;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            files.push(ChangedFile { path: path.to_string(), first_line: 1 });
            awaiting_hunk = true;
        } else if line.starts_with("+++ ") {
            awaiting_hunk = false;
        } else if awaiting_hunk && line.starts_with("@@ ") {
            // E.g. "@@ -3,2 +4,5 @@".
            let new_range = line.split_whitespace().nth(2).unwrap_or("+1");
            let start = new_range.trim_start_matches('+').split(',').next().unwrap_or("1");
            files.last_mut().unwrap().first_line = start.parse().unwrap_or(1).max(1);
            awaiting_hunk = false;
        }
    }
    Ok(files)
}

fn modes(repo: &Repo, commit: &str) -> Result<HashMap<String, String>> {
    let out = repo.run_bytes(&["ls-tree", "-r", "-z", commit], None)?;
    Ok(String::from_utf8_lossy(&out)
        .split('\0')
        .filter_map(|entry| {
            let (meta, path) = entry.split_once('\t')?;
            Some((path.to_string(), meta.split(' ').next()?.to_string()))
        })
        .collect())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let editor_junk = name.ends_with('~') || (name.starts_with('.') && (name.ends_with(".swp") || name.ends_with(".swo")));
        if path.is_dir() {
            walk(&path, out)?;
        } else if !editor_junk {
            out.push(path);
        }
    }
    Ok(())
}

// Opens the files in tabs, each scrolled to its first change, when the editor is Vim-like.
pub fn edit_files(editor: &str, cwd: &Path, files: &[ChangedFile], script_dir: &Path) -> Result<ExitStatus> {
    let mut cmd = Command::new("sh");
    cmd.current_dir(cwd);
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();

    if editor.contains("vim") {
        let lines: Vec<String> = files.iter().map(|f| f.first_line.to_string()).collect();
        let script = format!(
            "let s:lines = [{}]\nfor s:i in range(len(s:lines))\n  execute (s:i + 1) . 'tabnext'\n  call cursor(s:lines[s:i], 1)\n  normal! zz\nendfor\ntabfirst\n",
            lines.join(", "),
        );
        let script_path = script_dir.join("gg-goto-changes.vim");
        std::fs::write(&script_path, script)?;
        cmd.arg("-c")
            .arg(format!("{editor} --cmd 'set tabpagemax=200' -p -S \"$0\" \"$@\""))
            .arg(&script_path)
            .args(&paths);
    } else {
        cmd.arg("-c").arg(format!("{editor} \"$@\"")).arg(editor).args(&paths);
    }

    let status = cmd.status().context("Could not start editor")?;
    if !status.success() {
        bail!("Editor exited with {status}");
    }
    Ok(status)
}
