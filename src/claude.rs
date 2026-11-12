//! Sends a request to the Claude session next to gg, in the same Herdr tab, without moving focus away from gg.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

// Prompts a Claude already running in another pane of this tab, or else starts one in a new split. Returns a short acknowledgement.
pub fn send(prompt: &str, cwd: &Path, wide: bool) -> Result<String> {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        bail!("Not running inside Herdr, so there's no Claude to send to");
    }
    let var = |name: &str| std::env::var(name).with_context(|| format!("{name} isn't set"));
    let (workspace, tab, own_pane) = (var("HERDR_WORKSPACE_ID")?, var("HERDR_TAB_ID")?, var("HERDR_PANE_ID")?);

    let panes = herdr(&["pane", "list", "--workspace", &workspace])?;
    let claude = panes["result"]["panes"].as_array().into_iter().flatten().find(|p| {
        p["tab_id"] == tab.as_str() && p["pane_id"] != own_pane.as_str() && p["agent"] == "claude"
    });
    if let Some(pane) = claude.and_then(|p| p["pane_id"].as_str()) {
        herdr(&["agent", "prompt", pane, prompt])?;
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
