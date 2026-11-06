# herdr-worktrunk

A [herdr](https://herdr.dev) plugin that manages the git worktrees with [worktrunk](https://worktrunk.dev) (`wt`), and keeps one rule: **1 git worktree = 1 herdr workspace**.

worktrunk creates and removes each worktree, so its hooks run (`pre-start`, `post-start`, `pre-remove`, `post-remove`...) and each worktree has the path of `wt`. The plugin replaces the worktree keys of herdr, which use git and `~/.herdr/worktrees`.

## Actions

| Action | Result |
|--------|--------|
| `herdr-worktrunk.pick` | Opens the worktree picker (fzf) in a popup, with the worktrees and the branches of `wt list`. Type to filter. The worktree opens in its herdr workspace. |
| `herdr-worktrunk.remove` | Deletes the worktree of the focused workspace with `wt remove`, after a question in a small popup. wt keeps a branch that is not merged. Then the focus goes to the workspace of the main checkout. |
| `herdr-worktrunk.merge` | Merges the worktree of the focused workspace into the default branch with `wt merge` (squash, rebase, `pre-merge` hooks, fast-forward), then deletes it, after a question in a small popup. A large popup shows the output of wt. |

`herdr-worktrunk.remove` and `herdr-worktrunk.merge` refuse a workspace where an agent works: the removal deletes the folder of the agent. If an action cannot work (for example the picker outside a git repository), a small popup gives the reason, with an OK button.

### Keys of the picker

| Key | Result |
|-----|--------|
| `Enter` | Opens the selected worktree, or the worktree of the selected branch (wt creates it). If the typed name matches no branch, the first row is `+ Create worktree <name>`: `Enter` on this row creates the worktree. |
| `⌫` | With an empty filter: asks to delete the selected worktree. The picker closes, a small popup asks the question, then the picker comes back. wt keeps a branch that is not merged, and refuses a worktree with changes that are not committed. With text in the filter: erases a character. |
| `Tab`, `shift-Tab` | Change the tab of the preview: `diff` (all the changes since the branch left the default branch, `wt step diff`), `log` (the commits), `changes` (the changes that are not committed). |
| `shift-↑`, `shift-↓` | Scroll the preview. |
| `Esc` | Cancels. |

The question has 2 buttons: `[ Cancel ]`, the default button, and `[ Delete ]` in red (or `[ Merge ]`). `←` `→` and `Tab` change the button, `Enter` presses it, `y` confirms, `Esc` cancels. herdr shows one popup at a time: a detached process of the plugin opens the next popup when the previous popup is closed.

The worktrunk picker (`wt switch`) has fixed keys (`alt-c`, `alt-x`), and a terminal program never sees the `cmd` key. Thus this plugin uses fzf, with keys that do not need `alt` or `cmd`.

## The observer

The plugin starts an observer with herdr. Every 2 seconds, the observer reads the state of herdr, and does 2 things:

1. **The pane follows its agent.** If an agent works in another checkout of the same repository than its workspace, the observer moves the pane of the agent to the workspace of this checkout. herdr opens this workspace if necessary. For example, Claude Code enters a worktree (`EnterWorktree`, `/worktrunk:wt-switch-create`): its pane goes to the workspace of the worktree. When it leaves the worktree, its pane comes back.
2. **The workspace of a removed worktree closes.** If `wt remove` or `wt merge` removes a worktree, and no agent works in its workspace, the observer closes the workspace.

Limits:

- The observer watches only the agents. A shell that changes its folder does not move. A change to another repository does not move.
- A pane moves only if the observer sees the same difference in 2 snapshots in a row.
- A workspace where an agent works stays open after the removal of its worktree, for example after `wt merge` in the agent. Close it with the herdr key `close_workspace`.
- A pane that moves gets a new pane ID: address an agent with its herdr name. A running `herdr agent prompt --wait` on this agent stops with `agent_not_running`: run `herdr agent wait <name>`.

herdr runs `[[startup]]` when the server starts, but not when the plugin is installed. Thus the event `pane.agent_detected` also starts the observer. A lock keeps one observer for each herdr server. If an update replaces the binary, the observer runs the new binary. Its log is `observer.log`, in the state folder of the plugin (by default `~/.local/state/herdr/plugins/herdr-worktrunk/`).

## Configuration

`config.toml` in the config folder of the plugin (`herdr plugin config-dir herdr-worktrunk`):

```toml
# Start the observer (default: true).
observe = true

# What an agent can do without a question at the end of a task in a worktree: "ask" or "auto".
# The plugin does not read this key: the agents read it (for example the skill worktrees of
# owpac/prompts).
agent_mode = "ask"

[repositories."~/.dotfiles"]
agent_mode = "auto"
```

The plugin reads only the top-level key `observe`, and ignores the other keys.

## Requirements

- herdr 0.9.0 or later, on macOS or Linux.
- worktrunk (`wt`) and git on the `PATH` of the herdr server.
- [fzf](https://github.com/junegunn/fzf) for the picker. The plugin uses its events `change`, `reload` and `execute` (tested with fzf 0.74).

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
```

## Development

```sh
git clone https://github.com/owpac/herdr-worktrunk
cd herdr-worktrunk
mise install                                         # Rust and shellcheck of mise.toml
HERDR_WORKTRUNK_BUILD=source sh scripts/install.sh   # build bin/herdr-worktrunk with cargo
herdr plugin link "$PWD"
bin/herdr-worktrunk observe                          # start the observer once after the link
```

`herdr plugin link` does not run the build step: run `scripts/install.sh` again after each change. The observer runs the new binary by itself. The checks of the CI:

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
