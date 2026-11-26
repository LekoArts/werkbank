//! Prompts sent to Mastra Factory, and the exact message each one becomes.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// What a prompt is for. The Inbox shortcuts look prompts up by role, so a
/// prompt can be renamed or reworded without unbinding its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// `f` on one Linear issue; `{ref}` is its URL.
    Issue,
    /// `f` with issues marked; `{refs}` is their URLs, one per line.
    IssueBatch,
    /// `f` on a PR without picking; `{ref}` is its URL. Listed first in the
    /// PR picker.
    PrReview,
    /// More prompts for a PR, in the `f`/`s` picker and the Factory tab.
    Pr,
    /// Only on the Factory tab; `{ref}` is typed in.
    Free,
}

impl Role {
    const ALL: [Role; 5] = [
        Role::Issue,
        Role::IssueBatch,
        Role::PrReview,
        Role::Pr,
        Role::Free,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Prompt {
    pub name: String,
    pub role: Role,
    pub text: String,
}

impl Prompt {
    fn new(name: &str, role: Role, text: &str) -> Self {
        Prompt {
            name: name.into(),
            role,
            text: text.into(),
        }
    }

    /// Sent about one issue or PR: the Factory tab asks for its URL first.
    pub fn needs_ref(&self) -> bool {
        self.text.contains("{ref}") || matches!(self.role, Role::Issue | Role::PrReview | Role::Pr)
    }

    pub fn needs_refs(&self) -> bool {
        self.role == Role::IssueBatch || self.text.contains("{refs}")
    }

    /// The text with `{ref}`/`{refs}` filled in; refs the text doesn't place
    /// go on their own line at the end.
    pub fn fill(&self, refs: &[String]) -> String {
        let one = refs.first().map_or("", String::as_str);
        let list = refs
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut body = self.text.replace("{refs}", &list).replace("{ref}", one);
        let placed = self.text.contains("{ref}") || self.text.contains("{refs}");
        if !placed && !refs.is_empty() {
            body.push('\n');
            body.push_str(if refs.len() == 1 { one } else { &list });
        }
        body.trim().to_string()
    }
}

pub const DEFAULT_SIGNATURE: &str = "Sent via werkbank";

/// Slack's limit for a section block's text; the body is one section.
pub const SECTION_MAX: usize = 3000;

/// Every built-in prompt; a role the file doesn't define gets all of its own.
fn default_prompts() -> Vec<Prompt> {
    vec![
        Prompt::new(
            "Linear issue → PR",
            Role::Issue,
            "Understand the requirements in this ticket and become an expert on its topics. Move from triaging to finished PR ready for review by yourself. Between steps run adversarial reviews to check your own work. Verify each finding against current code. Fix only still-valid issues, skip the rest with a brief reason, keep changes minimal, and validate.\n{ref}",
        ),
        Prompt::new(
            "Linear issues → one PR",
            Role::IssueBatch,
            "Please work on these Linear issues together and open a single PR that covers all of them and is ready for review. Make one commit per issue. If they don't belong together, stop and ask me before combining them. Verify each finding against current code. Fix only still-valid issues, skip the rest with a brief reason, keep changes minimal, and validate.\n{refs}",
        ),
        Prompt::new(
            "Review PR",
            Role::PrReview,
            "Review the following PR. Become an expert on its topics and give useful comments in the context of the PR.\n{ref}",
        ),
        Prompt::new(
            "Fix CI",
            Role::Pr,
            "Please fix the failing CI checks on this PR: {ref}",
        ),
        Prompt::new(
            "Address review",
            Role::Pr,
            "Please address the review comments on this PR: {ref} Verify each finding against current code. Fix only still-valid issues, skip the rest with a brief reason, keep changes minimal, and validate.",
        ),
    ]
}

/// What goes to Slack: the body as the message, the signature as a small
/// line under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub body: String,
    pub signature: Option<String>,
}

impl Message {
    /// The body's length as Slack counts a section's text (UTF-16 units, so
    /// never fewer than the characters), mention and escaping included.
    pub fn slack_len(&self) -> usize {
        self.body.encode_utf16().count()
    }

