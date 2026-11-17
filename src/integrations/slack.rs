//! Posts one message to one Slack channel. Write-only by design: the only
//! endpoint is `chat.postMessage`, so the token needs nothing but `chat:write`.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::core::runner::Runner;

const POST: &[&str] = &[
    "curl",
    "-sS",
    "--connect-timeout",
    "5",
    "--max-time",
    "15",
    "https://slack.com/api/chat.postMessage",
    "-H",
    "Content-Type: application/x-www-form-urlencoded",
    "-H",
    "@-",
];

/// Where to post and with which user token.
#[derive(Clone, PartialEq, Eq)]
pub struct Slack {
    pub token: String,
    pub channel: String,
}

impl std::fmt::Debug for Slack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slack")
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

impl Slack {
    /// `SLACK_USER_TOKEN` and `SLACK_CHANNEL`, from the env or the plugin `.env`.
    pub fn load() -> Option<Slack> {
        Some(Slack {
            token: crate::core::env_or_dotenv("SLACK_USER_TOKEN")?,
            channel: crate::core::env_or_dotenv("SLACK_CHANNEL")?,
        })
    }

    /// Posts `text` to the channel. The token goes to curl as a header on
    /// stdin (`-H @-`), so it is never in argv, `ps`, or an error message.
    /// With a `footer`, the message is two blocks: `text` as the body and the
    /// footer as a small context line under it; `text` stays the plain
    /// fallback that notifications show.
    pub fn post_message<R: Runner>(
        &self,
        runner: &R,
        text: &str,
        footer: Option<&str>,
    ) -> Result<()> {
        let header = format!("Authorization: Bearer {}\n", self.token);
        let argv = post_argv(&self.channel, text, footer);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let out = runner
            .run(&argv, Some(header.as_bytes()), None)
            // The runner's timeout error quotes argv, which holds the whole message.
            .map_err(|_| anyhow!("Slack: curl did not finish"))?;
        if !out.success() {
            bail!("Slack: curl exited {}: {}", out.status, out.stderr.trim());
        }
        parse_reply(&out.stdout)
    }
}

/// The argv `post_message` runs; no secret in it.
pub fn post_argv(channel: &str, text: &str, footer: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = POST
        .iter()
        .map(|s| s.to_string())
        .chain([
            "--data-urlencode".into(),
            format!("channel={channel}"),
            "--data-urlencode".into(),
            format!("text={text}"),
        ])
        .collect();
    if let Some(footer) = footer {
        let blocks = json!([
            {"type": "section", "text": {"type": "mrkdwn", "text": text}},
            {"type": "context", "elements": [{"type": "mrkdwn", "text": footer}]},
        ]);
        argv.push("--data-urlencode".into());
        argv.push(format!("blocks={blocks}"));
    }
    argv
}

fn parse_reply(text: &str) -> Result<()> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| anyhow!("Slack: invalid JSON reply: {e}"))?;
    if v["ok"].as_bool() == Some(true) {
        return Ok(());
    }
    bail!("Slack: {}", v["error"].as_str().unwrap_or("request failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::{FakeRunner, Output};

    const OK: &str = include_str!("../../tests/fixtures/slack/ok.json");
    const ERR: &str = include_str!("../../tests/fixtures/slack/error.json");
    const TOKEN: &str = "xoxp-test-not-a-real-token";

    fn slack() -> Slack {
        Slack {
            token: TOKEN.into(),
            channel: "C0000000001".into(),
        }
    }

    fn argv(text: &str) -> Vec<String> {
        post_argv("C0000000001", text, None)
    }

    fn refs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn posts_to_chat_post_message_with_the_token_only_on_stdin() {
        let f = FakeRunner::new();
        let a = argv("hi & bye");
        f.ok(&refs(&a), None, OK);
        slack().post_message(&f, "hi & bye", None).unwrap();
        let calls = f.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]
                .argv
                .iter()
                .filter(|a| a.contains("slack.com"))
                .collect::<Vec<_>>(),
            ["https://slack.com/api/chat.postMessage"]
        );
        assert!(calls[0].argv.contains(&"channel=C0000000001".to_string()));
        assert!(calls[0].argv.contains(&"text=hi & bye".to_string()));
        assert!(calls[0]
            .argv
            .iter()
            .all(|a| !a.contains(TOKEN) && !a.starts_with("token=")));
        assert_eq!(
            calls[0].stdin.as_deref(),
            Some(format!("Authorization: Bearer {TOKEN}\n").as_bytes())
        );
    }

    #[test]
    fn slack_errors_and_curl_failures_never_show_the_token() {
        let f = FakeRunner::new();
        let a = argv("x");
        f.ok(&refs(&a), None, ERR);
        let err = slack().post_message(&f, "x", None).unwrap_err().to_string();
        assert_eq!(err, "Slack: not_in_channel");

        let f = FakeRunner::new();
        f.respond(
            &refs(&a),
            None,
            Output {
                status: 6,
                stdout: String::new(),
                stderr: "Could not resolve host: slack.com".into(),
            },
        );
        let err = slack().post_message(&f, "x", None).unwrap_err().to_string();
        assert!(err.contains("curl exited 6"), "{err}");
        assert!(!err.contains(TOKEN));

        let f = FakeRunner::new();
        f.fail(&refs(&a), None);
        let err = slack().post_message(&f, "x", None).unwrap_err().to_string();
        assert_eq!(err, "Slack: curl did not finish");

        let f = FakeRunner::new();
        f.ok(&refs(&a), None, "<html>");
        assert!(slack().post_message(&f, "x", None).is_err());

        assert!(!format!("{:?}", slack()).contains(TOKEN));
    }

    #[test]
    fn a_footer_goes_under_the_body_as_a_context_block() {
        let a = post_argv("C1", "<@U1> hi \"there\"", Some("via werkbank"));
        assert!(a.contains(&"text=<@U1> hi \"there\"".to_string()), "{a:?}");
        let blocks = a.last().unwrap().strip_prefix("blocks=").unwrap();
        let v: Value = serde_json::from_str(blocks).unwrap();
        assert_eq!(v[0]["type"], "section");
        assert_eq!(v[0]["text"]["text"], "<@U1> hi \"there\"");
        assert_eq!(v[1]["type"], "context");
        assert_eq!(v[1]["elements"][0]["text"], "via werkbank");
        assert!(!post_argv("C1", "x", None)
            .iter()
            .any(|s| s.starts_with("blocks=")));
    }
}
