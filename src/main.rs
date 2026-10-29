mod amend;
mod git;
mod rewrite;

use ansi_to_tui::IntoText;
use anyhow::{Result, bail};
use git::{Commit, Kind, Repo};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::{
        event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
        execute,
    },
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::collections::HashMap;
use unicode_width::UnicodeWidthChar;
use std::io::Write;
use std::time::{Duration, Instant};

const PUSHED_COMMITS_SHOWN: usize = 10;
const MIN_PANE_WIDTH: u16 = 30;
const MOUSE_SCROLL_ROWS: usize = 3;
const STALE_GRACE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const DIM: Color = Color::DarkGray;
const SELECTED_BG: Color = Color::Indexed(237);

struct PendingPush {
    rev: String,
    remote: String,
    // Set when pushing from a linked worktree, whose branch isn't what gets pushed to.
    worktree_branch: Option<String>,
    branch: String,
    commits: Vec<String>,
}

struct App {
    repo: Repo,
    branch: Option<String>,
    base: Option<String>,
    ahead: usize,
    behind: usize,
    commits: Vec<Commit>,
    list: ListState,
    diffs: HashMap<String, Text<'static>>,
    diff_scroll: usize,
    diff_height: usize,
    diff_width: usize,
    wrapped: Option<(String, usize, Text<'static>)>,
    // Row ranges (end exclusive) of the wrapped diff's hunks.
    hunks: Vec<(usize, usize)>,
    jumped_to_hunk: Option<usize>,
    commits_area: ratatui::layout::Rect,
    pending_push: Option<PendingPush>,
    // Set once the user drags the separator.
    list_width: Option<u16>,
    dragging_separator: bool,
    // When each stale commit stopped being focused.
    stale_unfocused_at: HashMap<String, Instant>,
    main_area: ratatui::layout::Rect,
    status: Option<(String, bool)>,
    pending_amend: Option<amend::Scratch>,
    fingerprint: String,
    last_poll: Instant,
}

fn main() -> Result<()> {
    let repo = Repo::discover()?;
    let mut app = App {
        repo,
        branch: None,
        base: None,
        ahead: 0,
        behind: 0,
        commits: vec![],
        list: ListState::default(),
        diffs: HashMap::new(),
        diff_scroll: 0,
        diff_height: 0,
        diff_width: 0,
        wrapped: None,
        hunks: vec![],
        jumped_to_hunk: None,
        commits_area: ratatui::layout::Rect::default(),
        pending_push: None,
        list_width: None,
        dragging_separator: false,
        stale_unfocused_at: HashMap::new(),
        main_area: ratatui::layout::Rect::default(),
        status: None,
        pending_amend: None,
        fingerprint: String::new(),
        last_poll: Instant::now(),
    };
    app.reload(None)?;

    let mut terminal = init_terminal();
    let result = app.run(&mut terminal);
    restore_terminal();
    result
}

impl App {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;

            if event::poll(Duration::from_millis(250))? {
                let key = match event::read()? {
                    Event::Key(key) => key,
                    Event::Mouse(_) if self.pending_push.is_some() => continue,
                    Event::Mouse(mouse) => {
                        match mouse.kind {
                            MouseEventKind::ScrollDown => self.diff_scroll = (self.diff_scroll + MOUSE_SCROLL_ROWS).min(self.max_scroll()),
                            MouseEventKind::ScrollUp => self.diff_scroll = self.diff_scroll.saturating_sub(MOUSE_SCROLL_ROWS),
                            MouseEventKind::Down(MouseButton::Left) if self.on_separator(mouse.column, mouse.row) => {
                                self.dragging_separator = true;
                            }
                            MouseEventKind::Down(MouseButton::Left) => self.click_commit(mouse.column, mouse.row),
                            MouseEventKind::Drag(MouseButton::Left) if self.dragging_separator => {
                                let main = self.main_area;
                                self.list_width = Some(clamp_list_width(mouse.column.saturating_sub(main.x) + 1, main.width));
                            }
                            MouseEventKind::Up(MouseButton::Left) => self.dragging_separator = false,
                            _ => {}
                        }
                        continue;
                    }
                    _ => continue,
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                self.status = None;
                if self.pending_push.is_some() {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Enter => self.push(terminal)?,
                        _ => self.pending_push = None,
                    }
                    continue;
                }

                let page = self.diff_height.saturating_sub(2).max(1);
                match key.code {
                    KeyCode::Char('Q') => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    KeyCode::Up => self.select(self.selected().saturating_sub(1)),
                    KeyCode::Down => self.select(self.selected() + 1),
                    KeyCode::PageUp => self.diff_scroll = self.diff_scroll.saturating_sub(page),
                    KeyCode::PageDown => self.diff_scroll = (self.diff_scroll + page).min(self.max_scroll()),
                    KeyCode::Home => self.diff_scroll = 0,
                    KeyCode::End => self.diff_scroll = self.max_scroll(),
                    KeyCode::Char('j') => self.jump_to_hunk(true),
                    KeyCode::Char('k') => self.jump_to_hunk(false),
                    KeyCode::Char('r') => self.reword(terminal)?,
                    KeyCode::Char('b') => self.rebase(terminal)?,
                    KeyCode::Char('p') => self.confirm_push("HEAD".into()),
                    KeyCode::Char('P') => {
                        if let Some(commit) = self.selected_real_commit() {
                            self.confirm_push(commit.hash);
                        }
                    }
                    KeyCode::Char('a') => self.amend(terminal)?,
                    KeyCode::Char('A') => self.blocking_amend(terminal)?,
                    _ => {}
                }
            }

