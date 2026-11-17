# Inbox demo

The video in the main README is a scripted tour of the Inbox, played on made-up data. Nothing real is read or sent: the Inbox runs with a throwaway `HOME` and plugin dirs, stand-ins for `gh`, `curl` and `herdr` that answer from invented JSON, and a fake daemon that keeps the state file fresh. No real agents, PRs, Linear issues, Slack or Herdr session appear in it.

- `inbox.py` holds the made-up data (agents, PRs, Linear issues), starts the Inbox in a pseudo-terminal and presses the keys listed in `TOUR`. The bottom two rows show each key and what it does.
- `record.sh` records `inbox.py` with asciinema and turns the recording into a GIF and an MP4.

## Watch it in your terminal

```sh
cargo build --release
python3 demo/inbox.py
```

The tour plays by itself, takes about a minute and quits at the end. Use a terminal at least 110 columns wide. To try a different build, pass it as the first argument: `python3 demo/inbox.py path/to/werkbank`.

## Record a new video

You need [asciinema](https://docs.asciinema.org/) 3, [agg](https://docs.asciinema.org/manual/agg/) and ffmpeg:

```sh
port install asciinema agg ffmpeg   # or: brew install asciinema agg ffmpeg
```

Then build and record:

```sh
cargo build --release
demo/record.sh                      # writes to target/demo/
```

This creates `inbox.cast` (the asciinema recording), `inbox.gif` and `inbox.mp4` in `target/demo/`, which git ignores. If `agg` or `ffmpeg` live somewhere other than your `PATH`, set `AGG=/path/to/agg` or `FFMPEG=/path/to/ffmpeg`. To write the files somewhere else, pass a folder: `demo/record.sh /tmp/demo`.

Watch the MP4 all the way through before you publish it. The tour waits a fixed time before each key, so on a slow machine a screen may still be loading when the next key arrives.

## Put it in the README

GitHub only plays videos it hosts itself:

1. Drag `target/demo/inbox.mp4` into any GitHub issue or PR comment box. You don't need to post the comment.
2. Copy the `https://github.com/user-attachments/assets/…` link it creates.
3. In the main `README.md`, replace the old link with the new one. Keep it on a line of its own so GitHub shows it as a video player.

## When the Inbox changes

Update `inbox.py`, then record again:

- **Keys or tabs changed:** edit `TOUR`. Each entry is `(seconds to wait, keys, caption)`. `"type:…"` types text one character at a time, and `None` just waits.
- **New things on screen:** add them to the made-up data (`AGENTS`, `PRS`, `ISSUES`, `DESCRIPTIONS`). Keep it invented: no real repos, people, issue ids or paths.
- **New outside calls:** if the Inbox calls a new `gh`, `curl` or `herdr` command, extend the fake script in `FAKE`. Otherwise that screen comes up empty or shows an error.