    /// Slack rejects a section over [`SECTION_MAX`] with `invalid_blocks`.
    pub fn too_long(&self) -> Option<String> {
        let n = self.slack_len();
        (n > SECTION_MAX).then(|| {
            format!(
                "message is {n} characters as Slack counts them (mention and escaped & < > included); the limit is {SECTION_MAX}, so cut at least {}",
                n - SECTION_MAX
            )
        })
    }

    /// The lines the confirm shows, signature last, read the way Slack shows
    /// the escaped refs.
    pub fn preview(&self) -> Vec<String> {
        let body = self
            .body
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        let mut lines: Vec<String> = body.lines().map(String::from).collect();
        if let Some(sig) = &self.signature {
            lines.push(String::new());
            lines.push(format!("— {sig}"));
        }
        lines
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    signature: Option<String>,
    #[serde(default)]
    prompts: Vec<Prompt>,
}

/// The prompts, signature and Factory mention the Inbox composes messages from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Factory {
    pub prompts: Vec<Prompt>,
    pub signature: String,
    /// `<@U…>`; `None` until `SLACK_FACTORY_MENTION` is set.
    pub mention: Option<String>,
    /// Why the prompt file was ignored, for the Inbox notice.
    pub warning: Option<String>,
}

impl Default for Factory {
    fn default() -> Self {
        Factory::from_file(None, None)
    }
}

impl Factory {
    /// `$WERKBANK_FACTORY_CONFIG`, else `$HERDR_PLUGIN_CONFIG_DIR/factory.json`.
    pub fn load() -> Factory {
        let path = Some(crate::core::env("WERKBANK_FACTORY_CONFIG"))
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
                    .map(|d| Path::new(&d).join("factory.json"))
            });
        let text = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok());
        Factory::from_file(
            text.as_deref(),
            crate::core::env_or_dotenv("SLACK_FACTORY_MENTION"),
        )
    }

    /// Defaults fill every role the file leaves out; a file that doesn't parse
    /// is ignored whole and reported.
    pub fn from_file(text: Option<&str>, mention: Option<String>) -> Factory {
        let (file, warning) = match text.map(serde_json::from_str::<File>) {
            None => (None, None),
            Some(Ok(f)) => (Some(f), None),
            Some(Err(e)) => (None, Some(format!("factory.json ignored: {e}"))),
        };
        let (signature, mut prompts) = match file {
            Some(f) => (f.signature, f.prompts),
            None => (None, Vec::new()),
        };
        for role in Role::ALL {
            if !prompts.iter().any(|p| p.role == role) {
                prompts.extend(default_prompts().into_iter().filter(|p| p.role == role));
            }
        }
        Factory {
            prompts,
            signature: signature.unwrap_or_else(|| DEFAULT_SIGNATURE.into()),
            mention: mention.as_deref().and_then(normalize_mention),
            warning,
        }
    }

    /// The first prompt with `role`; defaults guarantee one for every role
    /// but `Free`, which only the file adds.
    pub fn for_role(&self, role: Role) -> &Prompt {
        self.prompts
            .iter()
            .find(|p| p.role == role)
            .expect("defaults fill every role")
    }

    /// The exact Slack message: mention, prompt with refs, signature. `None`
    /// without a mention, since a message that tags nobody starts nothing.
    /// Refs are escaped for Slack, so a URL or a typed ref can't become a
    /// `<!channel>` ping or a link with other text; the prompt itself is yours
    /// and keeps its markup.
    pub fn compose(&self, prompt: &Prompt, refs: &[String]) -> Option<Message> {
        let refs: Vec<String> = refs.iter().map(|r| slack_escape(r)).collect();
        self.compose_message(&prompt.fill(&refs))
    }

    /// A typed message about one ref (a PR's URL), which goes on its own line
    /// under it, escaped like any ref.
    pub fn compose_about(&self, text: &str, r: &str) -> Option<Message> {
        self.compose_message(&format!("{}\n{}", text.trim(), slack_escape(r)))
    }

    /// The prompts a PR's `f`/`s` picker offers: review first, then the rest.
    pub fn pr_prompts(&self) -> Vec<&Prompt> {
        let of = |role| self.prompts.iter().filter(move |p| p.role == role);
        of(Role::PrReview).chain(of(Role::Pr)).collect()
    }

    /// A message typed as-is: mention, the text untouched, signature.
    pub fn compose_message(&self, body: &str) -> Option<Message> {
        let mention = self.mention.as_deref()?;
        let sig = self.signature.trim();
        Some(Message {
            body: format!("{mention} {}", body.trim()),
            signature: (!sig.is_empty()).then(|| sig.to_string()),
        })
    }

    /// A draft an agent wrote: escaped whole, so it can't ping a channel or
    /// hide a link; mrkdwn like `*bold*` and backticks still renders.
    pub fn compose_draft(&self, text: &str) -> Option<Message> {
        self.compose_message(&slack_escape(text.trim()))
    }
}

