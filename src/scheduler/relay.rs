//! Question relay through Linear comments.
//!
//! When an agent asks something (`powerqueue task block`, the blocked
//! marker, or a final message that reads like a question) the daemon posts
//! the question on the task's Linear issue ([`question_body`], tagged with
//! [`QUESTION_MARKER`]). On the Linear sync cadence it then reads the
//! issue's new comments:
//!
//! * `needs_attention` / `in_review` with an unanswered question: a human
//!   comment newer than the question is the answer. It is typed into the
//!   live session ([`answer_text`]), or, when the session is gone, kept as
//!   [`RelayState::pending_answer`] and the task is re-queued so the next
//!   launch resumes the session with the answer as its prompt.
//! * `running` / `idle`: a new human comment is typed into the session as
//!   a mid-flight hint ([`hint_text`]).
//!
//! Comments powerqueue posts carry [`OWN_MARKER`] (the question marker
//! starts with it) and their ids are remembered, so they are never relayed
//! back. Everything here is pure; the daemon does the I/O and keeps a
//! [`RelayState`] per task in kv ([`relay_key`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{BLOCKED_MARKER, DONE_MARKER, TaskId};
use crate::linear::IssueComment;

/// Hidden tag on every comment powerqueue posts.
pub const OWN_MARKER: &str = "<!-- powerqueue -->";

/// Hidden tag on a relayed agent question (starts like [`OWN_MARKER`]).
pub const QUESTION_MARKER: &str = "<!-- powerqueue:question -->";

/// Prefix shared by both markers.
const MARKER_PREFIX: &str = "<!-- powerqueue";

/// How many ids of comments powerqueue posted are remembered per task.
const POSTED_IDS_KEPT: usize = 50;

/// Longest question excerpt posted on Linear, in characters.
const QUESTION_MAX_CHARS: usize = 3000;

/// kv key of a task's [`RelayState`].
pub fn relay_key(task: TaskId) -> String {
    format!("relay.{task}")
}

/// A question powerqueue posted on the issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostedQuestion {
    /// Session that asked; the same text from the same session is posted once.
    pub session_id: uuid::Uuid,
    /// The question as posted (without the heading and markers).
    pub text: String,
    /// Linear's id of the comment, when it returned one.
    #[serde(default)]
    pub comment_id: Option<String>,
    pub posted_at: DateTime<Utc>,
    /// A reply was relayed (or the question was superseded).
    #[serde(default)]
    pub answered: bool,
}

/// What the relay remembers about one task (kv [`relay_key`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayState {
    /// The latest question posted for this task.
    #[serde(default)]
    pub question: Option<PostedQuestion>,
    /// Comments created at or before this instant were already handled.
    #[serde(default)]
    pub seen_until: Option<DateTime<Utc>>,
    /// Ids of comments powerqueue posted on the issue (latest last).
    #[serde(default)]
    pub posted: Vec<String>,
    /// An answer waiting for the next launch (the session was gone).
    #[serde(default)]
    pub pending_answer: Option<String>,
    /// Session that asked the question `pending_answer` replies to; the
    /// answer is only used to resume that session.
    #[serde(default)]
    pub pending_session: Option<uuid::Uuid>,
    /// Session whose agent ran `powerqueue task block` itself (set by the
    /// CLI from `POWERQUEUE_SESSION_ID`); its final message is a question.
    #[serde(default)]
    pub agent_blocked: Option<uuid::Uuid>,
}

impl RelayState {
    /// Remember the id of a comment powerqueue posted.
    pub fn record_posted(&mut self, id: &str) {
        if self.posted.iter().any(|p| p == id) {
            return;
        }
        self.posted.push(id.to_string());
        if self.posted.len() > POSTED_IDS_KEPT {
            let excess = self.posted.len() - POSTED_IDS_KEPT;
            self.posted.drain(..excess);
        }
    }

    /// True when `text` from `session` is the latest question and still
    /// waits for a reply (asked again after a reply, it is a new question).
    pub fn already_asked(&self, session: uuid::Uuid, text: &str) -> bool {
        self.open_question().is_some_and(|q| q.session_id == session && q.text == text)
    }

    /// The pending answer, if it replies to a question of `session`.
    pub fn answer_for(&self, session: Option<uuid::Uuid>) -> Option<&str> {
        let answer = self.pending_answer.as_deref()?;
        match self.pending_session {
            Some(asked) if Some(asked) != session => None,
            _ => Some(answer),
        }
    }

