# herdr-worktrunk

A [herdr](https://herdr.dev) plugin that manages the git worktrees with [worktrunk](https://worktrunk.dev) (`wt`), and keeps one rule: **1 git worktree = 1 herdr workspace**.

worktrunk creates and removes each worktree, so its hooks run (`pre-start`, `post-start`, `pre-remove`, `post-remove`...) and each worktree has the path of `wt`. The plugin replaces the worktree keys of herdr, which use git and `~/.herdr/worktrees`.

## Actions

| Action | Result |
|--------|--------|
| `herdr-worktrunk.pick` | Opens the worktree picker (fzf) in a popup, with the worktrees and the branches of `wt list`. Type to filter. The worktree opens in its herdr workspace. |
| `herdr-worktrunk.remove` | Deletes the worktree of the focused workspace with `wt remove`, after a question in a small popup. wt keeps a branch that is not merged. Then the focus goes to the workspace of the main checkout. |
| `herdr-worktrunk.merge` | Merges the worktree of the focused workspace into the default branch with `wt merge` (squash, rebase, `pre-merge` hooks, fast-forward), then deletes it, after a question in a small popup. A large popup shows the output of wt. |
| `herdr-worktrunk.clean` | Opens the list of the old worktrees without pull request, in all the repositories of the herdr workspaces (see [The popup cleaner](#the-popup-cleaner)). |

`herdr-worktrunk.remove` and `herdr-worktrunk.merge` refuse a workspace where an agent works: the removal deletes the folder of the agent. If an action cannot work (for example the picker outside a git repository), a small popup gives the reason, with an OK button.

### Keys of the picker

| Key | Result |
|-----|--------|
| `Enter` | Opens the selected worktree, or the worktree of the selected branch (wt creates it). If the typed name matches no branch, the first row is `+ <name> — new worktree from the default branch`: `Enter` on this row creates the worktree. |
| `⌫` | With an empty filter: asks to delete the selected worktree. The picker closes, a small popup asks the question, then the picker comes back. wt keeps a branch that is not merged, and refuses a worktree with changes that are not committed. With text in the filter: erases a character. |
| `Tab`, `shift-Tab` | Change the tab of the preview: `diff` (all the changes since the branch left the default branch, `wt step diff`), `log` (the commits), `changes` (the changes that are not committed). |
| `shift-↑`, `shift-↓` | Scroll the preview. |
| `Esc` | Cancels. |

Each row of the picker gives these items, from left to right:

- the state: `✓` no change, `●` changes that are not committed, `○` a branch without worktree, `+` a new worktree,
- the branch, and the subject of its last commit,
- the folder of the worktree, and `main` or `current`,
- the lines and the commits since the default branch (`+30 -4`, `↑2 ↓1`), and the marker of wt (`💬`),
- the age of the last commit.

If a row is too long, the folder goes away first.

The picker and the cleaner use the design of the plugins [herdr-bar](https://github.com/jeffarese/herdr-bar) and [herdr-newtab-plus](https://github.com/jeffarese/herdr-newtab-plus). The hints of the keys are at the bottom. The repository shows on the right of the prompt. The first line of the preview gives its tabs.

The question has 2 buttons: `[ Cancel ]`, the default button, and `[ Delete ]` in red (or `[ Merge ]`). `←` `→` and `Tab` change the button, `Enter` presses it, `y` confirms, `Esc` cancels. herdr shows one popup at a time: a detached process of the plugin opens the next popup when the previous popup is closed.

The worktrunk picker (`wt switch`) has fixed keys (`alt-c`, `alt-x`), and a terminal program never sees the `cmd` key. Thus this plugin uses fzf, with keys that do not need `alt` or `cmd`.

## The observer

The plugin starts an observer with herdr. Every 2 seconds, the observer reads the state of herdr, and does 2 things:

1. **The pane follows its agent.** If an agent works in another checkout of the same repository than its workspace, the observer moves the pane of the agent to the workspace of this checkout. herdr opens this workspace if necessary. For example, Claude Code enters a worktree (`EnterWorktree`, `/worktrunk:wt-switch-create`): its pane goes to the workspace of the worktree. When it leaves the worktree, its pane comes back. If the workspace is new (younger than 2 minutes) and has only one shell, the pane of the agent replaces this shell: for example the shell of a `post-start` hook that runs `herdr worktree open`. If the pane is the last pane of the workspace of a main checkout, the observer first opens a shell in this workspace: herdr closes a workspace without pane, and the workspaces of the worktrees then leave the group of the project.
2. **The workspace of a removed worktree closes.** If `wt remove` or `wt merge` removes a worktree, and no agent works in its workspace, the observer closes the workspace.

Limits:

- The observer watches only the agents. A shell that changes its folder does not move. A change to another repository does not move.
- A pane moves only if the observer sees the same difference in 2 snapshots in a row.
- A workspace where an agent works stays open after the removal of its worktree, for example after `wt merge` in the agent. Close it with the herdr key `close_workspace`.
- A pane that moves gets a new pane ID: address an agent with its herdr name. A running `herdr agent prompt --wait` on this agent stops with `agent_not_running`: run `herdr agent wait <name>`.

herdr runs `[[startup]]` when the server starts, but not when the plugin is installed. Thus the event `pane.agent_detected` also starts the observer. A lock keeps one observer for each herdr server. Its log is `observer.log`, in the state folder of the plugin (by default `~/.local/state/herdr/plugins/herdr-worktrunk/`).

The observer runs the binary of the last `observe` command. herdr installs each version of the plugin in a new folder, and keeps the old folders. Thus each `observe` command writes the path of its binary in `observer.binary`, in the state folder. Within 2 seconds, a running observer with another binary runs this binary, and writes `new binary: <old> -> <new>` in its log. If the new binary cannot run, the observer keeps its binary, and writes the error once in its log. After an update, the first agent that herdr detects gives the new binary to the observer. The observer also runs its own binary again if this binary changes, for example after `scripts/install.sh` in a linked folder.

The versions up to 2026.11.7 do not read `observer.binary`. After the update from one of these versions, stop the observer once: `pkill -f 'herdr-worktrunk observe --run'`.

The observer reads `config.toml` when it starts. After a change, stop the observer: `pkill -f 'herdr-worktrunk observe --run'`. The next agent that herdr detects starts it again.

### The worktrees of the finished pull requests

With `prune_merged = true` (see [Configuration](#configuration)), the observer also removes the worktree of each finished pull request: merged or closed. A second thread checks the repositories every 5 minutes. It checks a repository if a herdr workspace shows one of its checkouts.

The thread removes the worktree of a branch only if the branch has no open pull request, if the default branch does not contain the local commit, and if one of these conditions is true for a merged or closed pull request of the branch:

- The last commit of the pull request is the local commit of the branch.
- The pull request contains the local commit. This case covers the button "Update branch" of GitHub: it adds a commit that the local branch does not have.

Thus GitHub keeps each local commit: in the default branch after a merge, and in `refs/pull/<number>/head` after a close. After the removal of a closed pull request, the log gives the command that gets the branch back: `git fetch origin pull/<number>/head:<branch>`.

An agent in a finished worktree (its folder is in the worktree, or its workspace shows the worktree):

- If all these agents are `idle` or `done`, the thread closes their panes, then removes the worktree. The conversation stays on the disk: the log gives the command that resumes it, for example `claude --resume <session>`.
- If an agent is `working`, `blocked` or `unknown`, or if its workspace has the focus, the thread keeps the worktree, and tries again at the next pass.
- If the workspace of an agent shows the worktree, but the agent works in another folder, the thread does not close it, and keeps the worktree.
- The thread closes no agent if the worktree must stay: a new commit on the branch, or a change that is not committed.

The thread ignores a pull request from a fork, also with the same branch name.

The thread keeps:

- a worktree whose branch has an open pull request,
- a worktree whose local commit is in the default branch. A new worktree starts there, also on the name of an old branch. `wt step prune` removes this worktree, for example after a merge with a merge commit.
- the main worktree, a detached worktree and a locked worktree,
- a worktree with changes that are not committed: `wt remove` refuses it,
- a branch that got a new commit after the check. Just before the removal, the thread reads the commit of the branch again.

The thread removes the worktree and its branch with `wt remove --force-delete`. Without `--force-delete`, wt keeps a branch if it does not find its content in the default branch, for example after a close, or if the default branch later changed the same files. `--force-delete` acts only on the branch: the thread never uses `--force`, which removes the changes that are not committed. GitHub keeps the commits of the pull request (`refs/pull/<number>/head`). Then the observer closes the workspace of the worktree (rule 2).

The thread does not run `git fetch`. A fetch every 5 minutes can ask the ssh agent (for example 1Password) for a key while nobody is at the computer. gh uses the API of GitHub, with its own token.

To see the worktrees that the thread removes, run `list-merged` with the binary of the plugin. This command removes no worktree, and does not know the agents of herdr.

```sh
bin/herdr-worktrunk list-merged <folder of the repository>
```

Limits:

- The thread checks only a remote `origin` on the host `github.com`. An ssh alias of the host (`Host` in `~/.ssh/config`) does not work.
- The thread works only when herdr runs. It does not check a repository without a herdr workspace: the `post-start` hook of wt can run `wt step prune` for it.
- The thread sees only the agents. If a shell or an editor is open in the worktree, the thread removes the worktree, and the observer closes the workspace. Save your files before the merge.
- The thread stops a `gh` command after 1 minute, and a `git` command after 30 seconds.
- The thread never approves the hooks of a project (`--yes`). If a `pre-remove` or `post-remove` hook of the project has no approval, `wt remove` fails, and the log shows the error. Run `wt config approvals add` in the repository.
- The observer writes each removal and each new error in `observer.log`. An error that stays gives one line. When the thread starts, the log shows `prune_merged: on`.

### The old worktrees without pull request

With `prune_merged = true` and `stale_days = <days>`, the thread also asks about the old worktrees, once a day. An old worktree has:

- no pull request,
- no agent,
- no change that is not committed,
- no activity for `stale_days` days. The activity is the newest of the last commit and the last change of the reflog of `HEAD` of the worktree (its creation, a commit, a checkout, a reset). Thus a new worktree on an old commit is not old. A `git status` does not change the activity.

If an old worktree exists, and the thread did not ask about it in the last `stale_days` days, the thread opens the popup cleaner (see below). The popup shows all the worktrees: an old worktree that the user kept is also in the group "To delete". 2 files in the state folder keep the dates: `stale-last` (the last pass) and `stale-asked` (the last question for each worktree). To ask again at once, delete these 2 files: the next pass asks.

### The popup cleaner

The action `herdr-worktrunk.clean` opens the list of **all the linked worktrees of the repositories of the herdr workspaces**, in fzf. The list shows at once. Threads check each worktree in the background, and its row shows a spinner until its check ends.

The list has groups:

- **To delete**, at the top: the leftovers, the oldest first.
- One group for each project, with the other worktrees. Each row gives the reason that keeps the worktree.

A leftover has no agent, no change that is not committed, and one of these conditions:

- A merged or closed pull request contains the commit of its branch, with the same proofs as `prune_merged`. The age does not count.
- It has no pull request, and no activity for `stale_days` days (or 7 days if `stale_days` is 0). The cleaner ignores the last questions of the daily pass.

The other reasons keep a worktree: an open pull request, a recent activity, changes that are not committed, an agent, work after a finished pull request, a finished pull request with the commit in the default branch, or a check that failed (git, gh, or no remote on GitHub).

| Key | Result |
|-----|--------|
| `↑` `↓` | Move to the next worktree. The cursor skips the titles of the groups. |
| `Enter` | Asks to delete the selected worktree, then the list comes back. The question gives the reason of the row, for example an open pull request. |
| `ctrl-a` | Asks to delete all the worktrees of the group "To delete". A progress popup, then a summary, shows the result. |
| `Tab`, `shift-Tab` | Change the tab of the preview: `diff`, `log`, `changes` (as in the picker), and `checks`. `checks` gives the verdict and the checklist of the worktree: agent, changes, pull requests, activity. The title of a group shows the summary of its worktrees. |
| `Esc` | Quits. |

A filter hides the titles of the groups. Each row gives the state, the branch and its reason on the left, and the folder and the days without activity on the right.

Each deletion runs `wt remove`: wt keeps a branch that is not merged, and refuses a worktree with changes that are not committed. The cleaner refuses a worktree where an agent works, and closes the workspace of each deleted worktree.

From a shell, `clean` opens the same popup, for one repository or for all of them. `list-stale` prints the old worktrees of one repository without a popup.

```sh
bin/herdr-worktrunk clean [<folder of a repository>]
bin/herdr-worktrunk list-stale <folder of the repository>
```

How the checks work:

1. The popup lists the worktrees with git and herdr only, then shows the list.
2. 8 threads check the worktrees: `git status`, the last activity, then one call to gh for the pull requests of the branch. A worktree with an agent or with changes needs no call to gh.
3. After each result, the popup writes its state file, and sends a reload to fzf through the socket of fzf (`--listen`).
4. Before each reload, the popup keeps the id of the row of the cursor. After the reload, the cursor goes back to this row, also after a click.
5. After a question, the list comes back with the results of the last checks: these worktrees need no new check. A failed check runs again.

Limits:

- The daily pass waits 10 minutes at most for a free popup. Then it stops, and tries again the next day.

## Configuration

`config.toml` in the config folder of the plugin (`herdr plugin config-dir herdr-worktrunk`):

```toml
# Start the observer (default: true).
observe = true

# The observer removes the worktree of each merged or closed pull request (default: false). Needs gh.
prune_merged = false

# Once a day, ask about the worktrees without pull request and without change for this number
# of days (default: 0, off). Needs prune_merged = true.
stale_days = 0

# The size of the picker, the cleaner and the merger: "default" (the size of herdr-bar,
# 74 % × 62 % of the window) or "big" (90 % × 80 %).
popup_size = "default"

# What an agent can do without a question at the end of a task in a worktree: "ask" or "auto".
# The plugin does not read this key: the agents read it (for example the skill worktrees of
# owpac/prompts).
agent_mode = "ask"

[repositories."~/.dotfiles"]
agent_mode = "auto"
```

The plugin reads only the top-level keys `observe`, `prune_merged`, `stale_days` and `popup_size`, and ignores the other keys.

## Requirements

- herdr 0.9.0 or later, on macOS or Linux.
- worktrunk (`wt`) and git on the `PATH` of the herdr server.
- [fzf](https://github.com/junegunn/fzf) 0.73 or later, for the picker and the cleaner. The plugin uses its events `start`, `change`, `resize`, `load` and `every`, its server `--listen`, and its options `--footer`, `--ghost` and `--gutter` (tested with fzf 0.74).
- With `prune_merged = true` only: [gh](https://cli.github.com) on the `PATH` of the herdr server, with a login (`gh auth login`).

## Installation

Install a release: give its tag, from the [releases](https://github.com/owpac/herdr-worktrunk/releases) page.

```sh
herdr plugin install owpac/herdr-worktrunk --ref <tag>
```

The install downloads the release binary for your machine, and checks it with the `SHA256SUMS` file of the release. If the release has no binary for your machine, the install builds the binary with cargo (Rust 1.89 or later). Without `--ref`, herdr installs the last commit of `main`: its version is `0.0.0`, so the install always builds with cargo. To update, install the tag of the new release.

## Keybindings

Add `[[keys.command]]` entries to `~/.config/herdr/config.toml`, then run `herdr server reload-config`. To replace the worktree keys of herdr, unset them:

```toml
[keys]
new_worktree = ""
remove_worktree = ""

[[keys.command]]
key = "prefix+shift+g"
type = "plugin_action"
command = "herdr-worktrunk.pick"
description = "pick or create a worktree (wt)"

[[keys.command]]
key = "prefix+shift+x"
type = "plugin_action"
command = "herdr-worktrunk.remove"
description = "remove the worktree of the workspace (wt)"

[[keys.command]]
key = "prefix+shift+m"
type = "plugin_action"
command = "herdr-worktrunk.merge"
description = "merge the worktree of the workspace (wt)"

[[keys.command]]
key = "prefix+shift+c"
type = "plugin_action"
command = "herdr-worktrunk.clean"
description = "clean the old worktrees (wt)"
```

## Development

```sh
git clone https://github.com/owpac/herdr-worktrunk
cd herdr-worktrunk
mise install                                         # Rust and shellcheck of mise.toml
HERDR_WORKTRUNK_BUILD=source sh scripts/install.sh   # build bin/herdr-worktrunk with cargo
herdr plugin link "$PWD"
bin/herdr-worktrunk observe                          # give the binary of the link to the observer
```

`herdr plugin link` does not run the build step: run `scripts/install.sh` again after each change. A rebuild keeps the path of the binary: the observer sees its new modification time, and runs it by itself. A binary at another path needs an `observe` command: after `herdr plugin unlink` and an install, run `bin/herdr-worktrunk observe` of the installed folder, or wait for the next agent. The checks of the CI:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
shellcheck scripts/*.sh
```

## Release

Each push on `main` makes a release. The version is `YYYY.M.N`: the month of the push in UTC, and the number of the release in this month (for example `2026.11.1`). On `main`, `herdr-plugin.toml` and `Cargo.toml` keep the version `0.0.0`. The workflow `release` builds the binaries for Linux and macOS (`x86_64` and `aarch64`). Then it writes the version in a release commit on top of the commit of `main`, pushes the tag on this commit, and publishes the binaries with `SHA256SUMS`.

## Credits

- The behavior of the actions comes from [devashish2203/herdr-worktrunk](https://github.com/devashish2203/herdr-worktrunk) (MIT), a bash plugin for the same purpose. This plugin is a new implementation in Rust, with the observer.
- The build and release method comes from [herdr-bar](https://github.com/jeffarese/herdr-bar) (MIT).

## License

[MIT](LICENSE)
