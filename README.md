# Werkbank

A [Herdr](https://herdr.dev) plugin for running many Mastra Code agents in
parallel without losing track of them. One Rust binary, macOS and Linux.

## What you get

- **Inbox**: a Herdr tab with four views.
  - **Agents**: every agent, grouped by whose move it is, with what it is doing now, its goal progress, its PR and its Linear issue. You can reply without leaving the list.
  - **PRs**: your open PRs. Start a local agent on one, hand it to Factory, rerun failed CI or open its worktree.
  - **Issues**: your Linear issues. Start one locally in a fresh worktree, or send it to Factory (one issue, or several as one PR).
  - **Factory**: send prepared or free-form prompts to Mastra Factory through Slack.
- **Sidebar tokens** on each agent pane: `$pr` (`#123 ✓` / `✗` / `…` / `⇣` merged / `⊘` closed), `$stalled` (`⏸ 6m`), `$asking` (`?`).
- **Tab-bar summary**: `?1 ⏸2 ✗1 ⇣1`, empty when nothing needs you.
- **Notifications** for stalled agents and failed CI.
- **Worktree setup**: every new Herdr worktree gets your git-ignored `.env` and local settings copied in, then install and build run in a background tab.
- **Safe cleanup** of merged-PR worktrees. It refuses on anything unpushed, dirty or still in use.
- **Launchers**: a new tab with Mastra Code, or a new worktree with Mastra Code.

A background daemon (one per Herdr session) does the watching, so tokens and
notifications work with the Inbox closed.

## Install

Needs Herdr ≥ 0.9.5 with its Mastra Code integration enabled (Herdr settings →
integrations), `cargo`, `gh` (authenticated), `curl` and `sqlite3`.

```sh
cargo build --release && herdr plugin link .
herdr server stop   # the daemon starts with the next server
```

Then add to `~/.config/herdr/config.toml` (the plugin never edits it):

```toml
[ui]
tab_bar_right = [
  { type = "command", command = "/path/to/werkbank/target/release/werkbank status /Users/you/.local/state/herdr/plugins/werkbank/state-default.json", interval_seconds = 5, timeout_seconds = 4 },
]

[ui.sidebar.agents]
rows = [["state_icon","workspace","tab"],["agent","$pr","$stalled","$asking"]]

[[keys.command]]
key = "prefix+i"
type = "plugin_action"
command = "werkbank.open-inbox"

[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "werkbank.launch-tab"

[[keys.command]]
key = "prefix+y"
type = "plugin_action"
command = "werkbank.launch-worktree"
```

Run `herdr server reload-config` and re-attach. The keys are suggestions that
avoid Herdr's defaults (`herdr --default-config` lists those).

To update, pull and run `cargo build --release`. The daemon switches to the new
binary by itself; quit and reopen an open Inbox.

## Configure

Secrets go in the plugin's `.env`. A variable already in the Herdr server's
environment wins.

```sh
dir="$(herdr plugin config-dir werkbank)" && mkdir -p "$dir"
$EDITOR "$dir/.env" && chmod 600 "$dir/.env"
```

| Variable | Needed for |
|---|---|
| `LINEAR_API_KEY` | Issues tab and Linear info on agent rows (a personal API key) |
| `SLACK_USER_TOKEN` | Factory: a user token (`xoxp-…`), see below |
| `SLACK_CHANNEL` | Factory: the channel id (`C…`), the only channel Werkbank posts to |
| `SLACK_FACTORY_MENTION` | Factory: the Factory bot's member id (`U…`) |

Secrets reach `curl` on stdin, never argv, and never appear in notices or logs.

Optional, set in the Herdr server's environment:

| Variable | Default | |
|---|---|---|
| `WERKBANK_STALL_SECS` | `300` | Seconds without output before a working agent counts as stalled |
| `WERKBANK_POLL_MS` | `2000` | Daemon tick when Herdr's event stream is down. While it is up, events wake the daemon at once and it polls every 10 s |
| `WERKBANK_MASTRA_DB` | Mastra Code's `mastra.db` | Where agent recaps are read from (read-only) |
| `WERKBANK_FACTORY_CONFIG` | `<config dir>/factory.json` | Factory prompt file |

### Factory (Slack)

Factory starts a session when someone whose Slack account is linked to Factory
@-mentions its bot. Werkbank posts that message as you.

1. Create a Slack app with exactly one **user** token scope, `chat:write`, and
   install it. Use the user token: a bot token is rejected. With only that scope,
   a leaked token can post as you but cannot read anything.
2. Invite the Factory bot to your channel and link your Slack account in Factory.
3. Set the three `SLACK_*` variables.

Prompts can be overridden in `factory.json`. Every field is optional, and the
built-ins fill any role you leave out:

```json
{
  "signature": "Sent via werkbank",
  "prompts": [
    { "name": "Linear issue → PR", "role": "issue", "text": "Take {ref} to a PR ready for review." },
    { "name": "Linear issues → one PR", "role": "issue-batch", "text": "One PR for:\n{refs}" },
    { "name": "Review PR", "role": "pr-review", "text": "Review {ref}" },
    { "name": "Fix CI", "role": "pr", "text": "Fix the failing CI on {ref}" },
    { "name": "GitHub issue", "role": "free", "text": "Work on {ref}" }
  ]
}
```

`role` sets where a prompt shows up:

| Role | Used by |
|---|---|
| `issue` | `f` on Issues, one issue |
| `issue-batch` | `f` on Issues, marked issues |
| `pr-review`, `pr` | the `s`/`f` picker on PRs |
| `free` | the Factory tab only |

The Factory tab lists every prompt except `issue-batch`. It asks for a URL
first unless the prompt is `free` and has no `{ref}`. `{ref}` is one URL and
`{refs}` is one URL per line. Without a placeholder, the URL goes on its own
last line. The signature appears as a small line under the
Slack message; set `""` to drop it.

### Worktree setup

No config needed. Install and build commands are detected from `package.json`
(using its package manager) or `Cargo.toml`. To override them per repo, add
`<config dir>/setup.json`:

```json
{ "repos": { "/path/to/main/checkout": { "commands": ["pnpm install", "pnpm run build:core"] } } }
```

An empty list skips setup. Copied files are git-ignored, untracked, under
1 MiB and never overwrite an existing file. Copied:

- env and secret files: `.env*`, `.envrc`, `.dev.vars*`, `.npmrc`, `.yarnrc.yml`, `*.tfvars`, Rails keys
- local settings: `.claude/settings.local.json`, `CLAUDE.local.md`, `.vscode/`, `.idea/`

For a worktree that already exists, run the `setup-worktree` action.

## Using the Inbox

`prefix+i` opens it. `1`–`4` or `[` / `]` switch views, `j` / `k` move, `/`
filters, `r` refreshes, `q` quits. Anything that leaves the machine or starts
an agent first shows the exact text: `y` sends it, `n` cancels.

### Agents

Rows are sorted by stage, in this order: `✗` needs fixing (stalled, or its PR
fails or conflicts), `?` needs you, `✓` done, `○` idle, `●` working, `◕` in
review and `⇣` merged. Within a stage, the agent that has waited longest
comes first.
A row shows what the agent is saying now, its active goal (`goal 2/30` plus the
judge's last verdict), tasks, issue and PR. When there is no live text, it
shows a recap from Mastra Code's memory instead.

| Key | |
|---|---|
| `Enter` | Go to the agent's pane |
| `Tab` | Reply (`Enter` sends, `Alt+Enter` adds a newline). Only idle, done or stalled agents take one |
| `←` / `→` | Filter by stage |
| `o` | Open its PR |
| `c` | Clean up a merged worktree: type `DELETE` to confirm, `b` also deletes the branch (`git branch -d`) |

### PRs

These are PRs you opened or are assigned to, grouped as needs fixing, ready to
merge, waiting and drafts. `! conflicts` and `! behind base` lead the row.
`Enter` opens details: the merge gate, failing checks, reviewers and the
description.

| Key | |
|---|---|
| `s` | Start a local agent on the PR's worktree with a picked prompt (Review PR, Fix CI, Address review, or your own). It opens a new tab if the worktree is already open |
| `f` | The same picker, sent to Factory |
| `R` | Rerun the failed jobs of the head commit's failed GitHub Actions runs |
| `w` | Go to the PR's worktree, or create one (not for forks) |
| `o` | Open in the browser |

### Issues

These are your open Linear issues, grouped by state. A row shows the priority,
labels, project and open worktree; on a narrow Inbox the labels give way first.

| Key | |
|---|---|
| `s` | Start it locally: worktree on the issue's branch, Mastra Code, the issue as the first prompt. Also moves the issue to started and assigns it to you |
| `f` | Send it to Factory. With issues marked, sends them all as one PR |
| `Space` | Mark for a batch |
| `c` | Change status |
| `n` | Start any issue by ID |
| `o` | Open in the browser |

### Factory

**Message** sends whatever you type. The other rows are prompts that ask for a
URL first.

## Limits

- Herdr's status needs Mastra Code 0.50.0 or newer. Older versions can leave
  a pane `idle` while it works; Werkbank then reads the busy footer
  (`• working … esc to interrupt`) from the screen and counts it as working.
- Even on 0.50.0, Herdr shows `working` while an agent waits on a question, and
  keeps `working` after a goal-mode turn stops to wait for you. Werkbank finds
  questions on the screen (`?`), but such a goal pane shows as working and then
  stalled.
- Ask detection and the busy check read Mastra Code's screen, so a Mastra Code
  UI change can break them.
- A long silent command (a 6-minute test run) shows as stalled.
- Agents started before the Herdr integration was installed report `idle`; restart them.
- Recaps lag the conversation: Mastra Code writes memory in batches.
- Factory posts are not retried. Factory picks the repo itself, so a batch that
  spans repos goes to the wrong one.
- Worktree setup runs every repo's install and build scripts, including on PR
  branches you check out, with no trust list. Those scripts run as you: they
  can read the secret files setup copied into the worktree and anything else
  you can, including Werkbank's `.env`. Werkbank only keeps `LINEAR_API_KEY`
  and `SLACK_USER_TOKEN` out of their environment. Set `"commands": []` for
  repos you don't trust.

## Development

Run `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release`.
The daemon logs to `~/.local/state/herdr/plugins/werkbank/daemon-<session>.log`,
and `werkbank status <state file>` prints what the tab bar shows.

| Path | |
|---|---|
| `src/core/` | Subprocess runner (timeouts, test fakes), Herdr CLI bridge |
| `src/integrations/` | `github`, `linear`, `slack`, `mastracode` clients |
| `src/features/daemon.rs` | The daemon: lock, state file, binary handover |
| `src/features/inbox/` | Observer (stall, ask, notifications) and the Inbox UI |
| `src/features/factory/` | Factory prompts and messages |
| `src/features/launch/`, `setup/`, `cleanup.rs` | Launchers, worktree setup, cleanup |
