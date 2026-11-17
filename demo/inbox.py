#!/usr/bin/env python3
"""Runs the Inbox on made-up data and plays a scripted tour of it.

Nothing real is read or sent: HOME, the plugin dirs and the session are
throwaway, a held lock stands in for the daemon, and `gh`, `curl` and `herdr`
are fakes that answer from the JSON written here. Record it with
demo/record.sh.

    python3 demo/inbox.py [path/to/werkbank]
"""

import fcntl
import json
import os
import pty
import select
import shutil
import signal
import struct
import sys
import tempfile
import termios
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIXTURES = os.path.join(ROOT, "tests", "fixtures")
REPO = "acme/widgets"
SESSION = "demo"
NOW = int(time.time() * 1000)
MIN = 60_000


def ago(ms):
    return NOW - ms


def iso(ms):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ms / 1000))


# ---------------------------------------------------------------- agents


def pr(number, ci, review=None, mergeable="MERGEABLE"):
    return {
        "number": number,
        "state": "OPEN",
        "url": f"https://github.com/{REPO}/pull/{number}",
        "head_oid": f"{number:040x}",
        "ci": ci,
        "is_draft": False,
        "review_decision": review,
        "mergeable": mergeable,
    }


def agent(n, title, status, since_min, branch=None, **extra):
    a = {
        "info": {
            "pane_id": f"w{n}:p1",
            "workspace_id": f"w{n}",
            "tab_id": f"w{n}:t1",
            "agent": "mastracode",
            "agent_status": status,
            "cwd": f"/home/you/code/widgets-{n}",
            "foreground_cwd": f"/home/you/code/widgets-{n}",
            "terminal_title_stripped": f"Mastra Code - {title}",
            "tokens": {},
            "state_change_seq": n,
            "focused": False,
        },
        "workspace_label": branch.split("/")[-1] if branch else "widgets",
        "branch": branch,
        "pr": None,
        "stalled": None,
        "asking": False,
        "thread": None,
        "recap": None,
        "question": None,
        "stall_hash": None,
        "state_since_unix_ms": ago(since_min * MIN),
    }
    a.update(extra)
    return a


def recap(**fields):
    base = {"lines": [], "partial": False, "tasks_done": 0, "tasks_total": 0}
    base.update(fields)
    return base


AGENTS = [
    agent(
        1, "Migrate billing webhooks to the v2 API", "working", 9,
        "you/eng-412-billing-webhooks",
        pr=pr(128, "Pending"), stalled={"secs": 540, "nanos": 0}, stall_hash=7,
        recap=recap(now="Waiting for the webhook replay to finish.", now_unix_ms=ago(9 * MIN),
                    tasks_done=3, tasks_total=5, active_task="Replaying last week's events"),
    ),
    agent(
        2, "Add dark mode to settings", "idle", 34, "you/eng-398-dark-mode",
        pr=pr(131, "Fail"),
        recap=recap(now="Opened the PR; visual snapshots need a look.", now_unix_ms=ago(34 * MIN)),
    ),
    agent(
        3, "Export widgets as CSV", "working", 2, "you/eng-405-csv-export",
        asking=True, question="Should the export include archived widgets?",
        recap=recap(now="The encoder is done; one question before I wire up the download.",
                    now_unix_ms=ago(2 * MIN), tasks_done=2, tasks_total=4,
                    active_task="Writing the CSV encoder"),
    ),
    agent(
        4, "Clean up the deploy scripts", "blocked", 5,
        recap=recap(now="I need approval to run `terraform plan` against staging.",
                    now_unix_ms=ago(5 * MIN)),
    ),
    agent(
        5, "Write the onboarding guide", "done", 12, "you/eng-377-onboarding-guide",
        recap=recap(lines=["✓ Guide covers install, first widget and sharing",
                           "✓ All examples checked against the API"],
                    now="The guide is ready for review.", now_unix_ms=ago(12 * MIN),
                    tasks_done=6, tasks_total=6),
    ),
    agent(
        6, "Speed up the search index rebuild", "working", 18, "you/eng-420-search-speed",
        recap=recap(now="Profiling shows the tokenizer allocates per word, so I'm pooling the buffers.",
                    now_unix_ms=ago(1 * MIN), goal="Rebuild the index in under 30s",
                    goal_runs=[3, 30], judge="Rebuild is down to 41s; keep going."),
    ),
    agent(
        7, "Fix the flaky checkout test", "working", 4, "you/eng-433-flaky-checkout",
        recap=recap(now="Running the checkout suite 50 times to confirm the fix.",
                    now_unix_ms=ago(1 * MIN), tasks_done=2, tasks_total=5,
                    active_task="Running the checkout suite"),
    ),
]

