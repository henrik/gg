//! Explanations of a change for reviewing it: a short document of prose with the change's hunks spliced in, like literate programming.
//!
//! The document is Markdown-ish text where a line like `[[hunk 3]]` stands for hunk 3 (counting the commit message as hunk 0).

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// Enough for big commits without making Claude slow or expensive.
const MAX_DIFF_CHARS: usize = 120_000;

pub enum Segment {
    Prose(String),
    Hunk(usize),
}

// What the explainer gets to know about each hunk.
pub struct HunkInfo {
    pub number: usize,
    pub location: String,
    pub text: String,
}

pub struct Request {
    // E.g. "commit 4fe92da…" or "the unstaged changes".
    pub what: String,
    pub hunks: Vec<HunkInfo>,
    pub only: Option<usize>,
}

// Asks Claude in the background, without a trace in any chat: if there's a Claude next to gg, a throwaway fork of its session, which may know why the change was made; otherwise a fresh one in the repo. Stops early if `cancel` is set.
pub fn explain(request: &Request, repo_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<String> {
    if std::env::var("GG_EXPLAIN").as_deref() == Ok("placeholder") {
        return placeholder(&request.hunks);
    }

    let neighbour = crate::claude::neighbour().ok().flatten();
    let session = neighbour.as_ref().and_then(|n| Some((n.cwd.clone(), crate::claude::session_id(n)?)));
    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--output-format", "text", "--no-session-persistence", "--allowedTools", "Read,Grep,Glob"]);
    match &session {
        Some((cwd, id)) => cmd.current_dir(cwd).args(["--resume", id, "--fork-session"]),
        None => cmd.current_dir(repo_dir),
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not run claude")?;
    child.stdin.take().unwrap().write_all(prompt(request).as_bytes())?;

    // Read in the background so a full pipe can't stall Claude.
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("stopped");
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        let mut stderr = String::new();
        let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
        let message = stderr.lines().chain(out.lines()).filter(|l| !l.trim().is_empty()).last().unwrap_or("no output").to_string();
        bail!("claude failed: {message}");
    }
    Ok(out)
}

fn prompt(request: &Request) -> String {
    let scope = match request.only {
        Some(n) => format!(
            "Explain just hunk {n}, in the context of the whole change, in one or two short paragraphs. Put `[[hunk {n}]]` on its own line where the hunk should appear, normally after the first paragraph. Then up to 3 `! Check:` lines, or none."
        ),
        None => "Explain the whole change as a short guided reading:
- Start with one or two sentences on what it does and why, without restating the commit message.
- Then short sections, each a `## Title` and one to three sentences, followed by the hunks it covers, each as `[[hunk N]]` on its own line. Order sections so each makes sense given the earlier ones (data and types, then logic, then callers, then tests), not by file. Group hunks only when they clearly belong together.
- Place each non-trivial hunk exactly once. Instead of placing trivial hunks (renames, imports, formatting), end with one line like `Also: 4, 7 – imports and renames`.
- End with up to 3 `! Check:` lines for things worth verifying (edge cases, changed behaviour, missing tests), or none.
- About 150 words of prose for a small change, more only if the change really needs it."
            .into(),
    };

    let mut diff = String::new();
    for hunk in &request.hunks {
        diff.push_str(&format!("\n=== hunk {}: {} ===\n{}\n", hunk.number, hunk.location, hunk.text.trim_end()));
        if diff.len() > MAX_DIFF_CHARS {
            diff.push_str("\n(The rest of the diff is left out for length. Read the files if you need to.)\n");
            break;
        }
    }

    format!(
        "I'm reviewing {what} in gg, a git TUI, and want help understanding it so I can review it efficiently. Hunk 0 is the commit message, if there is one.

{scope}

Rules:
- Reply with only the document: no preamble, no closing remarks.
- Never quote or rewrite code; the hunks are spliced in where you put the markers.
- Explain intent and anything non-obvious, not what the diff plainly shows. Use `code` for identifiers. Plain Markdown only: `##` headings, paragraphs, `-` lists.
- You may read files in the repo for context, but don't change anything.
{diff}",
        what = request.what,
    )
}

pub fn parse(doc: &str) -> Vec<Segment> {
    let mut segments = vec![];
    let mut prose = String::new();
    for line in doc.lines() {
        if let Some(number) = hunk_marker(line) {
            if !prose.trim().is_empty() {
                segments.push(Segment::Prose(prose.trim().to_string()));
            }
            prose.clear();
            segments.push(Segment::Hunk(number));
        } else {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    if !prose.trim().is_empty() {
        segments.push(Segment::Prose(prose.trim().to_string()));
    }
    segments
}

fn hunk_marker(line: &str) -> Option<usize> {
    line.trim().strip_prefix("[[hunk ")?.strip_suffix("]]")?.trim().parse().ok()
}

// Stands in for Claude while the UI is being built.
fn placeholder(hunks: &[HunkInfo]) -> Result<String> {
    std::thread::sleep(Duration::from_secs(3));
    let mut doc = String::from("# Placeholder explanation\n\nThis is where Claude will explain the change, in the order that makes it easiest to follow.\n");
    for hunk in hunks {
        doc.push_str(&format!("\nHunk {} (`{}`) does something worth a sentence or two, and here it is:\n\n[[hunk {}]]\n", hunk.number, hunk.location, hunk.number));
    }
    doc.push_str("\nThings to check: whether this placeholder will be replaced soon.\n");
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prose_and_hunk_markers() {
        let segments = parse("# Title\n\nSome prose.\n[[hunk 2]]\nMore prose.\n  [[hunk 0]]  \n");
        let described: Vec<String> = segments
            .iter()
            .map(|s| match s {
                Segment::Prose(text) => format!("prose: {text}"),
                Segment::Hunk(n) => format!("hunk {n}"),
            })
            .collect();
        assert_eq!(described, ["prose: # Title\n\nSome prose.", "hunk 2", "prose: More prose.", "hunk 0"]);
    }
}