            if self.last_poll.elapsed() >= POLL_INTERVAL {
                self.last_poll = Instant::now();
                if self.current_fingerprint() != self.fingerprint {
                    self.reload_after_external_change()?;
                }
            }
            self.prune_stale();
        }
    }

    fn current_fingerprint(&self) -> String {
        let mut out = self.repo.run(&["rev-parse", "--symbolic-full-name", "HEAD"]).unwrap_or_default();
        out += &self.repo.run(&["rev-parse", "HEAD"]).unwrap_or_default();
        if let Some(base) = &self.base {
            out += &self.repo.run(&["rev-parse", base]).unwrap_or_default();
        }
        out + &self.repo.worktree_fingerprint()
    }

    fn reload(&mut self, prefer: Option<&str>) -> Result<()> {
        self.reload_with(prefer, false)
    }

    // If the focused commit disappeared, e.g. because an agent amended or reset it, it's kept and marked stale rather than jumping elsewhere.
    fn reload_after_external_change(&mut self) -> Result<()> {
        self.reload_with(None, true)
    }

    fn reload_with(&mut self, prefer: Option<&str>, keep_focused_as_stale: bool) -> Result<()> {
        let previous = self.commits.get(self.selected()).cloned();
        let mut stale: Vec<(usize, Commit)> =
            self.commits.iter().enumerate().filter(|(_, c)| c.stale).map(|(i, c)| (i, c.clone())).collect();
        if keep_focused_as_stale && let Some(previous) = previous.as_ref().filter(|p| p.kind == Kind::Commit && !p.stale) {
            stale.push((self.selected(), Commit { stale: true, ..previous.clone() }));
        }
        let first_load = self.commits.is_empty();

        self.branch = self.repo.branch();
        self.base = self.repo.base_ref();
        (self.ahead, self.behind) = self.base.as_deref().map(|b| self.repo.ahead_behind(b)).unwrap_or((0, 0));
        self.commits = self.repo.uncommitted();
        self.commits.extend(self.repo.commits(self.base.as_deref(), PUSHED_COMMITS_SHOWN)?);
        stale.sort_by_key(|(index, _)| *index);
        for (index, commit) in stale {
            if !self.commits.iter().any(|c| c.hash == commit.hash) {
                self.commits.insert(index.min(self.commits.len()), commit);
            }
        }
        self.stale_unfocused_at.retain(|hash, _| self.commits.iter().any(|c| c.stale && c.hash == *hash));
        self.diffs.retain(|hash, _| !hash.starts_with("gg:"));
        if self.wrapped.as_ref().is_some_and(|(hash, _, _)| hash.starts_with("gg:")) {
            self.wrapped = None;
        }
        self.fingerprint = self.current_fingerprint();

        let find = |hash: &str| self.commits.iter().position(|c| c.hash == hash);

        // Follows the selected commit if it was rewritten elsewhere (amended, rebased).
        let same_commit = |a: &Commit, b: &Commit| a.summary == b.summary && a.date == b.date && a.authors == b.authors;
        let find_rewritten = |previous: &Commit| self.commits.iter().position(|c| same_commit(c, previous));

        let index = prefer
            .and_then(find)
            .or_else(|| previous.as_ref().and_then(|p| find(&p.hash).or_else(|| find_rewritten(p))))
            .or_else(|| self.commits.iter().rposition(|c| !c.pushed))
            .unwrap_or(if first_load { 0 } else { self.selected() });
        let changed = !previous.as_ref().zip(self.commits.get(index)).is_some_and(|(p, c)| same_commit(p, c));
        self.list.select(Some(index.min(self.commits.len().saturating_sub(1))));
        if changed {
            self.diff_scroll = 0;
            self.jumped_to_hunk = None;
        }
        Ok(())
    }

    // Drops stale commits that haven't been focused for a while.
    fn prune_stale(&mut self) {
        let selected = self.commits.get(self.selected()).map(|c| c.hash.clone());
        let now = Instant::now();
        for commit in self.commits.iter().filter(|c| c.stale) {
            if Some(&commit.hash) == selected.as_ref() {
                self.stale_unfocused_at.remove(&commit.hash);
            } else {
                self.stale_unfocused_at.entry(commit.hash.clone()).or_insert(now);
            }
        }

        let expired = |c: &Commit| c.stale && self.stale_unfocused_at.get(&c.hash).is_some_and(|t| t.elapsed() >= STALE_GRACE);
        if !self.commits.iter().any(expired) {
            return;
        }
        let commits = std::mem::take(&mut self.commits);
        let (expired, kept): (Vec<Commit>, Vec<Commit>) = commits.into_iter().partition(|c| expired(c));
        self.commits = kept;
        for commit in expired {
            self.stale_unfocused_at.remove(&commit.hash);
        }
        if let Some(index) = selected.and_then(|hash| self.commits.iter().position(|c| c.hash == hash)) {
            self.list.select(Some(index));
        }
    }

    fn selected(&self) -> usize {
        self.list.selected().unwrap_or(0)
    }

    fn select(&mut self, index: usize) {
        let index = index.min(self.commits.len().saturating_sub(1));
        if index != self.selected() {
            self.list.select(Some(index));
            self.diff_scroll = 0;
            self.jumped_to_hunk = None;
        }
    }

    // The separator is the list's right border; the column right of it counts too, to be easier to grab.
    fn on_separator(&self, column: u16, row: u16) -> bool {
        let area = self.commits_area;
        let border = area.x + area.width.saturating_sub(1);
        (border..=border + 1).contains(&column) && (area.y..area.y + area.height).contains(&row)
    }

    fn click_commit(&mut self, column: u16, row: u16) {
        let area = self.commits_area;
        if !area.contains(ratatui::layout::Position { x: column, y: row }) {
            return;
        }

        // Each commit takes two rows.
        let index = self.list.offset() + (row - area.y) as usize / 2;
        if index < self.commits.len() {
            self.select(index);
        }
    }

    fn selected_commit(&self) -> Option<&Commit> {
        self.commits.get(self.selected())
    }

    fn diff(&mut self) -> Option<&Text<'static>> {
        let entry = self.selected_commit()?.clone();
        let hash = entry.hash.clone();
        if !self.diffs.contains_key(&hash) {
            let text = match self.repo.show_entry(&entry) {
                Ok(bytes) => {
                    let bytes = String::from_utf8_lossy(&bytes).replace('\t', "    ");
                    bytes.into_bytes().into_text().unwrap_or_else(|e| Text::raw(e.to_string()))
                }
                Err(e) => Text::raw(e.to_string()),
            };
            self.diffs.insert(hash.clone(), text);
        }

        let width = self.diff_width.max(1);
        if !self.wrapped.as_ref().is_some_and(|(h, w, _)| *h == hash && *w == width) {
            let (text, hunks) = wrap(&self.diffs[&hash], width);
            self.wrapped = Some((hash.clone(), width, text));
            self.hunks = hunks;
        }
        self.wrapped.as_ref().map(|(_, _, text)| text)
    }

    // The hunk last jumped to while it's still in view, or else the last hunk in view.
    fn current_hunk(&self) -> Option<usize> {
        let (top, bottom) = (self.diff_scroll, self.diff_scroll + self.diff_height);
        let in_view = |&(start, end): &(usize, usize)| start < bottom && end > top;
        if let Some(index) = self.jumped_to_hunk
            && self.hunks.get(index).is_some_and(in_view)
        {
            return Some(index);
        }
        self.hunks.iter().rposition(in_view)
    }

    fn jump_to_hunk(&mut self, forward: bool) {
        self.diff();
        let top = self.diff_scroll;
        let target = match self.current_hunk() {
            Some(index) if forward => Some(index + 1),
            Some(index) => index.checked_sub(1),
            None if forward => self.hunks.iter().position(|&(start, _)| start >= top),
            None => self.hunks.iter().rposition(|&(_, end)| end <= top),
        };
        if let Some(index) = target.filter(|&i| i < self.hunks.len()) {
            self.jumped_to_hunk = Some(index);
            self.diff_scroll = self.hunks[index].0.min(self.max_scroll());
        }
    }

    fn max_scroll(&mut self) -> usize {
        let height = self.diff_height;
        self.diff().map(|t| t.lines.len().saturating_sub(height)).unwrap_or(0)
    }

    fn set_status(&mut self, message: impl Into<String>, error: bool) {
        self.status = Some((message.into(), error));
    }

    fn suspend<T>(&mut self, terminal: &mut DefaultTerminal, f: impl FnOnce(&mut Self) -> T) -> Result<T> {
        restore_terminal();
        let result = f(self);
        *terminal = init_terminal();
        terminal.clear()?;
        Ok(result)
    }

    fn reword(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(commit) = self.selected_real_commit() else { return Ok(()) };
        if commit.pushed {
            self.set_status(format!("{} is already pushed – not rewording", commit.short), true);
            return Ok(());
        }

        let original = self.repo.message(&commit.hash)?;
        let dir = std::env::temp_dir().join(format!("gg-{}-{}", std::process::id(), commit.short));
        std::fs::create_dir_all(&dir)?;

        // This file name makes Vim use the gitcommit filetype.
        let path = dir.join("COMMIT_EDITMSG");
        let scissors = format!("{} ------------------------ >8 ------------------------", self.repo.comment_char());
        let stat = self.repo.run(&["show", "--format=", "--stat", &commit.hash])?;
        let patch = amend::patch(&self.repo, &commit.hash)?;
        let template = format!(
            "{}\n\n{scissors}\n{c} Do not modify or remove the line above.\n{c} Edits to the diff amend the commit, like `git add -e`. Everything else below is ignored.\n{stat}\n{patch}",
            original.trim_end(),
            c = self.repo.comment_char(),
        );
        std::fs::write(&path, template)?;

        let editor = self.repo.editor();
        let edited = self.suspend(terminal, |_| {
            std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$@\"")).arg(&editor).arg(&path).status()
        })?;

        let content = std::fs::read_to_string(&path)?;
        let (message, below) = content.split_once(&format!("\n{scissors}\n")).unwrap_or((&content, ""));
        let message = format!("{}\n", message.trim_end());
        let edited_patch = below.find("diff --git ").map(|i| &below[i..]).unwrap_or("");
        let message_changed = message.trim_end() != original.trim_end();
        let patch_changed = !amend::same_ignoring_trailing_whitespace(edited_patch, &patch);

        if !edited.is_ok_and(|s| s.success()) {
            self.set_status("Editor failed – reword aborted", true);
        } else if message.trim().is_empty() {
            self.set_status("Empty message – reword aborted", true);
        } else if !message_changed && !patch_changed {
            self.set_status("Nothing changed", false);
        } else {
            let result = patch_changed
                .then(|| amend::tree_from_patch(&self.repo, &commit.hash, edited_patch, &dir))
                .transpose()
                .and_then(|tree| {
                    let change = rewrite::Change { message: message_changed.then_some(message.as_str()), tree: tree.as_deref() };
                    rewrite::rewrite(&self.repo, &commit.hash, change, self.base.as_deref())
                });
            match result {
                Ok(new_hash) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    self.reload(Some(&new_hash))?;
                    let what = if patch_changed { "Amended" } else { "Reworded" };
                    self.set_status(format!("{what} {}", commit.short), false);
                }
                Err(e) => self.set_status(format!("Reword failed: {e:#}. Your edits are kept in {}", path.display()), true),
            }
            return Ok(());
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    // The selected entry unless it's uncommitted changes, which commit actions don't apply to.
    fn selected_real_commit(&mut self) -> Option<Commit> {
        let commit = self.selected_commit().cloned()?;
        if commit.kind != Kind::Commit {
            self.set_status(format!("{} changes aren't a commit", commit.short), true);
            return None;
        }
        if commit.stale {
            self.set_status(format!("{} is no longer on this branch", commit.short), true);
            return None;
        }
        Some(commit)
    }

    fn amendable_commit(&mut self) -> Option<Commit> {
        let commit = self.selected_real_commit()?;
        if commit.pushed {
            self.set_status(format!("{} is already pushed – not amending", commit.short), true);
            return None;
        }
        Some(commit)
    }

    fn amend(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(commit) = self.amendable_commit() else { return Ok(()) };

        // Retrying after a failure reuses the earlier edits.
        let scratch = match self.pending_amend.take() {
            Some(scratch) if scratch.commit == commit.hash => scratch,
            _ => amend::Scratch::prepare(&self.repo, &commit.hash)?,
        };
        if scratch.files.is_empty() {
            self.set_status(format!("{} changes no editable files", commit.short), true);
            scratch.remove();
            return Ok(());
        }

        let editor = self.repo.editor();
        let edited = self.suspend(terminal, |_| amend::edit_files(&editor, &scratch.files_dir(), &scratch.files, &scratch.root))?;
        if let Err(e) = edited {
            self.set_status(format!("{e} – amend aborted"), true);
            scratch.remove();
            return Ok(());
        }

        let result = scratch.build_tree(&self.repo).and_then(|tree| match tree {
            None => Ok(None),
            Some(tree) => {
                let change = rewrite::Change { tree: Some(&tree), ..Default::default() };
                rewrite::rewrite(&self.repo, &commit.hash, change, self.base.as_deref()).map(Some)
            }
        });
        match result {
            Ok(None) => {
                self.set_status("No changes", false);
                scratch.remove();
            }
            Ok(Some(new_hash)) => {
                scratch.remove();
                self.reload(Some(&new_hash))?;
                self.set_status(format!("Amended {}", commit.short), false);
            }
            Err(e) => {
                let dir = scratch.files_dir();
                self.set_status(format!("Amend failed: {e}. Press a to retry your edits (kept in {})", dir.display()), true);
                self.pending_amend = Some(scratch);
            }
        }
        Ok(())
    }

    // Stops an interactive rebase at the commit so it's edited in the real repo, where other agents can see a rebase is in progress.
    fn blocking_amend(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(commit) = self.amendable_commit() else { return Ok(()) };
        if self.repo.rebase_in_progress() {
            self.set_status("A rebase is already in progress", true);
            return Ok(());
        }
        let files = amend::changed_files(&self.repo, &commit.hash)?;
        let editor = self.repo.editor();
        let onto = if self.repo.rev_parse(&format!("{}^", commit.hash)).is_ok() { format!("{}^", commit.hash) } else { "--root".into() };

        let outcome = self.suspend(terminal, |app| -> Result<String> {
            let repo = &app.repo;
            let git = |args: &[&str]| repo.command().args(args).status().is_ok_and(|s| s.success());
            let mark_first_as_edit = r#"f() { awk '!done && /^pick / { sub(/^pick/, "edit"); done = 1 } { print }' "$1" > "$1.gg" && mv "$1.gg" "$1"; }; f"#;

            println!("Blocking amend of {}: rebasing in the real repo.\n", commit.short);
            let sequence_editor = format!("sequence.editor={mark_first_as_edit}");
            if !git(&["-c", &sequence_editor, "rebase", "-i", "--autostash", &onto]) {
                bail!("{}", pause("Could not start the rebase."));
            }
            if repo.rev_parse("HEAD")? != commit.hash {
                bail!("{}", pause("The rebase didn't stop at the expected commit; it's left in progress."));
            }

            if !files.is_empty() {
                let _ = amend::edit_files(&editor, &repo.dir, &files, &repo.git_path("")?);
            }

            loop {
                print!("\n[c] amend {} and continue rebase  [a] abort rebase  [l] leave rebase in progress (e.g. to edit more): ", commit.short);
                let _ = std::io::stdout().flush();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;

                match answer.trim() {
                    "c" => {
                        if repo.rev_parse("HEAD")? != commit.hash {
                            println!("HEAD moved during the amend (did someone commit?), so not amending. Sort it out with git, or abort.");
                            continue;
                        }
                        let mut add = vec!["add", "-A", "--"];
                        add.extend(files.iter().map(|f| f.path.as_str()));
                        let committed = git(&add)
                            && (repo.ok(&["diff", "--cached", "--quiet"]) || git(&["commit", "--amend", "--no-edit", "--quiet"]));
                        if !committed {
                            bail!("{}", pause("Amending failed; the rebase is left in progress."));
                        }
                        if !git(&["rebase", "--continue"]) {
                            bail!("{}", pause("The rebase stopped (conflicts?); resolve it with git."));
                        }
                        return Ok(format!("Amended {}", commit.short));
                    }
                    "a" => {
                        git(&["rebase", "--abort"]);
                        return Ok("Blocking amend aborted".into());
                    }
                    "l" => return Ok("Rebase left in progress".into()),
                    _ => {}
                }
            }
        })?;

        self.reload(None)?;
        match outcome {
            Ok(message) => self.set_status(message, false),
            Err(e) => self.set_status(e.to_string(), true),
        }
        Ok(())
    }

    // Asks to push `rev` to the default remote branch (e.g. origin/main).
    fn confirm_push(&mut self, rev: String) {
        if self.repo.rebase_in_progress() {
            self.set_status("A rebase is in progress – not pushing", true);
            return;
        }
        let Some(onto) = self.repo.default_remote_branch() else {
            self.set_status("No origin/main or origin/master to push to", true);
            return;
        };
        let commits = self.repo.run(&["log", "--format=%h %s", &format!("{onto}..{rev}")]).unwrap_or_default();
        if commits.is_empty() {
            self.set_status(format!("Nothing to push to {onto}"), true);
            return;
        }

        let (remote, branch) = onto.split_once('/').unwrap_or(("origin", &onto));
        self.pending_push = Some(PendingPush {
            rev,
            remote: remote.to_string(),
            worktree_branch: self.repo.is_linked_worktree().then(|| self.branch.clone().unwrap_or_else(|| "detached HEAD".into())),
            branch: branch.to_string(),
            commits: commits.lines().map(String::from).collect(),
        });
    }

    // Runs in the real terminal, since git may need to ask for credentials. Never forces.
    fn push(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(push) = self.pending_push.take() else { return Ok(()) };
        let rev = self.repo.rev_parse(&push.rev)?;
        let refspec = format!("{rev}:refs/heads/{}", push.branch);

        let pushed = self.suspend(terminal, |app| {
            println!("$ git push {} {refspec}", push.remote);
            let ok = app.repo.command().args(["push", &push.remote, &refspec]).status().is_ok_and(|s| s.success());
            if !ok {
                pause("Push failed (maybe rebase with b first?)");
            }
            ok
        })?;

        self.reload(None)?;
        if pushed {
            self.set_status(format!("Pushed {} to {}/{}", commits(push.commits.len()), push.remote, push.branch), false);
        } else {
            self.set_status("Push failed", true);
        }
        Ok(())
    }

    fn draw_push_modal(&self, frame: &mut Frame) {
        let Some(push) = &self.pending_push else { return };
        let area = frame.area();
        let shown = push.commits.len().min(area.height.saturating_sub(8) as usize);
        let note = push.worktree_branch.as_ref().map(|b| format!("Worktree: pushes {b} straight to {}/{}", push.remote, push.branch));
        let widest = push.commits.iter().chain(&note).map(|c| c.chars().count()).max().unwrap_or(0);
        let width = (widest as u16 + 4).clamp(44, area.width.saturating_sub(4).max(1));
        let height = (lines_needed(shown, push.commits.len()) as u16 + if note.is_some() { 2 } else { 0 }).min(area.height);
        let modal = ratatui::layout::Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };

        let mut lines: Vec<Line> = note.into_iter().flat_map(|n| [Line::from(n).bold().yellow(), Line::raw("")]).collect();
        lines.extend(push.commits.iter().take(shown).map(|c| {
            let (hash, summary) = c.split_once(' ').unwrap_or((c, ""));
            Line::from(vec![hash.to_string().yellow(), Span::raw(format!(" {summary}"))])
        }));
        if shown < push.commits.len() {
            lines.push(Line::from(format!("…and {} more", push.commits.len() - shown)).fg(DIM));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec!["y".bold().cyan(), Span::raw("/"), "Enter".bold().cyan(), Span::raw(" push   any other key cancels")]));

        let title = format!(" Push {} to {}/{}? ", commits(push.commits.len()), push.remote, push.branch);
        let block = Block::bordered().title(title.bold()).border_style(Style::new().fg(Color::Cyan)).padding(ratatui::widgets::Padding::horizontal(1));
        frame.render_widget(ratatui::widgets::Clear, modal);
        frame.render_widget(Paragraph::new(lines).block(block), modal);
    }

    fn rebase(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(onto) = self.repo.default_remote_branch() else {
            self.set_status("No origin/main or origin/master to rebase onto", true);
            return Ok(());
        };
        let remote = onto.split('/').next().unwrap_or("origin").to_string();

        let succeeded = self.suspend(terminal, |app| {
            let run = |args: &[&str]| {
                println!("\n$ git {}", args.join(" "));
                app.repo.command().args(args).status().is_ok_and(|s| s.success())
            };
            let ok = run(&["fetch", &remote]) && run(&["rebase", "--autostash", &onto]);
            if !ok {
                print!("\nPress Enter to return to gg…");
                let _ = std::io::stdout().flush();
                let _ = std::io::stdin().read_line(&mut String::new());
            }
            ok
        })?;

        self.reload(None)?;
        if succeeded {
            self.set_status(format!("Rebased onto {onto}"), false);
        } else {
            self.set_status("Fetch or rebase failed", true);
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, main, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
        self.main_area = main;
        let list_width = clamp_list_width(self.list_width.unwrap_or((main.width * 2 / 5).min(60)), main.width);
        let [left, right] = Layout::horizontal([Constraint::Length(list_width), Constraint::Min(0)]).areas(main);

        self.draw_header(frame, header);
        self.draw_commits(frame, left);
        self.draw_diff(frame, right);

        let keys = [
            ("↑↓", "commit"),
            ("PgUp/PgDn", "scroll"),
            ("Home/End", "top/end"),
            ("j/k", "hunk"),
            ("r", "reword"),
            ("a", "amend"),
            ("A", "blocking amend"),
            ("b", "rebase"),
            ("p", "push"),
            ("P", "push to selected"),
            ("Q", "quit"),
        ];
        let legend: Vec<Span> = keys
            .iter()
            .flat_map(|(key, what)| [Span::raw(" "), key.bold().cyan(), Span::raw(format!(" {what}"))])
            .collect();
        frame.render_widget(Line::from(legend).on_black(), footer);
        self.draw_push_modal(frame);
    }

    fn draw_header(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let mut spans = vec![
            Span::raw(" "),
            Span::raw(self.branch.as_deref().unwrap_or("(detached HEAD)")).bold().magenta(),
        ];
        if self.repo.rebase_in_progress() {
            spans.push(Span::raw(" REBASE IN PROGRESS ").bold().white().on_red());
        }
        if let Some(base) = &self.base {
            spans.push(Span::raw(format!(" ⇄ {base} ")).fg(DIM));
            spans.push(Span::raw(format!("↑{}", self.ahead)).green());
            spans.push(Span::raw(" "));
            let behind = Span::raw(format!("↓{}", self.behind));
            spans.push(if self.behind > 0 { behind.yellow() } else { behind.fg(DIM) });
        }
        if let Some((message, error)) = &self.status {
            spans.push(Span::raw("   "));
            let message = Span::raw(message.clone());
            spans.push(if *error { message.red().bold() } else { message.green() });
        }
        frame.render_widget(Line::from(spans), area);
    }

    fn draw_commits(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        self.commits_area = area;
        let base_hash = self.base.as_deref().and_then(|b| self.repo.rev_parse(b).ok());
        let items: Vec<ListItem> = self
            .commits
            .iter()
            .map(|c| {
                if c.kind != Kind::Commit {
                    let badge = if c.kind == Kind::Staged { Color::Green } else { Color::Red };
                    let first = Line::from(vec![
                        Span::styled(format!(" {} ", c.short.to_uppercase()), Style::new().fg(Color::Black).bg(badge).add_modifier(Modifier::BOLD)),
                        Span::raw(" "),
                        Span::styled(c.authors.join(", "), Style::new().fg(Color::Green)),
                    ]);
                    let second = Line::from(Span::styled(format!("  {}", c.summary), Style::new().fg(badge).add_modifier(Modifier::ITALIC)));
                    return ListItem::new(vec![first, second]);
                }

                let style = |color: Color| Style::new().fg(if c.pushed || c.stale { DIM } else { color });
                let mut first = vec![];
                if c.stale {
                    first.push(Span::styled(" STALE ", Style::new().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)));
                    first.push(Span::raw(" "));
                }
                first.extend([
                    Span::styled(c.short.clone(), style(Color::Yellow)),
                    Span::raw(" "),
                    Span::styled(c.date.clone(), style(Color::Blue)),
                    Span::raw(" "),
                    Span::styled(c.authors.join(", "), style(Color::Green)),
                ]);
                let mut second = vec![Span::raw("  ")];
                if base_hash.as_deref() == Some(c.hash.as_str())
                    && let Some(base) = &self.base
                {
                    second.push(Span::styled(format!("[{base}]"), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
                    second.push(Span::raw(" "));
                }
                let summary_style = if c.stale { style(Color::Reset).add_modifier(Modifier::CROSSED_OUT) } else { style(Color::Reset) };
                second.push(Span::styled(c.summary.clone(), summary_style));
                ListItem::new(vec![Line::from(first), Line::from(second)])
            })
            .collect();

        let list = List::new(items)
            .block(Block::new().borders(Borders::RIGHT).border_style(Style::new().fg(DIM)))
            .highlight_style(Style::new().bg(SELECTED_BG))
            .scroll_padding(2);
        frame.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_diff(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let inner = ratatui::layout::Rect { x: area.x + 1, width: area.width.saturating_sub(1), ..area };
        self.diff_height = inner.height as usize;
        self.diff_width = inner.width as usize;
        let scroll = self.diff_scroll.min(self.max_scroll());
        self.diff_scroll = scroll;
        let Some(text) = self.diff() else { return };
        let visible: Vec<Line> = text.lines.iter().skip(scroll).take(inner.height as usize).cloned().collect();
        frame.render_widget(Paragraph::new(visible), inner);

        if let Some((start, end)) = self.current_hunk().map(|i| self.hunks[i]) {
            let first = start.max(scroll) - scroll;
            let last = end.min(scroll + inner.height as usize) - scroll;
            let gutter = ratatui::layout::Rect { y: area.y + first as u16, height: (last - first) as u16, width: 1, ..area };
            let marker: Vec<Line> = (first..last).map(|_| Line::from("▎".cyan())).collect();
            frame.render_widget(Paragraph::new(marker), gutter);
        }
    }
}

fn pause(message: &str) -> String {
    print!("\n{message}\nPress Enter to return to gg…");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read_line(&mut String::new());
    message.to_string()
}

// Soft-wraps at exactly `width` columns, so scrolling can count screen rows. Also returns the row ranges of hunks.
fn wrap(text: &Text<'static>, width: usize) -> (Text<'static>, Vec<(usize, usize)>) {
    let mut rows = Vec::with_capacity(text.lines.len());
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut in_hunk = false;
    for line in &text.lines {
        let starts_with = |prefix: &str| line.spans.iter().flat_map(|s| s.content.chars()).take(prefix.len()).eq(prefix.chars());
        if in_hunk && (starts_with("@@ ") || starts_with("diff ")) {
            hunks.last_mut().unwrap().1 = rows.len();
            in_hunk = false;
        }
        if starts_with("@@ ") {
            hunks.push((rows.len(), rows.len()));
            in_hunk = true;
        }

        let mut row: Vec<Span<'static>> = Vec::new();
        let mut row_width = 0;
        for span in &line.spans {
            let mut chunk = String::new();
            for ch in span.content.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if row_width + ch_width > width && row_width > 0 {
                    row.push(Span::styled(std::mem::take(&mut chunk), span.style));
                    rows.push(Line::from(std::mem::take(&mut row)).style(line.style));
                    row_width = 0;
                }
                chunk.push(ch);
                row_width += ch_width;
            }
            if !chunk.is_empty() {
                row.push(Span::styled(chunk, span.style));
            }
        }
        rows.push(Line::from(row).style(line.style));
    }
    if in_hunk {
        hunks.last_mut().unwrap().1 = rows.len();
    }
    (Text::from(rows), hunks)
}

// Captures the mouse so the wheel scrolls the diff, rather than the terminal sending arrow keys.
fn init_terminal() -> DefaultTerminal {
    let terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    terminal
}

fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

// Borders, commits, an optional "…and N more", a blank line and the key hint.
fn lines_needed(shown: usize, total: usize) -> usize {
    2 + shown + usize::from(shown < total) + 2
}

fn commits(count: usize) -> String {
    if count == 1 { "1 commit".into() } else { format!("{count} commits") }
}

// Keeps at least MIN_PANE_WIDTH columns for each pane, as far as the terminal allows.
fn clamp_list_width(width: u16, total: u16) -> u16 {
    let max = total.saturating_sub(MIN_PANE_WIDTH).max(MIN_PANE_WIDTH.min(total));
    width.clamp(MIN_PANE_WIDTH.min(max), max)
}