AGENT_ISSUES = {
    "ENG-412": ("Migrate billing webhooks to the v2 API", "In Progress"),
    "ENG-398": ("Dark mode for the settings page", "In Review"),
    "ENG-405": ("Export widgets as CSV", "In Progress"),
    "ENG-377": ("Onboarding guide for new teams", "In Progress"),
    "ENG-420": ("Search index rebuild takes 3 minutes", "In Progress"),
    "ENG-433": ("Checkout test fails one run in ten", "Todo"),
}

MERGED = [
    {
        "kind": "MergedCleanup",
        "workspace_id": "w8",
        "workspace_label": "eng-361-rate-limits",
        "title": "/home/you/code/widgets-8",
        "detail": "#119 ⇣  you/eng-361-rate-limits  c cleanup",
        "pr": dict(pr(119, "Pass", "APPROVED"), state="MERGED"),
    }
]


def state(ticked):
    return {
        "agents": AGENTS,
        "merged": MERGED,
        "issues": {
            k: {
                "identifier": k,
                "title": t,
                "description": None,
                "url": f"https://linear.app/acme/issue/{k}",
                "branchName": None,
                "state": {"name": s},
            }
            for k, (t, s) in AGENT_ISSUES.items()
        },
        "error": None,
        "linear_missing_key": False,
        "ticked_unix_ms": ticked,
        "daemon_pid": os.getpid(),
        "stall_secs": 300,
        "poll_ms": 2000,
    }


# ---------------------------------------------------------------- GitHub

PRS = [
    # (section, number, title, author, ci, review, mergeable, merge state, updated min ago)
    ("mine", 131, "feat(settings): dark mode", "you", "FAILURE", None, "MERGEABLE", "UNSTABLE", 34),
    ("mine", 128, "feat(billing): v2 webhooks", "you", "PENDING", None, "MERGEABLE", "UNKNOWN", 9),
    ("mine", 126, "fix(api): paginate widget search", "you", "SUCCESS", "APPROVED", "MERGEABLE", "CLEAN", 80),
    ("mine", 122, "docs: rate limit headers", "you", "SUCCESS", "REVIEW_REQUIRED", "CONFLICTING", "DIRTY", 300),
    ("assigned", 127, "feat(sharing): invite links that expire", "sam", "SUCCESS", "CHANGES_REQUESTED", "MERGEABLE", "BLOCKED", 55),
    ("assigned", 124, "chore(deps): bump the test toolchain", "ana", "SUCCESS", "REVIEW_REQUIRED", "MERGEABLE", "BEHIND", 190),
]


def pr_lists():
    template = json.load(open(os.path.join(FIXTURES, "gh", "pr-lists.json")))["data"]["mine"]["nodes"][0]
    data = {"viewer": {"login": "you"}, "mine": {"issueCount": 0, "nodes": []},
            "assigned": {"issueCount": 0, "nodes": []}}
    for section, number, title, author, ci, review, mergeable, merge_state, mins in PRS:
        node = json.loads(json.dumps(template))
        node.update(
            id=f"PR_{number}", number=number, title=title,
            url=f"https://github.com/{REPO}/pull/{number}",
            author={"__typename": "User", "login": author},
            repository={"nameWithOwner": REPO}, headRepository={"nameWithOwner": REPO},
            headRefName=f"{author}/pr-{number}", reviewDecision=review,
            mergeable=mergeable, mergeStateStatus=merge_state, updatedAt=iso(ago(mins * MIN)),
            head={"nodes": [{"commit": {"statusCheckRollup": {"state": ci}}}]},
            commits={"totalCount": 1, "nodes": [{"commit": {"oid": f"{number:040x}"}}]},
        )
        data[section]["nodes"].append(node)
        data[section]["issueCount"] += 1
    return {"data": data}


