# gg

## A note from the human

A TUI for reviewing and modifying unpushed commits with minimal toe-stepping vis-à-vis coding agents.

This is high-vibes highly personal software. If it's helpful to anyone else, I'll grudgingly allow it.

## Docs by the agent

### Install

    cargo install --path .

Then run `gg` in any git repo. Re-run the install after pulling changes.

### Screen

- Header: current branch, what it's compared against (its upstream, else `origin/main` or `origin/master`; always the latter in a linked worktree), commits ahead/behind, how far behind `origin/main` you are if that's not what it's compared against, and a red banner while a rebase is in progress. gg fetches `origin/main` (and the upstream) in the background every 2 minutes, never prompting for credentials, and says so in the header if fetching fails.
- Left: `UNSTAGED` (including untracked files) and `STAGED` entries at the top when there are such changes, showing who a commit would be by, then your unpushed commits, plus the 10 pushed commits below them, which are greyed out. Each shows a short hash (6 characters, longer if needed to be unique), the date (just the time if within the last 24 hours), the initials of the author and co-authors (e.g. `HN + CC` for Henrik Nyh with Claude Code), and the summary. The upstream's position is labelled, e.g. `[origin/main]`. Newest is on top, and the oldest unpushed commit is selected at start.
- Right: full message, stat and coloured diff, soft-wrapped. Within changed lines, the parts that actually changed are highlighted, like Git's diff-highlight. A cyan bar in the gutter marks the current hunk for `j`/`k`: the one you last jumped to while it's in view, else the last hunk in view.
- The list refreshes by itself when HEAD, the upstream or uncommitted changes change (checked every second), keeping your selection, even if the selected commit was rewritten elsewhere.
- If the focused commit disappears because of something outside gg (e.g. an agent amends or resets it), it stays in the list marked `STALE`, so you can still read it. It's removed 2 seconds after you move off it. Stale commits can't be reworded, amended or pushed.

### Keys

| Key | Action |
| --- | --- |
| `↑`/`↓`, click | Select commit |
| `PgUp`/`PgDn`, mouse wheel | Scroll diff |
| `Home`/`End` | Jump to top/bottom of diff |
| `Ctrl+↑`/`Ctrl+↓` | Move the commit up/down past its neighbour |
| `j`/`k` | Next/previous hunk |
| `w` | Toggle hiding whitespace changes in diffs (like `git diff -w`) |
| `r` | Reword (and optionally edit the diff) |
| `a` | Amend the commit's files |
| `A` | Blocking amend, via an interactive rebase |
| `b` | Fetch and rebase onto `origin/main` (or `origin/master`) |
| `p` | Push all commits to that branch |
| `P` | Push up to and including the selected commit |
| `Q`, `Ctrl+C` | Quit |

Pushes fetch first, then ask for confirmation in a modal, and never force. If `origin/main` has moved on, the modal says so and confirming rebases onto it before pushing; if that rebase would conflict, nothing changes and nothing is pushed. In a linked worktree (from `git worktree add`), the worktree's branch is treated as a local workspace for `origin/main`: its own upstream is ignored, and the push dialogue says it pushes straight to `origin/main`. Drag the vertical separator to resize the panes (each keeps at least 30 columns). gg captures the mouse, so hold Shift (Option in iTerm2) to select text.

### Rewriting safely alongside agents

`r` and `a` are safe while agents keep committing: you edit the message, or the commit's changed files, in a scratch copy. Only afterwards does gg replay any newer commits onto the rewritten one in memory and move HEAD with a compare-and-swap. The working tree is only updated (under the index lock) when the replay changes HEAD's tree, keeping unrelated uncommitted changes. If someone else (e.g. Claude) amends, rewords or rebases the commit while you're editing it, gg finds the commit it became (same author and author date) and applies your edits to that instead, or tells you if it already has them. It fails if your edits conflict with theirs, if the commit has left the branch in some other way, if a later commit conflicts with the change, or if uncommitted changes are in the way. A failed `a` keeps your edits, and pressing `a` again retries them.

`a` opens the files the commit adds or modifies in Vim tabs, each scrolled to its first change.

In `r`, the commit's diff appears below the scissors line, as with `git commit -v`. Editing it amends the commit's code, like `git add -e`. The edited diff is applied to the commit's parent, and if it doesn't apply cleanly nothing changes.

`b` and the rebase before pushing are done the same way: your unpushed commits are replayed onto `origin/main` in memory and HEAD is moved with a compare-and-swap, so there's no stash and no rebase in progress for agents to trip over, and uncommitted changes are kept. Commits whose changes are already upstream are dropped. If that fails (a conflict, or uncommitted changes in the way), `b` falls back to a real `git rebase --autostash` in the terminal.

`A` instead stops an interactive rebase at the commit and opens its files in the real repo, so agents can see a rebase is in progress. When you quit Vim it offers to amend and continue, abort, or leave the rebase in progress.

`Ctrl+↑`/`Ctrl+↓` work the same way. They refuse if either commit is pushed or a merge, if a rebase is in progress, or if the two commits aren't independent: the swapped pair must produce exactly the same code as before, so the result at HEAD never changes.

Pushed commits can't be reworded, amended or moved.

### Conventions and assumptions

gg expects these, so tell your agents about the ones that involve them:

- **`[WIP]` commits:** a summary starting with `[WIP]` (any case) marks an optimistic commit that's ready for review but whose specs haven't run yet. gg shows it with a magenta `WIP` badge. If a push includes one, the modal warns about it, and pushing needs a second confirmation with `W` after `y`/`Enter`. Have the agent reword the commit to drop the prefix once its specs pass.
- **One mainline:** pushes and `b` target `origin`'s default branch (`origin/HEAD`, or else `origin/main` or `origin/master`), and never force.
- **Worktrees are workspaces:** a linked worktree's branch is treated as a local workspace for that mainline, not as a branch of its own.
- **Rewritten commits keep their author date:** that's how gg recognises a commit someone else amended or rebased while you were editing it. `git commit --amend` and rebases keep it; a `--reset-author` doesn't.
- **A Vim-like editor** gets the changed files in tabs, scrolled to the first change. Other editors just get the files.
