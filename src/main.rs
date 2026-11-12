mod amend;
mod claude;
mod git;
mod rewrite;

use ansi_to_tui::IntoText;
use anyhow::{Result, bail};
use git::{Commit, Kind, Repo};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::{
        event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
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
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const FETCH_INTERVAL: Duration = Duration::from_secs(120);
const DIM: Color = Color::DarkGray;
const SELECTED_BG: Color = Color::Indexed(237);
const REMOVED_BG: Color = Color::Indexed(52);
const ADDED_BG: Color = Color::Indexed(22);
const WIP: Color = Color::Magenta;

// Optimistic commits whose specs haven't run yet, e.g. "[WIP] Fix the thing".
fn is_wip(summary: &str) -> bool {
    summary.trim_start().get(..5).is_some_and(|p| p.eq_ignore_ascii_case("[wip]"))
}

struct PendingPush {
    rev: String,
    // E.g. "origin/main", which is `remote`/`branch`.
    onto: String,
    remote: String,
    // Set when pushing from a linked worktree, whose branch isn't what gets pushed to.
    worktree_branch: Option<String>,
    branch: String,
    commits: Vec<String>,
    // Commits on `onto` that we don't have yet, so we must rebase before pushing.
    behind: usize,
    fetch_failed: bool,
    wip: usize,
    // Pushing WIP commits takes a second, deliberate confirmation.
    confirming_wip: bool,
}

// A request to Claude being written, about the selected commit or hunk.
struct Request {
    // E.g. "Reviewing 4fe92d, src/main.rs:12-20".
    context: String,
    text: String,
}

struct App {
    repo: Repo,
    branch: Option<String>,
    base: Option<String>,
    ahead: usize,
    behind: usize,
    // Where pushes go and `b` rebases onto, e.g. "origin/main", and how far behind it HEAD is.
    target: Option<String>,
    behind_target: usize,
    commits: Vec<Commit>,
    list: ListState,
    diffs: HashMap<String, Text<'static>>,
    diff_scroll: usize,
    diff_height: usize,
    diff_width: usize,
    wrapped: Option<(String, usize, Text<'static>)>,
    hunks: Vec<Hunk>,
    // Only while it's in view; none is selected until j or k is pressed.
    selected_hunk: Option<usize>,
    diff_area: ratatui::layout::Rect,
    ignore_whitespace: bool,
    commits_area: ratatui::layout::Rect,
    // Rows each entry in the list takes, as last drawn.
    item_heights: Vec<usize>,
    pending_push: Option<PendingPush>,
    request: Option<Request>,
    // The outcome of sending a request to Claude, which happens in the background.
    request_sent: Option<std::sync::mpsc::Receiver<Result<String>>>,
    // Set once the user drags the separator.
    list_width: Option<u16>,
    dragging_separator: bool,
    // When each stale commit stopped being focused.
    stale_unfocused_at: HashMap<String, Instant>,
    main_area: ratatui::layout::Rect,
    status: Option<(String, bool)>,
    pending_amend: Option<amend::Scratch>,
    fingerprint: String,
    // Cached, since they're needed on every redraw.
    rebase_in_progress: bool,
    base_hash: Option<String>,
    fetch: Option<std::process::Child>,
    last_fetch: Option<Instant>,
    fetch_failed: bool,
}

fn main() -> Result<()> {
    let repo = Repo::discover()?;
    let mut app = App {
        repo,
        branch: None,
        base: None,
        ahead: 0,
        behind: 0,
        target: None,
        behind_target: 0,
        commits: vec![],
        list: ListState::default(),
        diffs: HashMap::new(),
        diff_scroll: 0,
        diff_height: 0,
        diff_width: 0,
        wrapped: None,
        hunks: vec![],
        selected_hunk: None,
        diff_area: ratatui::layout::Rect::default(),
        ignore_whitespace: false,
        commits_area: ratatui::layout::Rect::default(),
        item_heights: Vec::new(),
        pending_push: None,
        request: None,
        request_sent: None,
        list_width: None,
        dragging_separator: false,
        stale_unfocused_at: HashMap::new(),
        main_area: ratatui::layout::Rect::default(),
        status: None,
        pending_amend: None,
        fingerprint: String::new(),
        rebase_in_progress: false,
        base_hash: None,
        fetch: None,
        last_fetch: None,
        fetch_failed: false,
    };
    app.reload(None)?;

    let mut terminal = init_terminal();
    let result = app.run(&mut terminal);
    restore_terminal();
    if let Some(mut fetch) = app.fetch.take() {
        let _ = fetch.kill();
    }
    result
}

impl App {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let changes = watch_for_changes(self.repo.clone());
        loop {
            terminal.draw(|f| self.draw(f))?;

            // Handles everything that's queued before redrawing, so bursts of scroll or mouse-move events can't pile up.
            if event::poll(Duration::from_millis(250))? {
                loop {
                    if self.handle_event(event::read()?, terminal)? {
                        return Ok(());
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }

            if changes.try_iter().last().is_some_and(|fingerprint| fingerprint != self.fingerprint) {
                self.reload_after_external_change()?;
            }
            self.prune_stale();
            self.background_fetch();
            if let Some(result) = self.request_sent.as_ref().and_then(|r| r.try_recv().ok()) {
                self.request_sent = None;
                match result {
                    Ok(message) => self.set_status(message, false),
                    Err(e) => self.set_status(format!("{e:#}"), true),
                }
            }
        }
    }

    // Returns true to quit.
    fn handle_event(&mut self, event: Event, terminal: &mut DefaultTerminal) -> Result<bool> {
        let key = match event {
            Event::Paste(text) if self.request.is_some() => {
                self.request.as_mut().unwrap().text.push_str(&text.split_whitespace().collect::<Vec<_>>().join(" "));
                return Ok(false);
            }
            Event::Mouse(_) if self.request.is_some() => return Ok(false),
            Event::Key(key) => key,
            Event::Mouse(_) if self.pending_push.is_some() => return Ok(false),
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => self.diff_scroll = (self.diff_scroll + MOUSE_SCROLL_ROWS).min(self.max_scroll()),
                    MouseEventKind::ScrollUp => self.diff_scroll = self.diff_scroll.saturating_sub(MOUSE_SCROLL_ROWS),
                    MouseEventKind::Down(MouseButton::Left) if self.on_separator(mouse.column, mouse.row) => {
                        self.dragging_separator = true;
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.click_commit(mouse.column, mouse.row);
                        self.click_diff(mouse.column, mouse.row);
                    }
                    MouseEventKind::Drag(MouseButton::Left) if self.dragging_separator => {
                        let main = self.main_area;
                        self.list_width = Some(clamp_list_width(mouse.column.saturating_sub(main.x) + 1, main.width));
                    }
                    MouseEventKind::Up(MouseButton::Left) => self.dragging_separator = false,
                    _ => {}
                }
                return Ok(false);
            }
            _ => return Ok(false),
        };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }

        self.status = None;
        if let Some(request) = &mut self.request {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => self.request = None,
                KeyCode::Enter if request.text.trim().is_empty() => self.request = None,
                KeyCode::Enter => self.send_request(),
                KeyCode::Char('u') if ctrl => request.text.clear(),
                KeyCode::Char('w') if ctrl => {
                    let kept = request.text.trim_end().rfind(' ').map_or(0, |i| i + 1);
                    request.text.truncate(kept);
                }
                KeyCode::Char('c') if ctrl => self.request = None,
                KeyCode::Char(c) => request.text.push(c),
                KeyCode::Backspace => {
                    request.text.pop();
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.pending_push.is_some() {
            let confirming_wip = self.pending_push.as_ref().is_some_and(|p| p.confirming_wip);
            match key.code {
                KeyCode::Char('W') if confirming_wip => self.push(terminal)?,
                KeyCode::Char('y') | KeyCode::Enter if !confirming_wip => {
                    if let Some(push) = self.pending_push.as_mut().filter(|p| p.wip > 0) {
                        push.confirming_wip = true;
                    } else {
                        self.push(terminal)?;
                    }
                }
                _ => self.pending_push = None,
            }
            return Ok(false);
        }

        let page = self.diff_height.saturating_sub(2).max(1);
        match key.code {
            KeyCode::Char('Q') => return Ok(true),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => self.move_commit(true),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => self.move_commit(false),
            KeyCode::Up => self.select(self.selected().saturating_sub(1)),
            KeyCode::Down => self.select(self.selected() + 1),
            KeyCode::PageUp => self.diff_scroll = self.diff_scroll.saturating_sub(page),
            KeyCode::PageDown => self.diff_scroll = (self.diff_scroll + page).min(self.max_scroll()),
            KeyCode::Home => self.diff_scroll = 0,
            KeyCode::End => self.diff_scroll = self.max_scroll(),
            KeyCode::Char('j') => self.jump_to_hunk(true),
            KeyCode::Char('k') => self.jump_to_hunk(false),
            KeyCode::Char('w') => self.toggle_whitespace(),
            KeyCode::Char('c') => self.start_request(),
            KeyCode::Char('r') => self.reword(terminal)?,
            KeyCode::Char('b') => self.rebase(terminal)?,
            KeyCode::Char('p') => self.confirm_push(terminal, "HEAD".into())?,
            KeyCode::Char('P') => {
                if let Some(commit) = self.selected_real_commit() {
                    self.confirm_push(terminal, commit.hash)?;
                }
            }
            KeyCode::Char('a') => self.amend(terminal)?,
            KeyCode::Char('A') => self.blocking_amend(terminal)?,
            _ => {}
        }
        Ok(false)
    }

    // Keeps the remote branches fresh, so the header shows how far behind we are.
    fn background_fetch(&mut self) {
        if let Some(fetch) = &mut self.fetch {
            match fetch.try_wait() {
                Ok(None) => return,
                result => self.fetch_failed = !result.is_ok_and(|s| s.is_some_and(|s| s.success())),
            }
            self.fetch = None;
        }
        if self.last_fetch.is_none_or(|t| t.elapsed() >= FETCH_INTERVAL) {
            self.last_fetch = Some(Instant::now());
            self.fetch = self.repo.fetch_command().and_then(|mut cmd| cmd.spawn().ok());
        }
    }

    // Fetches right away (or waits for a background fetch that's already running). Returns whether it succeeded.
    fn fetch_now(&mut self, terminal: &mut DefaultTerminal) -> Result<bool> {
        let target = self.target.clone().unwrap_or_else(|| "origin".into());
        self.set_status(format!("Fetching {target}…"), false);
        terminal.draw(|f| self.draw(f))?;

        let status = match self.fetch.take() {
            Some(mut fetch) => fetch.wait().ok(),
            None => self.repo.fetch_command().and_then(|mut cmd| cmd.status().ok()),
        };
        self.last_fetch = Some(Instant::now());
        self.fetch_failed = !status.is_some_and(|s| s.success());
        self.status = None;
        Ok(!self.fetch_failed)
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
        self.target = self.repo.default_remote_branch();
        self.behind_target = self.target.as_deref().map(|t| self.repo.ahead_behind(t).1).unwrap_or(0);
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
        self.fingerprint = self.repo.fingerprint();
        self.rebase_in_progress = self.repo.rebase_in_progress();
        self.base_hash = self.base.as_deref().and_then(|b| self.repo.rev_parse(b).ok());

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
            self.selected_hunk = None;
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
            self.selected_hunk = None;
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

        let mut top = area.y as usize;
        for index in self.list.offset()..self.commits.len() {
            top += self.item_heights.get(index).copied().unwrap_or(2);
            if (row as usize) < top {
                self.select(index);
                return;
            }
        }
    }

    fn selected_commit(&self) -> Option<&Commit> {
        self.commits.get(self.selected())
    }

    fn diff(&mut self) -> Option<&Text<'static>> {
        let entry = self.selected_commit()?.clone();
        let hash = entry.hash.clone();
        if !self.diffs.contains_key(&hash) {
            let text = match self.repo.show_entry(&entry, self.ignore_whitespace) {
                Ok(bytes) => {
                    let bytes = String::from_utf8_lossy(&bytes).replace('\t', "    ");
                    let mut text = bytes.into_bytes().into_text().unwrap_or_else(|e| Text::raw(e.to_string()));
                    highlight_changes(&mut text);
                    text
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

    // The selected hunk, unless it has been scrolled out of view.
    fn current_hunk(&self) -> Option<&Hunk> {
        let (top, bottom) = (self.diff_scroll, self.diff_scroll + self.diff_height);
        self.selected_hunk.and_then(|i| self.hunks.get(i)).filter(|h| h.start < bottom && h.end > top)
    }

    // With no hunk selected, either key selects the first one on screen (or the next one below it).
    fn jump_to_hunk(&mut self, forward: bool) {
        self.diff();
        let top = self.diff_scroll;
        let target = match self.current_hunk().and(self.selected_hunk) {
            Some(index) if forward => Some(index + 1),
            Some(index) => index.checked_sub(1),
            None => self.hunks.iter().position(|h| h.end > top),
        };
        if let Some(index) = target.filter(|&i| i < self.hunks.len()) {
            self.selected_hunk = Some(index);
            self.diff_scroll = self.hunks[index].start.min(self.max_scroll());
        }
    }

    fn start_request(&mut self) {
        let Some(entry) = self.selected_commit().cloned() else { return };
        let what = match entry.kind {
            Kind::Commit => entry.short.clone(),
            Kind::Staged => "staged changes".into(),
            Kind::Unstaged => "unstaged changes".into(),
        };
        let context = match self.current_hunk() {
            Some(hunk) => format!("Reviewing {what}, {}", hunk.location()),
            None => format!("Reviewing {what}"),
        };
        self.request = Some(Request { context, text: String::new() });
    }

    fn send_request(&mut self) {
        let Some(request) = self.request.take() else { return };
        let prompt = format!("{}: {}", request.context, request.text.trim());
        let (cwd, wide) = (self.repo.dir.clone(), self.main_area.width >= self.main_area.height * 3);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(claude::send(&prompt, &cwd, wide)));
        self.request_sent = Some(receiver);
        self.set_status("Sending to Claude…", false);
    }

    fn draw_request(&self, frame: &mut Frame) {
        let Some(request) = &self.request else { return };
        let area = frame.area();
        let width = (area.width * 3 / 4).clamp(40.min(area.width), area.width);
        let inner_width = width.saturating_sub(4).max(1) as usize;
        let text = format!("{}▏", request.text);
        let rows = |s: &str| s.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() / inner_width + 1;
        let height = ((rows(&request.context) + rows(&text) + 4) as u16).min(area.height);
        let modal = ratatui::layout::Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };

        let lines = vec![
            Line::from(request.context.clone()).fg(DIM),
            Line::raw(text),
            Line::raw(""),
            Line::from(vec!["Enter".bold().cyan(), Span::raw(" send   "), "Esc".bold().cyan(), Span::raw(" cancel")]),
        ];
        let block = Block::bordered()
            .title(" Ask Claude ".bold())
            .border_style(Style::new().fg(Color::Cyan))
            .padding(ratatui::widgets::Padding::horizontal(1));
        frame.render_widget(ratatui::widgets::Clear, modal);
        frame.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }).block(block), modal);
    }

    fn click_diff(&mut self, column: u16, row: u16) {
        let area = self.diff_area;
        if !area.contains(ratatui::layout::Position { x: column, y: row }) {
            return;
        }
        let row = self.diff_scroll + (row - area.y) as usize;
        if let Some(index) = self.hunks.iter().position(|h| (h.start..h.end).contains(&row)) {
            self.selected_hunk = Some(index);
        }
    }

    fn toggle_whitespace(&mut self) {
        self.ignore_whitespace = !self.ignore_whitespace;
        self.diffs.clear();
        self.wrapped = None;
        self.selected_hunk = None;
        self.diff_scroll = 0;
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
                Ok(rewritten) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    self.reload(Some(&rewritten.hash))?;
                    let what = if patch_changed { "Amended" } else { "Reworded" };
                    self.set_status(rewritten_status(what, &commit.short, &rewritten), false);
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

    // Moves the selected commit up (later) or down (earlier) by swapping it with its neighbour.
    fn move_commit(&mut self, up: bool) {
        let Some(commit) = self.amendable_commit() else { return };
        if self.rebase_in_progress {
            self.set_status("A rebase is in progress – not moving commits", true);
            return;
        }
        let index = self.selected();
        let neighbour = if up { index.checked_sub(1) } else { Some(index + 1) };
        let Some(neighbour) = neighbour.and_then(|i| self.commits.get(i)).filter(|c| c.kind == Kind::Commit && !c.stale).cloned() else {
            self.set_status(format!("Nothing to move {} past", commit.short), true);
            return;
        };

        // Swapping with the commit above is swapping that commit with its parent.
        let (child, parent) = if up { (&neighbour, &commit) } else { (&commit, &neighbour) };
        if !self.repo.rev_parse(&format!("{}^", child.hash)).is_ok_and(|p| p == parent.hash) {
            self.set_status(format!("{} isn't the parent of {}", parent.short, child.short), true);
            return;
        }
        match rewrite::swap_with_parent(&self.repo, &child.hash, self.base.as_deref()) {
            Ok((new_child, new_parent)) => {
                let moved = if up { new_parent } else { new_child };
                let result = self.reload(Some(&moved));
                let direction = if up { "up" } else { "down" };
                self.set_status(format!("Moved {} {direction}", commit.short), false);
                if let Err(e) = result {
                    self.set_status(format!("{e:#}"), true);
                }
            }
            Err(e) => self.set_status(format!("{e:#}"), true),
        }
    }

    fn amend(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(commit) = self.amendable_commit() else { return Ok(()) };

        // Retrying after a failure reuses the earlier edits, also if the commit has been rewritten since.
        let scratch = match self.pending_amend.take() {
            Some(scratch) if scratch.commit == commit.hash || self.is_successor(&commit.hash, &scratch.commit) => scratch,
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
            Ok(Some(rewritten)) => {
                scratch.remove();
                self.reload(Some(&rewritten.hash))?;
                self.set_status(rewritten_status("Amended", &commit.short, &rewritten), false);
            }
            Err(e) => {
                let dir = scratch.files_dir();
                self.set_status(format!("Amend failed: {e}. Press a to retry your edits (kept in {})", dir.display()), true);
                self.pending_amend = Some(scratch);
            }
        }
        Ok(())
    }

    fn is_successor(&self, commit: &str, of: &str) -> bool {
        let Ok(head) = self.repo.rev_parse("HEAD") else { return false };
        !self.repo.is_ancestor(of, &head) && rewrite::successor(&self.repo, of, &head, self.base.as_deref()).is_ok_and(|s| s == commit)
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

    // Asks to push `rev` to the default remote branch (e.g. origin/main), after fetching it to see if we need to rebase first.
    fn confirm_push(&mut self, terminal: &mut DefaultTerminal, rev: String) -> Result<()> {
        if self.repo.rebase_in_progress() {
            self.set_status("A rebase is in progress – not pushing", true);
            return Ok(());
        }
        let Some(onto) = self.repo.default_remote_branch() else {
            self.set_status("No origin/main or origin/master to push to", true);
            return Ok(());
        };
        let fetched = self.fetch_now(terminal)?;
        let rev = self.repo.rev_parse(&rev)?;
        let commits = self.repo.run(&["log", "--format=%h %s", &format!("{onto}..{rev}")]).unwrap_or_default();
        if commits.is_empty() {
            self.set_status(format!("Nothing to push to {onto}"), true);
            return Ok(());
        }
        let behind = self.repo.run(&["rev-list", "--count", &format!("{rev}..{onto}")])?.trim().parse().unwrap_or(0);

        let commits: Vec<String> = commits.lines().map(String::from).collect();
        let wip = commits.iter().filter(|c| is_wip(c.split_once(' ').map_or("", |(_, s)| s))).count();
        let (remote, branch) = onto.split_once('/').unwrap_or(("origin", &onto));
        self.pending_push = Some(PendingPush {
            wip,
            confirming_wip: false,
            rev,
            onto: onto.clone(),
            behind,
            fetch_failed: !fetched,
            remote: remote.to_string(),
            worktree_branch: self.repo.is_linked_worktree().then(|| self.branch.clone().unwrap_or_else(|| "detached HEAD".into())),
            branch: branch.to_string(),
            commits,
        });
        Ok(())
    }

    // Runs in the real terminal, since git may need to ask for credentials. Never forces.
    fn push(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(push) = self.pending_push.take() else { return Ok(()) };
        let mut rev = push.rev.clone();
        if push.behind > 0 {
            if self.repo.rebase_in_progress() {
                self.set_status("A rebase is in progress – not pushing", true);
                return Ok(());
            }
            match rewrite::rebase(&self.repo, &push.onto) {
                Ok(mapping) => rev = mapping.get(&rev).cloned().unwrap_or(rev),
                Err(e) => {
                    self.reload(None)?;
                    self.set_status(format!("Couldn't rebase onto {}, so didn't push: {e:#}", push.onto), true);
                    return Ok(());
                }
            }
        }
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
        let mut notes: Vec<(String, Color)> = vec![];
        if let Some(branch) = &push.worktree_branch {
            notes.push((format!("Worktree: pushes {branch} straight to {}", push.onto), Color::Yellow));
        }
        if push.fetch_failed {
            notes.push((format!("Couldn't fetch {}, so it may be out of date", push.onto), Color::Red));
        }
        if push.behind > 0 {
            notes.push((format!("{} has {} we don't, so this rebases onto it first", push.onto, commits(push.behind)), Color::Yellow));
        }
        if push.wip > 0 {
            let these = if push.wip == 1 { "1 is a WIP commit, whose specs may not have run".to_string() } else { format!("{} are WIP commits, whose specs may not have run", push.wip) };
            notes.push((format!("Careful: {these}"), WIP));
        }
        let widest = push.commits.iter().chain(notes.iter().map(|(n, _)| n)).map(|c| c.chars().count()).max().unwrap_or(0);
        let width = (widest as u16 + 4).clamp(44, area.width.saturating_sub(4).max(1));
        let notes_height = if notes.is_empty() { 0 } else { notes.len() + 1 };
        let height = ((lines_needed(shown, push.commits.len()) + notes_height) as u16).min(area.height);
        let modal = ratatui::layout::Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };

        let mut lines: Vec<Line> = notes.into_iter().map(|(n, color)| Line::from(n).bold().fg(color)).collect();
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.extend(push.commits.iter().take(shown).map(|c| {
            let (hash, summary) = c.split_once(' ').unwrap_or((c, ""));
            let mut spans = vec![hash.to_string().yellow(), Span::raw(" ")];
            spans.extend(summary_spans(summary, Style::new()));
            Line::from(spans)
        }));
        if shown < push.commits.len() {
            lines.push(Line::from(format!("…and {} more", push.commits.len() - shown)).fg(DIM));
        }
        lines.push(Line::raw(""));
        let action = if push.behind > 0 { "rebase and push" } else { "push" };
        if push.confirming_wip {
            lines.push(Line::from(vec!["W".bold().fg(WIP), Span::raw(format!(" really {action}, WIP and all   any other key cancels"))]));
        } else {
            lines.push(Line::from(vec!["y".bold().cyan(), Span::raw("/"), "Enter".bold().cyan(), Span::raw(format!(" {action}   any other key cancels"))]));
        }

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
        if self.repo.rebase_in_progress() {
            self.set_status("A rebase is already in progress", true);
            return Ok(());
        }
        if !self.fetch_now(terminal)? {
            self.set_status(format!("Couldn't fetch {onto}"), true);
            return Ok(());
        }
        let error = match rewrite::rebase(&self.repo, &onto) {
            Ok(_) => {
                self.reload(None)?;
                self.set_status(format!("Rebased onto {onto}"), false);
                return Ok(());
            }
            Err(e) => format!("{e:#}"),
        };

        let succeeded = self.suspend(terminal, |app| {
            println!("Couldn't rebase in memory: {error}\nFalling back to a real rebase.\n\n$ git rebase --autostash {onto}");
            let ok = app.repo.command().args(["rebase", "--autostash", &onto]).status().is_ok_and(|s| s.success());
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
            self.set_status("Rebase failed", true);
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, main, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
        self.main_area = main;
        let list_width = clamp_list_width(self.list_width.unwrap_or(MIN_PANE_WIDTH), main.width);
        let [left, right] = Layout::horizontal([Constraint::Length(list_width), Constraint::Min(0)]).areas(main);

        self.draw_header(frame, header);
        self.draw_commits(frame, left);
        self.draw_diff(frame, right);

        let keys = [
            ("↑↓", "commit"),
            ("Ctrl+↑↓", "move"),
            ("PgUp/PgDn", "scroll"),
            ("Home/End", "top/end"),
            ("j/k", "hunk"),
            ("w", if self.ignore_whitespace { "show whitespace" } else { "hide whitespace" }),
            ("c", "ask Claude"),
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
        self.draw_request(frame);
    }

    fn draw_header(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let mut spans = vec![
            Span::raw(" "),
            Span::raw(self.branch.as_deref().unwrap_or("(detached HEAD)")).bold().magenta(),
        ];
        if self.rebase_in_progress {
            spans.push(Span::raw(" REBASE IN PROGRESS ").bold().white().on_red());
        }
        if self.ignore_whitespace {
            spans.push(Span::raw(" HIDING WHITESPACE CHANGES ").bold().black().on_yellow());
        }
        if let Some(base) = &self.base {
            spans.push(Span::raw(format!(" ⇄ {base} ")).fg(DIM));
            spans.push(Span::raw(format!("↑{}", self.ahead)).green());
            spans.push(Span::raw(" "));
            let behind = Span::raw(format!("↓{}", self.behind));
            spans.push(if self.behind > 0 { behind.yellow().bold() } else { behind.fg(DIM) });
        }
        if let Some(target) = &self.target
            && self.base.as_ref() != Some(target)
            && self.behind_target > 0
        {
            spans.push(Span::raw(format!("  ↓{} {target}", self.behind_target)).yellow().bold());
        }
        if self.fetch_failed {
            spans.push(Span::raw("  fetch failed").red());
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
        let summary_width = area.width.saturating_sub(1 + SUMMARY_INDENT.len() as u16) as usize;
        let base_hash = self.base_hash.clone();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let items: Vec<ListItem> = self
            .commits
            .iter()
            .map(|c| {
                if c.kind != Kind::Commit {
                    let badge = if c.kind == Kind::Staged { Color::Green } else { Color::Red };
                    let first = Line::from(vec![
                        Span::styled(format!(" {} ", c.short.to_uppercase()), Style::new().fg(Color::Black).bg(badge).add_modifier(Modifier::BOLD)),
                        Span::raw(" "),
                        Span::styled(abbreviate_authors(&c.authors), Style::new().fg(Color::Green)),
                    ]);
                    let summary = vec![Span::styled(c.summary.clone(), Style::new().fg(badge).add_modifier(Modifier::ITALIC))];
                    return ListItem::new([vec![first], wrap_summary(summary, summary_width)].concat());
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
                    Span::styled(compact_date(c, now), style(Color::Blue)),
                    Span::raw(" "),
                    Span::styled(abbreviate_authors(&c.authors), style(Color::Green)),
                ]);
                let mut second = vec![];
                if base_hash.as_deref() == Some(c.hash.as_str())
                    && let Some(base) = &self.base
                {
                    second.push(Span::styled(format!("[{base}]"), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
                    second.push(Span::raw(" "));
                }
                let summary_style = if c.stale { style(Color::Reset).add_modifier(Modifier::CROSSED_OUT) } else { style(Color::Reset) };
                if c.pushed || c.stale {
                    second.push(Span::styled(c.summary.clone(), summary_style));
                } else {
                    second.extend(summary_spans(&c.summary, summary_style));
                }
                ListItem::new([vec![Line::from(first)], wrap_summary(second, summary_width)].concat())
            })
            .collect();
        self.item_heights = items.iter().map(ListItem::height).collect();

        let list = List::new(items)
            .block(Block::new().borders(Borders::RIGHT).border_style(Style::new().fg(DIM)))
            .highlight_style(Style::new().bg(SELECTED_BG))
            .scroll_padding(2);
        frame.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_diff(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let inner = ratatui::layout::Rect { x: area.x + 1, width: area.width.saturating_sub(1), ..area };
        self.diff_area = area;
        self.diff_height = inner.height as usize;
        self.diff_width = inner.width as usize;
        let scroll = self.diff_scroll.min(self.max_scroll());
        self.diff_scroll = scroll;
        let Some(text) = self.diff() else { return };
        let visible: Vec<Line> = text.lines.iter().skip(scroll).take(inner.height as usize).cloned().collect();
        frame.render_widget(Paragraph::new(visible), inner);

        if let Some((start, end)) = self.current_hunk().map(|h| (h.start, h.end)) {
            let first = start.max(scroll) - scroll;
            let last = end.min(scroll + inner.height as usize) - scroll;
            let gutter = ratatui::layout::Rect { y: area.y + first as u16, height: (last - first) as u16, width: 1, ..area };
            let marker: Vec<Line> = (first..last).map(|_| Line::from("▎".cyan())).collect();
            frame.render_widget(Paragraph::new(marker), gutter);
        }
    }
}

// Sends the repo's fingerprint every so often, computed in the background since `git status` can be slow in big repos.
fn watch_for_changes(repo: Repo) -> std::sync::mpsc::Receiver<String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while sender.send(repo.fingerprint()).is_ok() {
            std::thread::sleep(WATCH_INTERVAL);
        }
    });
    receiver
}

fn pause(message: &str) -> String {
    print!("\n{message}\nPress Enter to return to gg…");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read_line(&mut String::new());
    message.to_string()
}

struct Hunk {
    // Rows of the wrapped diff, end exclusive.
    start: usize,
    end: usize,
    path: String,
    // Lines in the new version of the file, end inclusive.
    lines: (usize, usize),
}

impl Hunk {
    // E.g. "src/main.rs:12-20".
    fn location(&self) -> String {
        match self.lines {
            (first, last) if first == last => format!("{}:{first}", self.path),
            (first, last) => format!("{}:{first}-{last}", self.path),
        }
    }
}

// Soft-wraps at exactly `width` columns, so scrolling can count screen rows. Also returns the hunks. Expects paths without "a/" and "b/" prefixes.
fn wrap(text: &Text<'static>, width: usize) -> (Text<'static>, Vec<Hunk>) {
    let mut rows = Vec::with_capacity(text.lines.len());
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut in_hunk = false;
    let (mut old_path, mut path) = (String::new(), String::new());
    for line in &text.lines {
        let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if in_hunk && (plain.starts_with("@@ ") || plain.starts_with("diff ")) {
            hunks.last_mut().unwrap().end = rows.len();
            in_hunk = false;
        }
        if !in_hunk && let Some(p) = plain.strip_prefix("--- ") {
            old_path = p.trim_end().to_string();
        } else if !in_hunk && let Some(p) = plain.strip_prefix("+++ ") {
            let p = p.trim_end();
            path = if p == "/dev/null" { old_path.clone() } else { p.to_string() };
        }
        if plain.starts_with("@@ ") {
            hunks.push(Hunk { start: rows.len(), end: rows.len(), path: path.clone(), lines: new_lines(&plain) });
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
        hunks.last_mut().unwrap().end = rows.len();
    }
    (Text::from(rows), hunks)
}

// The new file's lines from a header like "@@ -3,2 +4,5 @@". A hunk that only removes lines gives the line they were after.
fn new_lines(header: &str) -> (usize, usize) {
    let range = header.split_whitespace().nth(2).unwrap_or("+1").trim_start_matches('+');
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    let (start, count): (usize, usize) = (start.parse().unwrap_or(1), count.parse().unwrap_or(1));
    (start, start + count.max(1) - 1)
}

// Like Git's diff-highlight: pairs up removed lines with the added lines directly after them, in order, and highlights what changed in each pair, i.e. what's between their common prefix and suffix. Pairs with less than half in common are left alone, as they're probably unrelated lines.
fn highlight_changes(text: &mut Text<'static>) {
    let plain: Vec<Vec<char>> = text.lines.iter().map(|l| l.spans.iter().flat_map(|s| s.content.chars()).collect()).collect();
    let starts_with = |i: usize, c: char| plain.get(i).is_some_and(|l| l.first() == Some(&c));
    let mut in_hunk = false;
    let mut i = 0;
    while i < plain.len() {
        // Merge commits' combined diffs have "@@@" hunks with two marker columns, which we leave alone.
        if plain[i].starts_with(&['@', '@', ' ']) {
            in_hunk = true;
        } else if !(starts_with(i, ' ') || starts_with(i, '-') || starts_with(i, '+') || starts_with(i, '\\')) {
            in_hunk = false;
        }
        if !in_hunk || !starts_with(i, '-') {
            i += 1;
            continue;
        }

        let added_start = (i..).find(|&j| !starts_with(j, '-')).unwrap();
        let added_end = (added_start..).find(|&j| !starts_with(j, '+')).unwrap();
        for (old, new) in (i..added_start).zip(added_start..added_end) {
            let (a, b) = (&plain[old][1..], &plain[new][1..]);
            let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
            let max_suffix = a.len().min(b.len()) - prefix;
            let suffix = a.iter().rev().zip(b.iter().rev()).take(max_suffix).take_while(|(x, y)| x == y).count();
            if (prefix + suffix) * 2 >= a.len().max(b.len()) && a != b {
                set_background(&mut text.lines[old], 1 + prefix, 1 + a.len() - suffix, REMOVED_BG);
                set_background(&mut text.lines[new], 1 + prefix, 1 + b.len() - suffix, ADDED_BG);
            }
        }
        i = added_end;
    }
}

// Sets the background of the characters from `start` (inclusive) to `end` (exclusive).
fn set_background(line: &mut Line<'static>, start: usize, end: usize, bg: Color) {
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    let mut pos = 0;
    for span in line.spans.drain(..) {
        let chars: Vec<char> = span.content.chars().collect();
        let len = chars.len();
        let (from, to) = (start.clamp(pos, pos + len) - pos, end.clamp(pos, pos + len) - pos);
        for (a, b, highlighted) in [(0, from, false), (from, to, true), (to, len, false)] {
            if a < b {
                let style = if highlighted { span.style.bg(bg) } else { span.style };
                spans.push(Span::styled(chars[a..b].iter().collect::<String>(), style));
            }
        }
        pos += len;
    }
    line.spans = spans;
}

// Captures the mouse so the wheel scrolls the diff, rather than the terminal sending arrow keys.
fn init_terminal() -> DefaultTerminal {
    let terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    terminal
}

fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
}

// Just the time for commits from the last 24 hours. The date is formatted like "2026-11-12 14:05".
fn compact_date(commit: &Commit, now: i64) -> String {
    let recent = (0..24 * 60 * 60).contains(&(now - commit.timestamp));
    match commit.date.split_once(' ') {
        Some((_, time)) if recent => time.to_string(),
        _ => commit.date.clone(),
    }
}

// E.g. "HN + CC" for Henrik Nyh with Claude (Code) as co-author, whose name includes a model like "Claude Opus 5.5".
fn abbreviate_authors(authors: &[String]) -> String {
    let abbreviate = |name: &String| {
        if name.split_whitespace().next() == Some("Claude") {
            return "CC".to_string();
        }
        let initials: String = name.split_whitespace().filter_map(|w| w.chars().next()).filter(|c| c.is_alphabetic()).flat_map(char::to_uppercase).collect();
        if initials.is_empty() { name.clone() } else { initials }
    };
    authors.iter().map(abbreviate).collect::<Vec<_>>().join(" + ")
}

const SUMMARY_INDENT: &str = "  ";

// Word-wraps the summary to at most two indented lines of `width` columns, ending in "…" if it doesn't fit.
fn wrap_summary(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let chars: Vec<(char, Style)> = spans.iter().flat_map(|s| s.content.chars().map(move |c| (c, s.style))).collect();
    let width = width.max(2);
    let mut lines = vec![];
    let mut rest = &chars[..];

    while !rest.is_empty() {
        let last_line = lines.len() == 1;
        let fitting = chars_fitting(rest, width);
        let mut line = if fitting == rest.len() {
            std::mem::take(&mut rest).to_vec()
        } else if last_line {
            let mut line = rest[..chars_fitting(rest, width - 1)].to_vec();
            while line.last().is_some_and(|(c, _)| c.is_whitespace()) {
                line.pop();
            }
            let style = line.last().map_or(Style::new(), |(_, s)| *s);
            line.push(('…', style));
            rest = &[];
            line
        } else {
            // Break at the last space that fits, or mid-word if there's none.
            let split = rest[..=fitting].iter().rposition(|(c, _)| *c == ' ').filter(|&i| i > 0).unwrap_or(fitting);
            let line = rest[..split].to_vec();
            rest = &rest[split..];
            while rest.first().is_some_and(|(c, _)| *c == ' ') {
                rest = &rest[1..];
            }
            line
        };
        while line.last().is_some_and(|(c, _)| *c == ' ') {
            line.pop();
        }

        let mut spans: Vec<Span> = vec![];
        for (c, style) in line {
            match spans.last_mut() {
                Some(span) if span.style == style => span.content.to_mut().push(c),
                _ => spans.push(Span::styled(c.to_string(), style)),
            }
        }
        spans.insert(0, Span::raw(SUMMARY_INDENT));
        lines.push(Line::from(spans));
    }
    lines
}

// How many of the characters fit in `width` columns.
fn chars_fitting(chars: &[(char, Style)], width: usize) -> usize {
    let mut used = 0;
    chars.iter().take_while(|(c, _)| {
        used += c.width().unwrap_or(0);
        used <= width
    }).count()
}

// Shows a "[WIP]" prefix as a badge, so those commits stand out.
fn summary_spans(summary: &str, style: Style) -> Vec<Span<'static>> {
    if !is_wip(summary) {
        return vec![Span::styled(summary.to_string(), style)];
    }
    let rest = summary.trim_start()[5..].trim_start();
    let badge = Span::styled(" WIP ", Style::new().fg(Color::Black).bg(WIP).add_modifier(Modifier::BOLD));
    vec![badge, Span::raw(" "), Span::styled(rest.to_string(), style.fg(WIP))]
}

fn rewritten_status(what: &str, short: &str, rewritten: &rewrite::Rewritten) -> String {
    match &rewritten.followed {
        None => format!("{what} {short}"),
        Some(f) if f.already_there => format!("{short} was rewritten as {} meanwhile, which already has your edits", &f.successor[..7]),
        Some(f) => format!("{short} was rewritten as {} meanwhile – {} that instead", &f.successor[..7], what.to_lowercase()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn highlighted(line: &Line) -> Vec<String> {
        line.spans.iter().filter(|s| s.style.bg.is_some()).map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn wraps_summary_to_two_lines_with_ellipsis() {
        let text = |lines: Vec<Line>| lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert_eq!(text(wrap_summary(vec![Span::raw("Short")], 10)), ["  Short"]);
        assert_eq!(text(wrap_summary(vec![Span::raw("Fix the thing well")], 10)), ["  Fix the", "  thing well"]);
        assert_eq!(text(wrap_summary(vec![Span::raw("Fix the thing very nicely indeed")], 10)), ["  Fix the", "  thing ver…"]);
        assert_eq!(text(wrap_summary(vec![Span::raw("Supercalifragilistics")], 10)), ["  Supercalif", "  ragilisti…"]);
    }

    #[test]
    fn abbreviates_authors() {
        let authors = ["Henrik Nyh".to_string(), "Claude Opus 5.5".to_string(), "Anna-Karin von Ekholm".to_string(), "bot".to_string()];
        assert_eq!(abbreviate_authors(&authors), "HN + CC + AVE + B");
    }

    #[test]
    fn recognises_wip_commits() {
        assert!(is_wip("[WIP] Fix the thing"));
        assert!(is_wip("[wip] Fix the thing"));
        assert!(!is_wip("Fix the [WIP] thing"));
        assert!(!is_wip("WIP"));
    }

    #[test]
    fn highlights_what_changed_in_paired_lines() {
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,4 +1,4 @@\n same\n-let x = foo(1);\n-gone\n+let x = bar(1);\n+something else entirely\n+added\n";
        let mut text = Text::from(diff);
        highlight_changes(&mut text);

        assert_eq!(highlighted(&text.lines[1]), Vec::<String>::new());
        assert_eq!(highlighted(&text.lines[5]), ["foo"]);
        assert_eq!(highlighted(&text.lines[7]), ["bar"]);
        assert_eq!(highlighted(&text.lines[6]), Vec::<String>::new());
        assert_eq!(highlighted(&text.lines[8]), Vec::<String>::new());
    }
}
