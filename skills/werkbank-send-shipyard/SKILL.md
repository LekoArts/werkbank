---
name: werkbank-send-shipyard
description: Hand work to Mastra Factory (the Shipyard bot in Slack) as drafts the user approves in the Werkbank Inbox. Use when the user asks to send, hand off or delegate findings, tasks or fixes to Factory or Shipyard, or to "draft it for Factory". Covers the `werkbank factory draft` CLI, the 3000-character limit, and how to write a prompt a remote agent can act on without this machine.
---

# Send work to Shipyard (Mastra Factory)

`werkbank factory draft` saves a message as a **draft**. Nothing is posted.
The user reads each draft in the Werkbank Inbox's Factory tab and sends it
with `y`; it then posts to Slack mentioning Shipyard, and each message starts
one Factory session that works on its own.

## The command

```sh
WERKBANK="$(herdr plugin list | sed -n 's/^- werkbank .*\[local:\(.*\)\]$/\1/p')/target/release/werkbank"
"$WERKBANK" factory draft <<'MSG'
…the message…
MSG
```

- Quote the heredoc delimiter (`<<'MSG'`) so the shell leaves `$`, backticks
  and quotes in the message alone.
- If the lookup prints nothing, ask the user where Werkbank is installed.
- `"$WERKBANK" factory draft --help` prints the rules below in short.
- Success prints `Draft saved (N/3000 characters)`. Any other output means
  nothing was saved: read the error, fix the message, run again.

## Hard limits

- **3000 characters**, counted as Slack does: the bot mention is added in
  front, `&` `<` `>` count as `&amp;` `&lt;` `&gt;`, an emoji counts as two.
  Over the limit the command refuses and says how much to cut. Cut words, not
  facts: drop pleasantries, restated context and long logs first.
- **One task per draft.** One draft is one Factory session and usually one
  PR. Unrelated tasks go in separate drafts; small related fixes that belong
  in one PR may share one.
- **Plain URLs only.** `<url|text>` links are escaped and show as text.
  Slack mrkdwn works: `*bold*`, `_italic_`, `` `code` ``, ``` blocks ```, `- ` lists.
- **No secrets, tokens, home paths or usernames.** The message lands in a
  shared Slack channel.

## Factory can't see this machine

The Factory agent starts from a fresh clone of the repo. It cannot see this
worktree, uncommitted changes, local files, terminal output or this
conversation. Every draft must stand on its own:

- Name the repo (`owner/name`) and the base branch.
- Point at code by repo-relative path, plus symbol or line, on a pushed
  commit or `main`. A GitHub permalink is best.
- Copy in the evidence: the numbers, the error line, the failing command.
  Trim logs to the lines that matter.
- If the task needs work that only exists locally, say so to the user and
  ask before pushing a branch. Never push on your own to make a draft work.

## Collect, then write

1. **Gather.** For each finding: what is wrong or wanted, where it lives,
   the evidence, and why it matters. Re-check every finding against current
   code; drop anything stale.
2. **Split.** Group findings into PR-sized tasks. Each task becomes one draft.
3. **Write** each draft in this shape, shortest form that still stands alone:

```
*<one-line goal, imperative>*
Repo: owner/name (base: main)

*Context*
<2–4 sentences: what you found and why it matters>

*Evidence*
- <path/to/file.ts:123 or permalink> — <what it shows>
- <numbers, e.g. "dist is 4.2 MB, 3.1 MB of it is X">

*Do*
- <concrete change 1>
- <concrete change 2>

*Done when*
- <checkable outcome, e.g. "pnpm build passes", "size drops below N">
- PR ready for review

*Out of scope*
- <what not to touch>
```

   Leave out a section that would be empty. Say what you are unsure of
   instead of guessing; Factory can investigate.
4. **Draft** each one with the command above.
5. **Report** to the user: how many drafts, the goal line of each, and that
   they wait in the Inbox's Factory tab (`4`, then `⏎` to read, `y` to send,
   `d` to discard).