CHECKS = {
    "FAILURE": [("lint", "SUCCESS"), ("unit tests", "SUCCESS"), ("visual snapshots", "FAILURE"),
                ("e2e (chromium)", "SUCCESS")],
    "PENDING": [("lint", "SUCCESS"), ("unit tests", None), ("e2e (chromium)", None)],
    "SUCCESS": [("lint", "SUCCESS"), ("unit tests", "SUCCESS"), ("e2e (chromium)", "SUCCESS")],
}

BODIES = {
    131: "## What\n\nAdds a dark theme to the settings page and remembers the choice per user.\n\n"
         "## Why\n\nThe most requested setting in the feedback board.\n\n"
         "## Notes\n\n- Colours come from the design tokens\n- Snapshots updated for both themes\n"
         "- [x] Tests added",
}


def pr_detail(number):
    row = next(p for p in PRS if p[1] == number)
    _, number, title, author, ci, review, mergeable, merge_state, mins = row
    d = json.load(open(os.path.join(FIXTURES, "gh", "pr-detail.json")))
    p = d["data"]["repository"]["pullRequest"]
    node = pr_lists()["data"][row[0]]["nodes"]
    p.update(next(n for n in node if n["number"] == number))
    p["body"] = BODIES.get(number, f"## What\n\n{title}.\n\n- [x] Tests added")
    p["baseRefName"] = "main"
    p["reviewRequests"] = {"nodes": [{"requestedReviewer": {"__typename": "User", "login": u}}
                                     for u in ("sam", "ana")]}
    p["latestReviews"] = {"nodes": []}
    contexts = [{"__typename": "CheckRun", "name": n, "status": "COMPLETED" if c else "IN_PROGRESS",
                 "conclusion": c} for n, c in CHECKS[ci]]
    p["head"] = {"nodes": [{"commit": {"statusCheckRollup": {"state": ci, "contexts": {"nodes": contexts}}}}]}
    return d


# ---------------------------------------------------------------- Linear

STATES = {
    "triage": ("st-triage", "Triage", "triage"),
    "backlog": ("st-backlog", "Backlog", "backlog"),
    "todo": ("st-todo", "Todo", "unstarted"),
    "progress": ("st-prog", "In Progress", "started"),
    "review": ("st-review", "In Review", "started"),
}

ISSUES = [
    # (id, title, state, priority, project, labels)
    ("ENG-433", "Checkout test fails one run in ten", "todo", 1, "Reliability", ["bug", "flaky"]),
    ("ENG-441", "Bulk-edit widget tags", "todo", 2, "Widgets", ["feature"]),
    ("ENG-444", "Show last sync time on the dashboard", "todo", 3, "Widgets", ["feature", "ui"]),
    ("ENG-446", "Widget names truncate on mobile", "todo", 3, "Widgets", ["bug", "ui"]),
    ("ENG-412", "Migrate billing webhooks to the v2 API", "progress", 2, "Billing", ["backend"]),
    ("ENG-405", "Export widgets as CSV", "progress", 2, "Widgets", ["feature"]),
    ("ENG-420", "Search index rebuild takes 3 minutes", "progress", 3, "Search", ["performance"]),
    ("ENG-398", "Dark mode for the settings page", "review", 3, "Widgets", ["ui"]),
    ("ENG-450", "Audit log for admin actions", "backlog", 4, "Compliance", []),
    ("ENG-452", "Webhook retries ignore Retry-After", "triage", 0, None, ["bug"]),
]


def issue_node(i):
    ident, title, st, prio, project, labels = i
    sid, sname, stype = STATES[st]
    slug = ident.lower()
    return {
        "id": f"issue-{slug}", "identifier": ident, "title": title,
        "url": f"https://linear.app/acme/issue/{ident}",
        "branchName": f"you/{slug}", "priority": prio,
        "state": {"id": sid, "name": sname, "type": stype},
        "team": {"id": "team-eng", "key": "ENG"},
        "project": {"name": project} if project else None,
        "assignee": {"id": "user-you", "name": "You"},
        "labels": {"nodes": [{"name": l} for l in labels]},
    }


