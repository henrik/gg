//! Sends a request to the Claude session next to gg, in the same Herdr tab, without moving focus away from gg.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Neighbour {
    pub pane: String,
    pub cwd: PathBuf,
    // Claude Code sets it to the conversation's title, e.g. "Fix the login form".
    pub title: String,
}

// A Claude running in another pane of gg's Herdr tab.
pub fn neighbour() -> Result<Option<Neighbour>> {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        bail!("Not running inside Herdr, so there's no Claude to send to");
    }
    let var = |name: &str| std::env::var(name).with_context(|| format!("{name} isn't set"));
    let (workspace, tab, own_pane) = (var("HERDR_WORKSPACE_ID")?, var("HERDR_TAB_ID")?, var("HERDR_PANE_ID")?);

    let panes = herdr(&["pane", "list", "--workspace", &workspace])?;
    let claude = panes["result"]["panes"].as_array().into_iter().flatten().find(|p| {
        p["tab_id"] == tab.as_str() && p["pane_id"] != own_pane.as_str() && p["agent"] == "claude"
    });
    Ok(claude.and_then(|p| {
        Some(Neighbour {
            pane: p["pane_id"].as_str()?.to_string(),
            cwd: PathBuf::from(p["cwd"].as_str()?),
            title: p["terminal_title_stripped"].as_str().unwrap_or("").to_string(),
        })
    }))
}

// The ID of the neighbour's session. Herdr doesn't know it, so it's the most recent session for the directory whose title matches the pane's, or else the most recent one.
pub fn session_id(neighbour: &Neighbour) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let slug: String = neighbour.cwd.to_string_lossy().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let dir = Path::new(&home).join(".claude/projects").join(slug);
    let mut sessions: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().and_then(|m| m.modified()).ok()?, e.path())))
        .collect();
    sessions.sort_by(|a, b| b.0.cmp(&a.0));

    let needle = format!(r#""aiTitle":{}"#, serde_json::to_string(&neighbour.title).ok()?);
    let titled = (!neighbour.title.is_empty())
        .then(|| sessions.iter().take(20).find(|(_, path)| std::fs::read_to_string(path).is_ok_and(|s| s.contains(&needle))))
        .flatten();
    let (_, path) = titled.or(sessions.first())?;
    Some(path.file_stem()?.to_string_lossy().into_owned())
}

// Prompts a Claude already running in another pane of this tab, or else starts one in a new split. Returns a short acknowledgement.
pub fn send(prompt: &str, cwd: &Path, wide: bool) -> Result<String> {
    if let Some(neighbour) = neighbour()? {
        herdr(&["agent", "prompt", &neighbour.pane, prompt])?;
        return Ok("Sent to Claude".into());
    }

    let direction = if wide { "right" } else { "down" };
    let split = herdr(&["pane", "split", "--current", "--direction", direction, "--cwd", &cwd.to_string_lossy(), "--no-focus"])?;
    let pane = split["result"]["pane"]["pane_id"].as_str().context("Herdr didn't say which pane it opened")?;
    herdr(&["pane", "run", pane, &format!("claude {}", shell_quote(prompt))])?;
    Ok("Started Claude in a new pane with your request".into())
}

fn herdr(args: &[&str]) -> Result<Value> {
    let out = Command::new("herdr").args(args).output().context("Could not run herdr")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);

        // Errors are JSON like {"error":{"message":"…"}}.
        let message = serde_json::from_str::<Value>(&stderr)
            .ok()
            .and_then(|e| e["error"]["message"].as_str().map(String::from))
            .unwrap_or_else(|| stderr.trim().to_string());
        bail!("herdr {} failed: {message}", args.first().unwrap_or(&""));
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}