    /// The posted question still waiting for a reply.
    pub fn open_question(&self) -> Option<&PostedQuestion> {
        self.question.as_ref().filter(|q| !q.answered)
    }

    /// True for a comment powerqueue wrote itself (marker or remembered id).
    pub fn is_own(&self, comment: &IssueComment) -> bool {
        comment.body.contains(MARKER_PREFIX) || self.posted.contains(&comment.id)
    }

    /// Comments newer than `since` that a human wrote, oldest first, and the
    /// newest `createdAt` among all of `comments` (own ones included), which
    /// becomes the next `seen_until`.
    pub fn human_comments<'a>(
        &self,
        comments: &'a [IssueComment],
        since: DateTime<Utc>,
    ) -> (Vec<&'a IssueComment>, Option<DateTime<Utc>>) {
        let fresh: Vec<&IssueComment> = comments.iter().filter(|c| c.created_at > since).collect();
        let newest = fresh.iter().map(|c| c.created_at).max();
        let human = fresh.into_iter().filter(|c| !self.is_own(c) && !c.body.trim().is_empty()).collect();
        (human, newest)
    }
}

/// Append [`OWN_MARKER`] to a comment body unless it already carries a marker.
pub fn tag_own(body: &str) -> String {
    if body.contains(MARKER_PREFIX) { body.to_string() } else { format!("{}\n\n{OWN_MARKER}", body.trim_end()) }
}