def my_issues():
    return {"data": {"viewer": {"id": "user-you", "assignedIssues": {
        "pageInfo": {"hasNextPage": False}, "nodes": [issue_node(i) for i in ISSUES]}}}}


DESCRIPTIONS = {
    "ENG-412": "Stripe retires the v1 webhook API at the end of the quarter.\n\n"
               "- Move the endpoint to v2 event payloads\n- Verify signatures with the new secret\n"
               "- Replay last week's events against staging before switching over",
}


def issue_detail(ident):
    i = next(i for i in ISSUES if i[0] == ident)
    node = issue_node(i)
    node["description"] = DESCRIPTIONS.get(ident, f"{i[1]}.\n\n- Write the fix\n- Add a test")
    return {"data": {"viewer": {"id": "user-you", "name": "You"}, "issue": node}}


def team_states():
    return {"data": {"team": {"states": {"nodes": [
        {"id": sid, "name": n, "type": t, "position": p}
        for p, (sid, n, t) in enumerate(STATES.values())]}}}}


# ---------------------------------------------------------------- fakes

FAKE = r'''#!/usr/bin/env python3
import json, os, sys
d = os.environ["DEMO_DIR"]
args = sys.argv[1:]
text = " ".join(args)
def out(name):
    sys.stdout.write(open(os.path.join(d, name)).read())
tool = os.path.basename(sys.argv[0])
if tool == "gh":
    if "pullRequest(number" in text:
        n = next(a for a in args if a.startswith("number="))[7:]
        out(f"pr-{n}.json")
    elif "graphql" in text:
        out("pr-lists.json")
    else:
        print("[]")
elif tool == "curl":
    body = text + sys.stdin.read()
    if "slack.com" in text:
        print('{"ok":true}')
    elif "assignedIssues" in body:
        out("my-issues.json")
    elif "states" in body:
        out("team-states.json")
    elif "issue(id" in body:
        ident = body.split('"id":"')[-1].split('"')[0] if '"id":"' in body else ""
        out(f"issue-{ident}.json")
    else:
        print('{"data":{}}')
elif tool == "herdr":
    if "workspace" in args and "list" in args:
        print(json.dumps({"id": "cli:workspace:list", "result": {"type": "workspace_list", "workspaces": []}}))
    else:
        print(json.dumps({"id": "cli", "result": {"type": "ok"}}))
'''


def setup(tmp):
    data = os.path.join(tmp, "data")
    bin_ = os.path.join(tmp, "bin")
    home = os.path.join(tmp, "home")
    state_dir = os.path.join(tmp, "state")
    config_dir = os.path.join(tmp, "config")
    for p in (data, bin_, home, state_dir, config_dir):
        os.makedirs(p, mode=0o700)
    files = {"pr-lists.json": pr_lists(), "my-issues.json": my_issues(), "team-states.json": team_states()}
    files.update({f"pr-{p[1]}.json": pr_detail(p[1]) for p in PRS})
    for i in ISSUES:
        # The Inbox may ask by identifier or by node id.
        files[f"issue-{i[0]}.json"] = files[f"issue-issue-{i[0].lower()}.json"] = issue_detail(i[0])
    for name, value in files.items():
        with open(os.path.join(data, name), "w") as f:
            json.dump(value, f)
    for tool in ("gh", "curl", "herdr"):
        path = os.path.join(bin_, tool)
        with open(path, "w") as f:
            f.write(FAKE)
        os.chmod(path, 0o755)
    with open(os.path.join(config_dir, ".env"), "w") as f:
        f.write("LINEAR_API_KEY=demo\nSLACK_USER_TOKEN=demo\nSLACK_CHANNEL=C0DEMO\n"
                "SLACK_FACTORY_MENTION=U0FACTORY\n")
    return data, bin_, home, state_dir, config_dir