/// Slack's three control characters in message text.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `U123`, `@U123` and `<@U123>` all become `<@U123>`.
fn normalize_mention(raw: &str) -> Option<String> {
    let id = raw.trim();
    let id = id
        .strip_prefix("<@")
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(id);
    let id = id.strip_prefix('@').unwrap_or(id);
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| format!("<@{id}>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUE: &str = "https://linear.app/acme/issue/ACME-1/a";
    const ISSUE2: &str = "https://linear.app/acme/issue/ACME-2/b";

    fn factory(text: Option<&str>) -> Factory {
        Factory::from_file(text, Some("U0FACTORY".into()))
    }

    #[test]
    fn defaults_cover_every_role_with_the_default_signature() {
        let f = factory(None);
        assert_eq!(f.prompts.len(), 5);
        assert_eq!(f.warning, None);
        assert_eq!(
            f.compose(
                f.for_role(Role::PrReview),
                &["https://github.com/a/b/pull/7".into()]
            )
            .unwrap(),
            Message {
                body: "<@U0FACTORY> Review the following PR. Become an expert on its topics and give useful comments in the context of the PR.\nhttps://github.com/a/b/pull/7".into(),
                signature: Some("Sent via werkbank".into()),
            }
        );
        let pr: Vec<&str> = f.pr_prompts().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(pr, ["Review PR", "Fix CI", "Address review"]);
        assert!(f.prompts.iter().all(|p| p.role != Role::Free));
        let local = f
            .for_role(Role::PrReview)
            .fill(&["https://x/?a=1&b=<2>".into()]);
        assert!(
            local.ends_with("\nhttps://x/?a=1&b=<2>"),
            "local prompts aren't escaped"
        );
        assert_eq!(
            f.compose_about(" look at the flake ", "https://x/1&2")
                .unwrap()
                .body,
            "<@U0FACTORY> look at the flake\nhttps://x/1&amp;2"
        );
    }

    #[test]
    fn batch_lists_every_url_in_the_given_order() {
        let f = factory(None);
        let msg = f
            .compose(f.for_role(Role::IssueBatch), &[ISSUE2.into(), ISSUE.into()])
            .unwrap();
        assert!(
            msg.body.ends_with(&format!("\n- {ISSUE2}\n- {ISSUE}")),
            "{msg:?}"
        );
        assert!(msg.body.contains("one commit per issue"), "{msg:?}");
    }

    #[test]
    fn file_prompts_override_their_role_by_role_not_name() {
        let f = factory(Some(
            r#"{"signature":"via my terminal","prompts":[
                {"name":"Renamed","role":"issue","text":"Do {ref} now"},
                {"name":"Extra","role":"free","text":"Ping"}]}"#,
        ));
        assert_eq!(f.for_role(Role::Issue).name, "Renamed");
        assert_eq!(
            f.prompts.len(),
            6,
            "issue-batch, pr-review and pr defaults fill in; free is the file's alone"
        );
        let m = f.compose(f.for_role(Role::Issue), &[ISSUE.into()]).unwrap();
        assert_eq!(m.body, format!("<@U0FACTORY> Do {ISSUE} now"));
        assert_eq!(m.signature.as_deref(), Some("via my terminal"));
        assert_eq!(
            f.compose(f.for_role(Role::Free), &[]).unwrap().body,
            "<@U0FACTORY> Ping"
        );
    }

    #[test]
    fn a_template_without_a_placeholder_still_carries_the_ref() {
        let f = factory(Some(
            r#"{"prompts":[{"name":"x","role":"pr-review","text":"Review please"}]}"#,
        ));
        assert_eq!(
            f.compose(f.for_role(Role::PrReview), &["U".into()])
                .unwrap()
                .body,
            "<@U0FACTORY> Review please\nU"
        );
    }

    #[test]
    fn a_typed_message_keeps_its_braces_and_lines() {
        let f = Factory::from_file(None, Some("U1".into()));
        let m = f.compose_message("  look at {ref}\nand this  ").unwrap();
        assert_eq!(m.body, "<@U1> look at {ref}\nand this");
        assert_eq!(
            m.preview(),
            ["<@U1> look at {ref}", "and this", "", "— Sent via werkbank"]
        );
        assert_eq!(Factory::from_file(None, None).compose_message("x"), None);
    }

    #[test]
    fn refs_cannot_ping_the_channel() {
        let f = factory(None);
        let m = f
            .compose(
                f.for_role(Role::PrReview),
                &["<!channel> https://x/?a=1&b=2".into()],
            )
            .unwrap();
        assert_eq!(
            m.body,
            "<@U0FACTORY> Review the following PR. Become an expert on its topics and give useful comments in the context of the PR.\n&lt;!channel&gt; https://x/?a=1&amp;b=2"
        );
    }

    #[test]
    fn empty_signature_is_left_off() {
        let f = factory(Some(r#"{"signature":""}"#));
        let m = f.compose(f.for_role(Role::Issue), &[ISSUE.into()]).unwrap();
        assert_eq!(m.signature, None);
        assert!(!m.preview().iter().any(|l| l.contains("werkbank")));
    }

    #[test]
    fn malformed_file_falls_back_to_defaults_with_a_warning() {
        let f = factory(Some(r#"{"prompt":[]}"#));
        assert_eq!(f.prompts, factory(None).prompts);
        assert!(f.warning.unwrap().contains("unknown field"));
        let f = factory(Some(
            r#"{"prompts":[{"name":"x","role":"nope","text":"t"}]}"#,
        ));
        assert!(f.warning.is_some());
    }

    #[test]
    fn mentions_normalise_and_none_means_no_message() {
        for raw in ["U0FACTORY", "@U0FACTORY", "<@U0FACTORY>", " U0FACTORY\n"] {
            assert_eq!(normalize_mention(raw).as_deref(), Some("<@U0FACTORY>"));
        }
        assert_eq!(normalize_mention(""), None);
        assert_eq!(normalize_mention("<!channel>"), None);
        let f = Factory::from_file(None, None);
        assert_eq!(f.compose(f.for_role(Role::Issue), &[ISSUE.into()]), None);
    }

    #[test]
    fn drafts_are_escaped_whole_and_keep_mrkdwn() {
        let m = factory(None)
            .compose_draft("  <!channel> see *this* & `that` <https://x|y>\n")
            .unwrap();
        assert_eq!(
            m.body,
            "<@U0FACTORY> &lt;!channel&gt; see *this* &amp; `that` &lt;https://x|y&gt;"
        );
        assert_eq!(m.signature.as_deref(), Some(DEFAULT_SIGNATURE));
    }

    #[test]
    fn the_limit_counts_mention_escaping_and_utf16() {
        let f = factory(None);
        let room = SECTION_MAX - "<@U0FACTORY> ".len();
        let fits = f.compose_draft(&"a".repeat(room)).unwrap();
        assert_eq!(fits.slack_len(), SECTION_MAX);
        assert_eq!(fits.too_long(), None);
        let amp = f
            .compose_draft(&format!("{}&", "a".repeat(room - 4)))
            .unwrap();
        assert_eq!(amp.slack_len(), SECTION_MAX + 1);
        let err = amp.too_long().unwrap();
        assert!(
            err.contains("3001") && err.contains("cut at least 1"),
            "{err}"
        );
        // An emoji is two UTF-16 units: one character, counted as two.
        let emoji = f
            .compose_draft(&format!("{}🦀", "a".repeat(room - 1)))
            .unwrap();
        assert!(emoji.too_long().is_some());
    }
}
