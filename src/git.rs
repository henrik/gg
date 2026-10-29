use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_UNTRACKED_DIFFS: usize = 50;

pub struct Repo {
    pub dir: PathBuf,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Commit,
    Unstaged,
    Staged,
}

// A commit, or a pseudo-entry for uncommitted changes.
#[derive(Clone)]
pub struct Commit {
    pub kind: Kind,
    pub hash: String,
    pub short: String,
    pub date: String,
    pub authors: Vec<String>,
    pub summary: String,
    pub pushed: bool,
    // No longer on the branch, but kept around while focused.
    pub stale: bool,
}

impl Commit {
    fn uncommitted(kind: Kind, authors: &[String], summary: String) -> Self {
        let label = if kind == Kind::Staged { "Staged" } else { "Unstaged" };
        Self {
            kind,
            hash: format!("gg:{}", label.to_lowercase()),
            short: label.into(),
            date: String::new(),
            authors: authors.to_vec(),
            summary,
            pushed: false,
            stale: false,
        }
    }
}

impl Repo {
    pub fn discover() -> Result<Self> {
        let here = Self { dir: PathBuf::from(".") };
        let top = here.run(&["rev-parse", "--show-toplevel"]).context("Not inside a git repository")?;
        Ok(Self { dir: PathBuf::from(top.trim()) })
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.dir);
        cmd
    }

    pub fn run_bytes(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        self.exec(self.command(), args, input)
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.run_bytes(args, None)?).into_owned())
    }

    pub fn run_with_index(&self, index: &Path, args: &[&str]) -> Result<String> {
        let mut cmd = self.command();
        cmd.env("GIT_INDEX_FILE", index);
        Ok(String::from_utf8_lossy(&self.exec(cmd, args, None)?).into_owned())
    }

    fn exec(&self, mut cmd: Command, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut child = cmd
            .args(args)
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Could not run git {}", args.join(" ")))?;
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    }

    pub fn git_path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.dir.join(self.run(&["rev-parse", "--git-path", name])?.trim()))
    }

    pub fn ok(&self, args: &[&str]) -> bool {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        Ok(self.run(&["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])?.trim().to_string())
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        self.ok(&["merge-base", "--is-ancestor", ancestor, descendant])
    }

    pub fn branch(&self) -> Option<String> {
        let out = self.run(&["symbolic-ref", "--short", "-q", "HEAD"]).ok()?;
        Some(out.trim().to_string()).filter(|s| !s.is_empty())
    }

    // The remote branch we rebase onto, e.g. "origin/main".
    pub fn default_remote_branch(&self) -> Option<String> {
        if let Ok(out) = self.run(&["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"]) {
            return Some(out.trim().to_string());
        }
        ["origin/main", "origin/master"].into_iter().find(|r| self.rev_parse(r).is_ok()).map(String::from)
    }

    // A linked worktree (not the main checkout), whose branch is integrated straight into the default branch.
    pub fn is_linked_worktree(&self) -> bool {
        let dirs = self.run(&["rev-parse", "--absolute-git-dir", "--git-common-dir"]).unwrap_or_default();
        let mut dirs = dirs.lines().map(|d| self.dir.join(d).canonicalize().ok());
        let (git_dir, common_dir) = (dirs.next().flatten(), dirs.next().flatten());
        git_dir.is_some() && git_dir != common_dir
    }

    // What "pushed" is measured against: the default remote branch in a linked worktree, else the branch's upstream, else the default remote branch.
    pub fn base_ref(&self) -> Option<String> {
        if self.is_linked_worktree() {
            return self.default_remote_branch();
        }
        self.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| self.default_remote_branch())
    }

    pub fn ahead_behind(&self, base: &str) -> (usize, usize) {
        let out = self.run(&["rev-list", "--left-right", "--count", &format!("HEAD...{base}")]).unwrap_or_default();
        let mut parts = out.split_whitespace().map(|n| n.parse().unwrap_or(0));
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
    }

    pub fn commits(&self, base: Option<&str>, extra_pushed: usize) -> Result<Vec<Commit>> {
        let unpushed: HashSet<String> = match base {
            Some(base) => self.run(&["rev-list", &format!("{base}..HEAD")])?.lines().map(String::from).collect(),
            None => HashSet::new(),
        };
        let limit = format!("--max-count={}", unpushed.len() + extra_pushed);
        let format = "--format=%H%x00%h%x00%ad%x00%an%x00%s%x00%(trailers:key=Co-authored-by,valueonly,separator=%x1f)%x1e";
        let out = self.run(&["log", "--topo-order", &limit, "--date=format:%Y-%m-%d %H:%M", format, "HEAD"])?;

        Ok(out
            .split('\x1e')
            .map(|r| r.trim_start_matches('\n'))
            .filter(|r| !r.is_empty())
            .map(|record| {
                let f: Vec<&str> = record.split('\0').collect();
                let mut authors = vec![f[3].to_string()];
                for co in f.get(5).unwrap_or(&"").split('\x1f') {
                    let name = co.split('<').next().unwrap_or("").trim();
                    if !name.is_empty() && !authors.iter().any(|a| a == name) {
                        authors.push(name.to_string());
                    }
                }
                Commit {
                    kind: Kind::Commit,
                    hash: f[0].to_string(),
                    short: f[1].to_string(),
                    date: f[2].to_string(),
                    authors,
                    summary: f[4].to_string(),
                    pushed: base.is_some() && !unpushed.contains(f[0]),
                    stale: false,
                }
            })
            .collect())
    }

    // Entries for unstaged (including untracked) and staged changes, each only if non-empty.
    pub fn uncommitted(&self) -> Vec<Commit> {
        let authors = self.current_authors();
        let untracked = self.untracked().len();
        let mut entries = Vec::new();

        let unstaged_stat = self.run(&["diff", "--shortstat"]).unwrap_or_default();
        if !unstaged_stat.trim().is_empty() || untracked > 0 {
            let mut summary = unstaged_stat.trim().to_string();
            if untracked > 0 {
                let files = if untracked == 1 { "1 untracked file".to_string() } else { format!("{untracked} untracked files") };
                summary = if summary.is_empty() { files } else { format!("{summary}, {files}") };
            }
            entries.push(Commit::uncommitted(Kind::Unstaged, &authors, summary));
        }

        let staged_stat = self.run(&["diff", "--cached", "--shortstat"]).unwrap_or_default();
        if !staged_stat.trim().is_empty() {
            entries.push(Commit::uncommitted(Kind::Staged, &authors, staged_stat.trim().to_string()));
        }
        entries
    }

    // Who a commit made now would be by: the configured author plus any co-authors in the commit template.
    fn current_authors(&self) -> Vec<String> {
        let ident = self.run(&["var", "GIT_AUTHOR_IDENT"]).unwrap_or_default();
        let mut authors = vec![ident.split('<').next().unwrap_or("").trim().to_string()];

        let template = self.run(&["config", "--path", "commit.template"]).unwrap_or_default();
        if let Ok(content) = std::fs::read_to_string(self.dir.join(template.trim())) {
            for line in content.lines() {
                if let Some((key, value)) = line.split_once(':')
                    && key.trim().eq_ignore_ascii_case("co-authored-by")
                {
                    authors.push(value.split('<').next().unwrap_or("").trim().to_string());
                }
            }
        }
        authors.retain(|a| !a.is_empty());
        authors
    }

    fn untracked(&self) -> Vec<String> {
        let out = self.run_bytes(&["ls-files", "--others", "--exclude-standard", "-z"], None).unwrap_or_default();
        String::from_utf8_lossy(&out).split('\0').filter(|p| !p.is_empty()).map(String::from).collect()
    }

    // Changes in status, file contents or the index, for noticing uncommitted changes.
    pub fn worktree_fingerprint(&self) -> String {
        let status = self.run_bytes(&["status", "--porcelain=v1", "-z"], None).unwrap_or_default();
        let status = String::from_utf8_lossy(&status).into_owned();
        let mut out = status.clone();
        let paths = status.split('\0').filter_map(|entry| entry.get(3..)).chain(std::iter::once(""));
        for path in paths {
            let path = if path.is_empty() { self.git_path("index").unwrap_or_default() } else { self.dir.join(path) };
            if let Ok(meta) = std::fs::metadata(&path) {
                out += &format!("{:?}{}", meta.modified().ok(), meta.len());
            }
        }
        out
    }

    pub fn show_entry(&self, entry: &Commit) -> Result<Vec<u8>> {
        match entry.kind {
            Kind::Commit => self.show(&entry.hash),
            Kind::Staged => self.run_bytes(&["diff", "--cached", "--color=always", "--stat", "--patch"], None),
            Kind::Unstaged => {
                let mut out = self.run_bytes(&["diff", "--color=always", "--stat", "--patch"], None)?;
                let untracked = self.untracked();
                for path in untracked.iter().take(MAX_UNTRACKED_DIFFS) {
                    // Exits with 1 when there are differences, which there always are.
                    let diff = self.command().args(["diff", "--color=always", "--no-index", "--", "/dev/null", path]).output()?;
                    out.push(b'\n');
                    out.extend_from_slice(&diff.stdout);
                }
                if untracked.len() > MAX_UNTRACKED_DIFFS {
                    out.extend_from_slice(format!("\n…and {} more untracked files\n", untracked.len() - MAX_UNTRACKED_DIFFS).as_bytes());
                }
                Ok(out)
            }
        }
    }

    pub fn show(&self, hash: &str) -> Result<Vec<u8>> {
        self.run_bytes(
            &[
                "show",
                "--color=always",
                "--stat",
                "--patch",
                "--format=%C(yellow)commit %H%C(reset)%nAuthor: %an <%ae>%nDate:   %ad%n%n%B",
                hash,
            ],
            None,
        )
    }

    pub fn message(&self, hash: &str) -> Result<String> {
        self.run(&["log", "-1", "--format=%B", hash])
    }

    pub fn rebase_in_progress(&self) -> bool {
        ["rebase-merge", "rebase-apply"].iter().any(|d| self.git_path(d).is_ok_and(|p| p.exists()))
    }

    pub fn comment_char(&self) -> String {
        let configured = self.run(&["config", "core.commentChar"]).map(|s| s.trim().to_string()).unwrap_or_default();
        if configured.is_empty() || configured == "auto" { "#".into() } else { configured }
    }

    pub fn editor(&self) -> String {
        self.run(&["var", "GIT_EDITOR"]).map(|s| s.trim().to_string()).unwrap_or_else(|_| "vim".into())
    }
}
