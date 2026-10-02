//! Structured person requests: a typed question with named answers, carried on a person ask.
//!
//! A request is one of four types. A `decision` proposes one action and names what accepting,
//! declining and (optionally) requesting changes each do. A `choice` names two to five options.
//! `feedback` asks for text. Each named answer has a stable ID, so the asker dispatches on that
//! ID instead of reading prose. An `update` asks nothing: it brings the person information they
//! asked for, names the work or message where they asked (`about`), and clears once read. An
//! ask without a request stays a free-text ask.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::St3Error;

pub const REQUEST_VERSION: u32 = 1;
const MAX_ANSWERS: usize = 5;
const MAX_REASONS: usize = 8;
const MAX_SUBJECTS: usize = 12;
const MAX_CONDITIONS: usize = 6;
const MAX_LINE: usize = 600;
const MAX_TEXT: usize = 8_000;
const SUBJECT_KINDS: &[&str] = &[
    "pull_request",
    "issue",
    "document",
    "mission",
    "run",
    "step",
    "agent",
    "host",
    "commit",
    "link",
];

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestType {
    Decision,
    Choice,
    Feedback,
    Update,
}

impl RequestType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Choice => "choice",
            Self::Feedback => "feedback",
            Self::Update => "update",
        }
    }
}

/// What a decision answer does to the proposed action.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    Accept,
    Decline,
    RequestChanges,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StructuredRequest {
    pub version: u32,
    #[serde(rename = "type")]
    pub request_type: RequestType,
    /// The one concrete question. Required, except on an update, which asks nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Why only this person can answer: no runtime fact or standing instruction settles it.
    /// Required, except on an update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_person: Option<String>,
    /// An update's proof that the person asked: their mission run or step run, or their
    /// message to the agent posting the update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// Absent means the asker makes no recommendation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<Recommendation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<RequestSubject>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<AnswerOption>,
    /// A choice may also take a custom answer in the person's words.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Recommendation {
    pub answer: String,
    pub reason: String,
}

/// A thing the question is about. `revision` pins what was reviewed, such as a PR head.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RequestSubject {
    pub kind: String,
    pub label: String,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnswerOption {
    pub id: String,
    pub label: String,
    /// What happens next when the person gives this answer.
    pub consequence: String,
    /// Required on a decision answer; absent on a choice option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<DecisionOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<String>,
}

/// A person's answer as submitted: a named answer `id`, text, or both.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnswerInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

fn invalid(message: impl Into<String>) -> St3Error {
    St3Error::new("invalid-person-request", message.into())
}

fn invalid_answer(message: impl Into<String>) -> St3Error {
    St3Error::new("invalid-person-answer", message.into())
}

fn line(name: &str, value: &str) -> Result<(), St3Error> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{name} must not be empty")));
    }
    if value.chars().count() > MAX_LINE {
        return Err(invalid(format!(
            "{name} must be at most {MAX_LINE} characters"
        )));
    }
    Ok(())
}

fn answer_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

impl StructuredRequest {
    /// Parses and checks a request, returning its canonical JSON for the ask claim.
    pub fn parse(value: &Value) -> Result<(Self, Value), St3Error> {
        let request: Self = serde_json::from_value(value.clone())
            .map_err(|error| invalid(format!("the request is malformed: {error}")))?;
        request.validate()?;
        let canonical =
            serde_json::to_value(&request).map_err(|error| invalid(error.to_string()))?;
        Ok((request, canonical))
    }