def keep_daemon(state_dir, stop):
    """Holds the daemon lock and keeps the state fresh, as the daemon would."""
    lock = open(os.path.join(state_dir, f"daemon-{SESSION}.lock"), "w")
    fcntl.flock(lock, fcntl.LOCK_EX)
    path = os.path.join(state_dir, f"state-{SESSION}.json")
    while not stop.is_set():
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(state(int(time.time() * 1000)), f)
        os.replace(tmp, path)
        stop.wait(5)
    lock.close()


# ---------------------------------------------------------------- tour

UP, DOWN, ESC, ENTER, TAB = "k", "j", "\x1b", "\r", "\t"
RIGHT, LEFT = "\x1b[C", "\x1b[D"

# (seconds to wait first, keys); typed text goes one character at a time.
TOUR = [
    (3.5, None),
    (0.7, DOWN), (0.7, DOWN), (0.7, DOWN), (0.7, DOWN), (0.7, DOWN), (0.7, DOWN),
    (0.7, RIGHT), (1.0, RIGHT), (1.0, RIGHT), (1.0, LEFT), (0.7, LEFT), (0.7, LEFT),
    (1.0, "k"), (0.3, "k"), (0.3, "k"), (0.3, "k"), (0.3, "k"), (0.3, "k"), (0.7, DOWN),
    (1.2, TAB), (0.6, "type:Replayed it. Carry on with the migration."), (1.5, ESC),
    (1.5, "2"), (2.5, DOWN), (0.7, UP), (0.8, ENTER), (3.0, DOWN), (0.4, DOWN), (0.4, DOWN),
    (1.5, "f"), (1.5, DOWN), (0.8, ENTER), (3.0, "n"), (1.0, ESC),
    (1.5, "3"), (2.5, DOWN), (0.8, " "), (0.7, DOWN), (0.7, " "), (1.0, "f"), (3.5, "n"),
    (1.0, ENTER), (3.0, ESC),
    (1.5, "4"), (2.0, DOWN), (0.7, DOWN), (0.7, DOWN), (2.0, "1"), (3.0, None),
]


def tour(fd, finished):
    for wait, keys in TOUR:
        time.sleep(wait)
        if keys is None:
            continue
        if keys.startswith("type:"):
            for ch in keys[5:]:
                os.write(fd, ch.encode())
                time.sleep(0.055)
        else:
            # A lone ESC needs a gap before the next key, or it reads as alt+key.
            os.write(fd, keys.encode())
    # Quit off camera, so the recording ends on the Inbox, not a blank screen.
    finished.set()
    os.write(fd, b"q")


def main():
    exe = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "release", "werkbank")
    tmp = tempfile.mkdtemp(prefix="werkbank-demo-")
    try:
        data, bin_, home, state_dir, config_dir = setup(tmp)
        stop = threading.Event()
        threading.Thread(target=keep_daemon, args=(state_dir, stop), daemon=True).start()
        time.sleep(0.3)
        env = {
            "PATH": f"{bin_}:/usr/bin:/bin",
            "HOME": home,
            "TERM": os.environ.get("TERM", "xterm-256color"),
            "LANG": "en_US.UTF-8",
            "DEMO_DIR": data,
            "HERDR_SESSION": SESSION,
            "HERDR_PLUGIN_STATE_DIR": state_dir,
            "HERDR_PLUGIN_CONFIG_DIR": config_dir,
            "WERKBANK_MASTRA_DB": os.path.join(tmp, "none.db"),
        }
        cols, rows = shutil.get_terminal_size((110, 32))
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(exe, [exe, "inbox"], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        finished = threading.Event()
        threading.Thread(target=tour, args=(fd, finished), daemon=True).start()
        while True:
            try:
                r, _, _ = select.select([fd], [], [], 0.1)
                if r:
                    chunk = os.read(fd, 65536)
                    if not chunk:
                        break
                    if not finished.is_set():
                        os.write(sys.stdout.fileno(), chunk)
            except OSError:
                break
            done, _ = os.waitpid(pid, os.WNOHANG)
            if done:
                break
        stop.set()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    main()
