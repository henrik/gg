//! Explanations of a change for reviewing it: a short document of prose with the change's hunks spliced in, like literate programming.
//!
//! The document is Markdown-ish text where a line like `[[hunk 3]]` stands for hunk 3 (counting the commit message as hunk 0).

use anyhow::Result;
use std::time::Duration;

pub enum Segment {
    Prose(String),
    Hunk(usize),
}

// What the explainer gets to know about each hunk.
pub struct HunkInfo {
    pub number: usize,
    pub location: String,
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
pub fn placeholder(hunks: &[HunkInfo]) -> Result<String> {
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