/// The last paragraph of an agent's final message, without the powerqueue
/// markers. Empty when nothing is left.
pub fn last_paragraph(message: &str) -> String {
    let cleaned = message.replace(BLOCKED_MARKER, "").replace(DONE_MARKER, "");
    let paragraphs: Vec<String> = cleaned
        .split("\n\n")
        .map(|p| p.lines().map(str::trim).collect::<Vec<_>>().join("\n").trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    paragraphs.last().cloned().unwrap_or_default()
}

/// The question to relay: the last paragraph of `message`, else `reason`.
/// `None` when both are empty.
pub fn question_text(message: &str, reason: Option<&str>) -> Option<String> {
    let paragraph = last_paragraph(message);
    let text = if paragraph.is_empty() { reason.map(str::trim).unwrap_or_default().to_string() } else { paragraph };
    if text.is_empty() {
        return None;
    }
    Some(truncate_chars(&text, QUESTION_MAX_CHARS))
}

/// The Linear comment for a question: heading, the question, the reason
/// given to `task block` when it adds something, a hint on how to answer,
/// and [`QUESTION_MARKER`].
pub fn question_body(text: &str, reason: Option<&str>) -> String {
    let mut body = format!("🤖 Pregunta del agente\n\n{}", quote(text));
    if let Some(reason) = reason.map(str::trim).filter(|r| !r.is_empty() && !text.contains(r)) {
        body.push_str(&format!("\n\nMotivo: {reason}"));
    }
    body.push_str("\n\nResponde con un comentario en este ticket; powerqueue se lo pasa a la sesión del agente.\n\n");
    body.push_str(QUESTION_MARKER);
    body
}

/// Markdown block quote of `text`.
fn quote(text: &str) -> String {
    text.lines().map(|l| if l.trim().is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

/// The reply as one line for the session: `Reply from <author> on the
/// Linear issue to your question: <text>`.
pub fn answer_text(comments: &[&IssueComment]) -> String {
    format!(
        "Reply from {} on the Linear issue to your question: {} (continue the task with this answer)",
        authors(comments),
        joined(comments)
    )
}

/// A mid-flight hint for a running session.
pub fn hint_text(comments: &[&IssueComment]) -> String {
    format!(
        "New comment from {} on the Linear issue (take it into account; ignore it if you posted it yourself): {}",
        authors(comments),
        joined(comments)
    )
}

fn authors(comments: &[&IssueComment]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for c in comments {
        let name = c.author.as_deref().unwrap_or("a human");
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() { "a human".to_string() } else { names.join(", ") }
}

fn joined(comments: &[&IssueComment]) -> String {
    comments.iter().map(|c| one_line(&c.body)).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" — ")
}

/// Collapse text to one line: tmux `send-keys` submits at the first newline.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn at(mins: i64) -> DateTime<Utc> {
        "2026-10-06T12:00:00Z".parse::<DateTime<Utc>>().unwrap() + Duration::minutes(mins)
    }

    fn comment(id: &str, body: &str, mins: i64) -> IssueComment {
        IssueComment { id: id.into(), body: body.into(), created_at: at(mins), author: Some("Edgar".into()) }
    }

    #[test]
    fn last_paragraph_drops_markers_and_earlier_text() {
        let msg = "I looked at the schema.\n\nShould the new column be nullable?\nThe old rows have no value.\n\n[[POWERQUEUE:BLOCKED]]";
        assert_eq!(last_paragraph(msg), "Should the new column be nullable?\nThe old rows have no value.");
        assert_eq!(last_paragraph("[[POWERQUEUE:BLOCKED]]"), "");
        assert_eq!(question_text("", Some(" need creds ")).as_deref(), Some("need creds"));
        assert_eq!(question_text("  ", None), None);
    }

    #[test]
    fn question_body_quotes_and_tags() {
        let body = question_body("Which table?\nA or B", Some("schema unclear"));
        assert!(body.starts_with("🤖 Pregunta del agente\n\n> Which table?\n> A or B"), "{body}");
        assert!(body.contains("Motivo: schema unclear"), "{body}");
        assert!(body.ends_with(QUESTION_MARKER), "{body}");
        // A reason already in the text is not repeated.
        assert!(!question_body("Which table? schema unclear", Some("schema unclear")).contains("Motivo"));
    }

    #[test]
    fn own_comments_are_never_relayed() {
        let mut state = RelayState::default();
        state.record_posted("c-own");
        let comments = vec![
            comment("c-old", "before the question", 0),
            comment("c-own", "Linear stripped the marker", 2),
            comment("c-q", &question_body("Which?", None), 3),
            comment("c-tagged", &tag_own("powerqueue started attempt 2"), 4),
            comment("c-a", "Use table B.\n\nThanks", 5),
            comment("c-b", "Also keep the index", 6),
        ];
        let (human, newest) = state.human_comments(&comments, at(1));
        let ids: Vec<&str> = human.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["c-a", "c-b"]);
        assert_eq!(newest, Some(at(6)));
        assert_eq!(
            answer_text(&human),
            "Reply from Edgar on the Linear issue to your question: Use table B. Thanks — Also keep the index (continue the task with this answer)"
        );
        assert!(hint_text(&human).starts_with("New comment from Edgar on the Linear issue"));
        // Only own comments: nothing to relay, but the cursor still moves.
        let (human, newest) = state.human_comments(&comments[..4], at(1));
        assert!(human.is_empty());
        assert_eq!(newest, Some(at(4)));
    }

    #[test]
    fn idempotent_per_session_and_text() {
        let s1 = uuid::Uuid::new_v4();
        let mut state = RelayState {
            question: Some(PostedQuestion {
                session_id: s1,
                text: "Which?".into(),
                comment_id: None,
                posted_at: at(0),
                answered: false,
            }),
            ..RelayState::default()
        };
        assert!(state.already_asked(s1, "Which?"));
        assert!(!state.already_asked(s1, "Another?"));
        assert!(!state.already_asked(uuid::Uuid::new_v4(), "Which?"));
        assert!(state.open_question().is_some());
        state.question.as_mut().unwrap().answered = true;
        assert!(state.open_question().is_none());
        assert!(!state.already_asked(s1, "Which?"), "asked again after a reply: a new question");

        state.pending_answer = Some("Use B".into());
        state.pending_session = Some(s1);
        assert_eq!(state.answer_for(Some(s1)), Some("Use B"));
        assert_eq!(state.answer_for(Some(uuid::Uuid::new_v4())), None, "never replayed into another session");
    }

    #[test]
    fn posted_ids_are_capped_and_deduplicated() {
        let mut state = RelayState::default();
        for i in 0..60 {
            state.record_posted(&format!("c{i}"));
        }
        state.record_posted("c59");
        assert_eq!(state.posted.len(), POSTED_IDS_KEPT);
        assert_eq!(state.posted.first().map(String::as_str), Some("c10"));
        assert_eq!(tag_own("x"), format!("x\n\n{OWN_MARKER}"));
        assert_eq!(tag_own(&question_body("q", None)), question_body("q", None));
    }

    #[test]
    fn one_line_collapses_whitespace() {
        assert_eq!(one_line("a\n\n b\tc  "), "a b c");
        assert_eq!(truncate_chars("ñandú", 3), "ñan…");
    }
}

