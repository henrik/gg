# gg

## A note from the human

A TUI for reviewing and modifying unpushed commits with minimal toe-stepping vis-à-vis coding agents.

This is high-vibes highly personal software. If it's helpful to anyone else, I'll grudgingly allow it.

## Docs by the agent

### Install

    cargo install --path .

Then run `gg` in any git repo. Re-run the install after pulling changes.

### Screen

- Header: current branch, what it's compared against (its upstream, else `origin/main` or `origin/master`; always the latter in a linked worktree), commits ahead/behind, and a red banner while a rebase is in progress.
- Left: `UNSTAGED` (including untracked files) and `STAGED` entries at the top when there are such changes, showing who a commit would be by, then your unpushed commits, plus the 10 pushed commits below them, which are greyed out. Each shows short hash, date, author and co-authors, and summary. The upstream's position is labelled, e.g. `[origin/main]`. Newest is on top, and the oldest unpushed commit is selected at start.
- Right: full message, stat and coloured diff, soft-wrapped. A cyan bar in the gutter marks the current hunk for `j`/`k`: the one you last jumped to while it's in view, else the last hunk in view.
- The list refreshes by itself when HEAD, the upstream or uncommitted changes change (checked every second), keeping your selection, even if the selected commit was rewritten elsewhere.
- If the focused commit disappears because of something outside gg (e.g. an agent amends or resets it), it stays in the list marked `STALE`, so you can still read it. It's removed 2 seconds after you move off it. Stale commits can't be reworded, amended or pushed.

### Keys

| Key | Action |
| --- | --- |
| `↑`/`↓`, click | Select commit |
| `PgUp`/`PgDn`, mouse wheel | Scroll diff |
| `Home`/`End` | Jump to top/bottom of diff |
| `j`/`k` | Next/previous hunk |
| `r` | Reword (and optionally edit the diff) |
| `a` | Amend the commit's files |
| `A` | Blocking amend, via an interactive rebase |
| `b` | Fetch and rebase onto `origin/main` (or `origin/master`), autostashing |
| `p` | Push all commits to that branch |
| `P` | Push up to and including the selected commit |
| `Q`, `Ctrl+C` | Quit |

Pushes ask for confirmation in a modal and never force. In a linked worktree (from `git worktree add`), the worktree's branch is treated as a local workspace for `origin/main`: its own upstream is ignored, and the push dialogue says it pushes straight to `origin/main`. Drag the vertical separator to resize the panes (each keeps at least 30 columns). gg captures the mouse, so hold Shift (Option in iTerm2) to select text.

### Rewriting safely alongside agents

`r` and `a` are safe while agents keep committing: you edit the message, or the commit's changed files, in a scratch copy. Only afterwards does gg replay any newer commits onto the rewritten one in memory and move HEAD with a compare-and-swap. The working tree is only updated (under the index lock) when the replay changes HEAD's tree, keeping unrelated uncommitted changes. It fails if the commit has left the branch, if a later commit conflicts with the change, or if uncommitted changes are in the way. A failed `a` keeps your edits, and pressing `a` again retries them.

`a` opens the files the commit adds or modifies in Vim tabs, each scrolled to its first change.

In `r`, the commit's diff appears below the scissors line, as with `git commit -v`. Editing it amends the commit's code, like `git add -e`. The edited diff is applied to the commit's parent, and if it doesn't apply cleanly nothing changes.

`A` instead stops an interactive rebase at the commit and opens its files in the real repo, so agents can see a rebase is in progress. When you quit Vim it offers to amend and continue, abort, or leave the rebase in progress.

Pushed commits can't be reworded or amended.