    pub fn validate(&self) -> Result<(), St3Error> {
        if self.version != REQUEST_VERSION {
            return Err(invalid(format!(
                "this daemon reads request version {REQUEST_VERSION}"
            )));
        }
        if self.request_type == RequestType::Update {
            if self.question.is_some() || self.why_person.is_some() {
                return Err(invalid(
                    "an update asks nothing: it has no question or why_person",
                ));
            }
            line(
                "about (the run, step or message where the person asked)",
                self.about.as_deref().unwrap_or_default(),
            )?;
        } else {
            line("question", self.question.as_deref().unwrap_or_default())?;
            line("why_person", self.why_person.as_deref().unwrap_or_default())?;
            if self.about.is_some() {
                return Err(invalid("only an update names what it is about"));
            }
        }
        if let Some(summary) = &self.summary {
            line("summary", summary)?;
        }
        if self.reasons.len() > MAX_REASONS {
            return Err(invalid(format!("at most {MAX_REASONS} reasons")));
        }
        for reason in &self.reasons {
            line("a reason", reason)?;
        }
        if self.subjects.len() > MAX_SUBJECTS {
            return Err(invalid(format!("at most {MAX_SUBJECTS} subjects")));
        }
        for subject in &self.subjects {
            if !SUBJECT_KINDS.contains(&subject.kind.as_str()) {
                return Err(invalid(format!(
                    "subject kind `{}` is not one of {}",
                    subject.kind,
                    SUBJECT_KINDS.join(", ")
                )));
            }
            line("a subject label", &subject.label)?;
            if subject.reference.is_none() && subject.url.is_none() {
                return Err(invalid(format!(
                    "subject `{}` needs a ref or a url",
                    subject.label
                )));
            }
            if subject
                .url
                .as_deref()
                .is_some_and(|url| !(url.starts_with("https://") || url.starts_with("http://")))
            {
                return Err(invalid(format!(
                    "subject `{}` url must be http or https",
                    subject.label
                )));
            }
            for (name, value) in [("ref", &subject.reference), ("revision", &subject.revision)] {
                if let Some(value) = value {
                    line(&format!("subject {name}"), value)?;
                }
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for answer in &self.answers {
            if !answer_id(&answer.id) {
                return Err(invalid(format!(
                    "answer id `{}` must be lowercase letters, digits and dashes, starting with a letter",
                    answer.id
                )));
            }
            if !ids.insert(answer.id.as_str()) {
                return Err(invalid(format!("answer id `{}` repeats", answer.id)));
            }
            line("an answer label", &answer.label)?;
            line("an answer consequence", &answer.consequence)?;
            if answer.conditions.len() > MAX_CONDITIONS {
                return Err(invalid(format!(
                    "at most {MAX_CONDITIONS} conditions per answer"
                )));
            }
            for condition in &answer.conditions {
                line("a condition", condition)?;
            }
        }
        match self.request_type {
            RequestType::Decision => {
                let count = |outcome| {
                    self.answers
                        .iter()
                        .filter(|answer| answer.outcome == Some(outcome))
                        .count()
                };
                if self.answers.iter().any(|answer| answer.outcome.is_none())
                    || count(DecisionOutcome::Accept) != 1
                    || count(DecisionOutcome::Decline) != 1
                    || count(DecisionOutcome::RequestChanges) > 1
                {
                    return Err(invalid(
                        "a decision names one accept answer, one decline answer and at most one request_changes answer",
                    ));
                }
                if self.custom {
                    return Err(invalid("only a choice takes a custom answer"));
                }
            }
            RequestType::Choice => {
                if !(2..=MAX_ANSWERS).contains(&self.answers.len()) {
                    return Err(invalid(format!(
                        "a choice names 2 to {MAX_ANSWERS} options"
                    )));
                }
                if self.answers.iter().any(|answer| answer.outcome.is_some()) {
                    return Err(invalid("choice options have no outcome"));
                }
            }
            RequestType::Feedback => {
                if !self.answers.is_empty() || self.custom || self.recommendation.is_some() {
                    return Err(invalid(
                        "feedback asks for text: it has no answers, custom answer or recommendation",
                    ));
                }
            }
            RequestType::Update => {
                if !self.answers.is_empty()
                    || self.custom
                    || self.recommendation.is_some()
                    || !self.reasons.is_empty()
                {
                    return Err(invalid(
                        "an update asks nothing: it has no answers, reasons or recommendation",
                    ));
                }
            }
        }
        if let Some(recommendation) = &self.recommendation {
            if !ids.contains(recommendation.answer.as_str()) {
                return Err(invalid(format!(
                    "the recommendation names `{}`, which is not one of the answers",
                    recommendation.answer
                )));
            }
            line("the recommendation reason", &recommendation.reason)?;
        }
        Ok(())
    }

    /// Resolves a submitted answer to the typed answer the done claim records. A feedback
    /// request also accepts a bare summary as its text, so a client that only sends text can
    /// still answer it.
    pub fn answer(&self, input: Option<&AnswerInput>, summary: &str) -> Result<Value, St3Error> {
        if self.request_type == RequestType::Update {
            // Opening an update or pressing read clears it; any summary is only history.
            if input.is_some_and(|input| {
                input.text.is_some() || input.id.as_deref().is_some_and(|id| id != "read")
            }) {
                return Err(invalid_answer(
                    "an update takes no answer; it is only read. Reply in a conversation",
                ));
            }
            return Ok(json!({"type": "update", "outcome": "read"}));
        }
        let fallback;
        let input = match input {
            Some(input) => input,
            None if self.request_type == RequestType::Feedback && !summary.trim().is_empty() => {
                fallback = AnswerInput {
                    id: None,
                    text: Some(summary.to_owned()),
                };
                &fallback
            }
            None => {
                return Err(St3Error::new(
                    "answer-required",
                    format!(
                        "this {} needs one of its named answers: {}",
                        self.request_type.as_str(),
                        self.answer_ids()
                    ),
                ));
            }
        };
        let text = input
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        if text.is_some_and(|text| text.chars().count() > MAX_TEXT) {
            return Err(invalid_answer(format!(
                "answer text must be at most {MAX_TEXT} characters"
            )));
        }
        let mut answer = json!({"type": self.request_type.as_str()});
        match (self.request_type, input.id.as_deref()) {
            (RequestType::Update, _) => unreachable!("an update returns above"),
            (RequestType::Feedback, Some(_)) => {
                return Err(invalid_answer("feedback has no named answers; send text"));
            }
            (RequestType::Feedback, None) => {
                let text = text.ok_or_else(|| invalid_answer("feedback needs text"))?;
                answer["outcome"] = json!("feedback");
                answer["text"] = json!(text);
                return Ok(answer);
            }
            (RequestType::Choice, None) => {
                if !self.custom {
                    return Err(St3Error::new(
                        "answer-required",
                        format!(
                            "this choice needs one of its options: {}",
                            self.answer_ids()
                        ),
                    ));
                }
                let text = text.ok_or_else(|| invalid_answer("a custom answer needs text"))?;
                answer["outcome"] = json!("custom");
                answer["text"] = json!(text);
                return Ok(answer);
            }
            (RequestType::Decision, None) => {
                return Err(St3Error::new(
                    "answer-required",
                    format!(
                        "this decision needs one of its named answers: {}",
                        self.answer_ids()
                    ),
                ));
            }
            (_, Some(id)) => {
                let option = self
                    .answers
                    .iter()
                    .find(|option| option.id == id)
                    .ok_or_else(|| {
                        invalid_answer(format!(
                            "`{id}` is not one of this request's answers: {}",
                            self.answer_ids()
                        ))
                    })?;
                let outcome = match option.outcome {
                    Some(DecisionOutcome::Accept) => "accept",
                    Some(DecisionOutcome::Decline) => "decline",
                    Some(DecisionOutcome::RequestChanges) => {
                        if text.is_none() {
                            return Err(invalid_answer(
                                "requesting changes needs text naming the changes",
                            ));
                        }
                        "request_changes"
                    }
                    None => "selected",
                };
                answer["outcome"] = json!(outcome);
                answer["id"] = json!(option.id);
                answer["label"] = json!(option.label);
                if let Some(text) = text {
                    answer["text"] = json!(text);
                }
            }
        }
        Ok(answer)
    }

    fn answer_ids(&self) -> String {
        let mut ids = self
            .answers
            .iter()
            .map(|answer| answer.id.as_str())
            .collect::<Vec<_>>();
        if self.request_type == RequestType::Choice && self.custom {
            ids.push("or custom text");
        }
        ids.join(", ")
    }
}

/// The human-readable history line for a typed answer.
pub fn answer_summary(answer: &Value) -> String {
    let text = answer["text"].as_str();
    match (answer["label"].as_str(), text) {
        (Some(label), Some(text)) => format!("{label}: {text}"),
        (Some(label), None) => label.to_owned(),
        (None, Some(text)) => text.to_owned(),
        (None, None) if answer["outcome"] == "read" => "Read".into(),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision() -> Value {
        json!({
            "version": 1,
            "type": "decision",
            "question": "Change the browser sign-in, then review the gateway again?",
            "why_person": "Only the owner decides how browsers authenticate.",
            "recommendation": {"answer": "revise-auth", "reason": "Native clients already pair this way."},
            "subjects": [{"kind": "pull_request", "label": "#41", "url": "https://example.com/pull/41", "revision": "abc123"}],
            "answers": [
                {"id": "land", "label": "Land the gateway", "outcome": "accept", "consequence": "The gateway merges as it is."},
                {"id": "keep-open", "label": "Keep it open", "outcome": "decline", "consequence": "Nothing merges."},
                {"id": "revise-auth", "label": "Revise auth and review", "outcome": "request_changes", "consequence": "The author changes sign-in and asks again."}
            ]
        })
    }

    #[test]
    fn a_decision_records_its_named_outcome_and_needs_text_for_changes() {
        let (request, canonical) = StructuredRequest::parse(&decision()).unwrap();
        assert_eq!(canonical["subjects"][0]["revision"], "abc123");
        let accept = request
            .answer(
                Some(&AnswerInput {
                    id: Some("land".into()),
                    text: None,
                }),
                "",
            )
            .unwrap();
        assert_eq!(
            accept,
            json!({"type": "decision", "outcome": "accept", "id": "land", "label": "Land the gateway"})
        );
        assert_eq!(answer_summary(&accept), "Land the gateway");
        let changes = AnswerInput {
            id: Some("revise-auth".into()),
            text: None,
        };
        assert_eq!(
            request.answer(Some(&changes), "").unwrap_err().code,
            "invalid-person-answer"
        );
        let changes = AnswerInput {
            text: Some("Use device pairing.".into()),
            ..changes
        };
        let answer = request.answer(Some(&changes), "").unwrap();
        assert_eq!(answer["outcome"], "request_changes");
        assert_eq!(
            answer_summary(&answer),
            "Revise auth and review: Use device pairing."
        );
        // Prose never selects an answer.
        assert_eq!(
            request.answer(None, "yes").unwrap_err().code,
            "answer-required"
        );
        let unknown = AnswerInput {
            id: Some("yes".into()),
            text: None,
        };
        assert_eq!(
            request.answer(Some(&unknown), "").unwrap_err().code,
            "invalid-person-answer"
        );
    }

    #[test]
    fn malformed_requests_are_refused() {
        let mut two_accepts = decision();
        two_accepts["answers"][1]["outcome"] = json!("accept");
        let mut bad_recommendation = decision();
        bad_recommendation["recommendation"]["answer"] = json!("merge");
        let mut unknown_field = decision();
        unknown_field["deadline"] = json!("tomorrow");
        let mut no_reason = decision();
        no_reason["why_person"] = json!(" ");
        let mut bad_id = decision();
        bad_id["answers"][0]["id"] = json!("Land");
        let mut feedback_with_answers = decision();
        feedback_with_answers["type"] = json!("feedback");
        let mut one_option = json!({
            "version": 1, "type": "choice", "question": "Which?", "why_person": "Taste.",
            "answers": [{"id": "a", "label": "A", "consequence": "A happens."}]
        });
        let mut newer = decision();
        newer["version"] = json!(2);
        for request in [
            two_accepts,
            bad_recommendation,
            unknown_field,
            no_reason,
            bad_id,
            feedback_with_answers,
            one_option.take(),
            newer,
        ] {
            assert_eq!(
                StructuredRequest::parse(&request).unwrap_err().code,
                "invalid-person-request",
                "{request}"
            );
        }
    }

    #[test]
    fn an_update_names_where_it_was_asked_for_and_is_only_read() {
        let update = json!({
            "version": 1, "type": "update", "about": "mission-run/example/report/1",
            "summary": "The nightly build is green again.",
            "subjects": [{"kind": "document", "label": "Report", "ref": "doc/example/report"}]
        });
        let (request, canonical) = StructuredRequest::parse(&update).unwrap();
        assert_eq!(canonical, update);
        let read = json!({"type": "update", "outcome": "read"});
        assert_eq!(request.answer(None, "").unwrap(), read);
        assert_eq!(answer_summary(&read), "Read");
        let pressed = AnswerInput {
            id: Some("read".into()),
            text: None,
        };
        assert_eq!(request.answer(Some(&pressed), "").unwrap(), read);
        for input in [
            AnswerInput {
                id: Some("yes".into()),
                text: None,
            },
            AnswerInput {
                id: None,
                text: Some("Thanks, now do the next one".into()),
            },
        ] {
            assert_eq!(
                request.answer(Some(&input), "").unwrap_err().code,
                "invalid-person-answer"
            );
        }
        let mut no_about = update.clone();
        no_about.as_object_mut().unwrap().remove("about");
        let mut asks = update.clone();
        asks["question"] = json!("Is this fine?");
        let mut answers = update.clone();
        answers["answers"] =
            json!([{"id": "ok", "label": "OK", "consequence": "Nothing changes."}]);
        let mut about_on_decision = decision();
        about_on_decision["about"] = json!("mission-run/example/report/1");
        let mut decision_without_question = decision();
        decision_without_question
            .as_object_mut()
            .unwrap()
            .remove("question");
        for request in [
            no_about,
            asks,
            answers,
            about_on_decision,
            decision_without_question,
        ] {
            assert_eq!(
                StructuredRequest::parse(&request).unwrap_err().code,
                "invalid-person-request",
                "{request}"
            );
        }
    }

    #[test]
    fn choice_takes_an_option_or_allowed_custom_text_and_feedback_takes_text() {
        let choice = json!({
            "version": 1, "type": "choice", "question": "How should an oversized tree load?",
            "why_person": "It changes what every client shows.", "custom": true,
            "answers": [
                {"id": "paged", "label": "Page it", "consequence": "Clients read pages with explicit metadata."},
                {"id": "limit", "label": "Fail past a limit", "consequence": "Large trees return an error."}
            ]
        });
        let (request, _) = StructuredRequest::parse(&choice).unwrap();
        let selected = request
            .answer(
                Some(&AnswerInput {
                    id: Some("paged".into()),
                    text: None,
                }),
                "",
            )
            .unwrap();
        assert_eq!(selected["outcome"], "selected");
        let custom = request
            .answer(
                Some(&AnswerInput {
                    id: None,
                    text: Some("Page it, but warn".into()),
                }),
                "",
            )
            .unwrap();
        assert_eq!(
            custom,
            json!({"type": "choice", "outcome": "custom", "text": "Page it, but warn"})
        );
        let feedback = json!({
            "version": 1, "type": "feedback", "question": "What should the page say?",
            "why_person": "It is the owner's voice.",
            "subjects": [{"kind": "document", "label": "Draft", "ref": "doc/example/draft"}]
        });
        let (request, _) = StructuredRequest::parse(&feedback).unwrap();
        assert_eq!(
            request.answer(None, "Shorter, please.").unwrap(),
            json!({"type": "feedback", "outcome": "feedback", "text": "Shorter, please."})
        );
        assert_eq!(
            request
                .answer(
                    Some(&AnswerInput {
                        id: Some("ok".into()),
                        text: None
                    }),
                    ""
                )
                .unwrap_err()
                .code,
            "invalid-person-answer"
        );
    }
}