#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    use super::*;
    use crate::strategies::*;

    /// Text with paragraphs, blank lines, markers and odd whitespace.
    fn message() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                line(),
                Just(String::new()),
                Just("   ".to_string()),
                Just(BLOCKED_MARKER.to_string()),
                Just(DONE_MARKER.to_string()),
                (line(), line()).prop_map(|(a, b)| format!("{a}\n{b}")),
            ],
            0..5,
        )
        .prop_map(|parts| parts.join("\n\n"))
    }

    fn comment() -> impl Strategy<Value = IssueComment> {
        (word(), message(), past_instant(), prop::option::of(word())).prop_map(|(id, body, created_at, author)| IssueComment {
            id,
            body,
            created_at,
            author,
        })
    }

    proptest! {
        /// The last paragraph is a non-empty paragraph of the message (with
        /// the markers removed) whenever one exists, else empty.
        #[test]
        fn last_paragraph_is_a_paragraph_of_the_message(msg in message()) {
            let last = last_paragraph(&msg);
            let cleaned = msg.replace(BLOCKED_MARKER, "").replace(DONE_MARKER, "");
            let has_text = cleaned.split("\n\n").any(|p| !p.trim().is_empty());
            prop_assert_eq!(!last.is_empty(), has_text);
            if has_text {
                for line in last.lines() {
                    prop_assert_eq!(line.trim(), line, "lines are trimmed");
                    prop_assert!(cleaned.contains(line), "{line:?} not in {cleaned:?}");
                }
                prop_assert!(!last.starts_with('\n') && !last.ends_with('\n'));
            }
        }

        /// A question is relayed iff the message or the reason says
        /// something; it never exceeds the excerpt limit.
        #[test]
        fn question_text_needs_some_text(msg in message(), reason in prop::option::of(line())) {
            let text = question_text(&msg, reason.as_deref());
            let paragraph = last_paragraph(&msg);
            let expected = !paragraph.is_empty() || reason.as_deref().is_some_and(|r| !r.trim().is_empty());
            prop_assert_eq!(text.is_some(), expected);
            if let Some(text) = &text {
                prop_assert!(text.chars().count() <= QUESTION_MAX_CHARS + 1);
                prop_assert_eq!(text.trim(), text.as_str());
                let body = question_body(text, reason.as_deref());
                prop_assert!(body.ends_with(QUESTION_MARKER));
                prop_assert!(body.contains(MARKER_PREFIX));
                prop_assert_eq!(tag_own(&body), body.clone(), "a tagged body is not tagged twice");
            }
        }

        /// Relayed answers and hints are single lines naming every author
        /// once; comments are recognised as powerqueue's own iff tagged or
        /// remembered.
        #[test]
        fn relayed_text_is_one_line(comments in prop::collection::vec(comment(), 0..4), remembered in prop::option::of(word())) {
            let refs: Vec<&IssueComment> = comments.iter().collect();
            for text in [answer_text(&refs), hint_text(&refs)] {
                prop_assert!(!text.contains('\n'));
                for c in &comments {
                    let author = c.author.as_deref().unwrap_or("a human");
                    prop_assert!(text.contains(author));
                    prop_assert!(text.contains(&one_line(&c.body)));
                }
            }
            let mut state = RelayState::default();
            if let Some(id) = &remembered {
                state.record_posted(id);
            }
            for c in &comments {
                let own = c.body.contains(MARKER_PREFIX) || remembered.as_deref() == Some(c.id.as_str());
                prop_assert_eq!(state.is_own(c), own);
                let tagged = IssueComment { body: tag_own(&c.body), ..c.clone() };
                prop_assert!(state.is_own(&tagged), "tagging marks a comment as powerqueue's own");
                prop_assert!(tagged.body.ends_with(OWN_MARKER) || c.body.contains(MARKER_PREFIX));
            }
        }

        /// `one_line` collapses all whitespace and keeps every word.
        #[test]
        fn one_line_keeps_the_words(text in "[\\s\\S]{0,80}") {
            let flat = one_line(&text);
            prop_assert!(!flat.contains('\n') && !flat.contains('\t'));
            prop_assert!(!flat.contains("  "));
            prop_assert_eq!(flat.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>(), text.split_whitespace().collect::<Vec<_>>());
        }
    }
}
