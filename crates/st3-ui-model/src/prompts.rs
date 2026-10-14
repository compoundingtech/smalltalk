//! A native harness prompt as web, tui and ios show it: which of the answers the daemon offers a
//! client may show, and the form they read as, worded and numbered like the harness's own.
//!
//! The TypeScript twin is `clients/typescript/st3-views/promptForm.ts` (with `offeredAnswers` in
//! `homeView.ts`); both are checked against `clients/typescript/st3-views/tests/fixtures/
//! prompt-forms.json`, so web, tui and ios offer the same answers with the same words.
//!
//! Only answers the daemon lists in `action_parameters["prompt.respond"].answers` are ever shown,
//! and what an answer means is the daemon's: a label says it in the harness's words, and never
//! more. Claude's own dialog may offer more (keep allowing, block from now on); those stay in its
//! terminal, and the form says so.

use serde_json::Value;

/// A tool call the seat's harness recorded and has no result for yet: its tool and its input.
#[derive(Clone, Copy, Debug)]
pub struct Call<'a> {
    pub tool: &'a str,
    pub input: &'a Value,
}

/// Whose prompt it is: the words on a form are that harness's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
    Other,
}

/// What a prompt asks, read from the answers the daemon lists for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// May a tool call run: `allow`, `deny`.
    Permission,
    /// May a plan go ahead: `approve`, `deny`.
    Plan,
    /// The harness's question: `answer` with the choices, or `deny`.
    Question,
    /// Claude's new MCP server dialog: `use`, `use-all`, `skip`.
    Mcp,
    /// A workspace trust dialog: `trust`, and `exit` or `quit`.
    Trust,
    /// A call Claude's auto mode denied: `approve-once`, `approve-kind`, `dismiss`.
    Denial,
    Other,
}

impl Kind {
    pub fn of(answers: &[String]) -> Self {
        let has = |answer: &str| answers.iter().any(|offered| offered == answer);
        if has("answer") {
            Self::Question
        } else if has("approve") {
            Self::Plan
        } else if has("approve-once") || has("approve-kind") {
            Self::Denial
        } else if has("use") || has("use-all") || has("skip") {
            Self::Mcp
        } else if has("trust") {
            Self::Trust
        } else if has("allow") {
            Self::Permission
        } else {
            Self::Other
        }
    }
}

/// A call's input as lines a person reads: each field on its own line, with a value's further
/// lines indented under it, so a multi-line command or an edit's text is there in full.
pub fn call_lines(input: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match input {
        Value::Object(fields) => {
            for (key, value) in fields {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => serde_json::to_string_pretty(other).unwrap_or_default(),
                };
                let mut rest = text.lines();
                lines.push(format!("{key}: {}", rest.next().unwrap_or_default()));
                lines.extend(rest.map(|line| format!("  {line}")));
            }
        }
        Value::Null => {}
        Value::String(text) => lines.extend(text.lines().map(str::to_owned)),
        other => lines.extend(
            serde_json::to_string_pretty(other)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned),
        ),
    }
    lines
}

/// One option of a harness question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub description: String,
}

/// One question of a harness's pending `AskUserQuestion` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub text: String,
    pub header: String,
    pub options: Vec<Choice>,
    pub multi: bool,
}

/// The questions an `AskUserQuestion` call asks, when it is one and every question can be read.
pub fn questions(call: Call<'_>) -> Option<Vec<Question>> {
    if call.tool != "AskUserQuestion" {
        return None;
    }
    let asked = call.input.get("questions")?.as_array()?;
    let questions = asked
        .iter()
        .map(|question| {
            let options = question
                .get("options")?
                .as_array()?
                .iter()
                .map(|option| {
                    Some(Choice {
                        label: option
                            .get("label")?
                            .as_str()
                            .filter(|label| !label.is_empty())?
                            .to_owned(),
                        description: option
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Question {
                text: question.get("question")?.as_str()?.to_owned(),
                header: question
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                options,
                multi: question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!questions.is_empty() && questions.iter().all(|question| !question.options.is_empty()))
        .then_some(questions)
}

/// The answers a client may offer, as `offeredAnswers` in `st3-views/homeView.ts`: `allow` and
/// `approve` only when exactly one call is unanswered and it can be shown, since only then is it
/// certain what they let through; `answer` only when that call's questions can be shown and
/// chosen from; every other answer (deny, and a screen prompt's own answers) always.
pub fn offered_answers(answers: &[String], calls: &[Call<'_>]) -> Vec<String> {
    let certain = matches!(calls, [only] if !call_lines(only.input).is_empty());
    answers
        .iter()
        .filter(|answer| match answer.as_str() {
            "allow" | "approve" => certain,
            "answer" => matches!(calls, [only] if questions(*only).is_some()),
            _ => true,
        })
        .cloned()
        .collect()
}

/// One numbered option of a form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormOption {
    /// The answer st sends for it.
    pub answer: String,
    /// The harness's words for that answer.
    pub label: String,
    /// Enter may answer it when it is marked: the safe answer, about what is shown in full.
    /// Every other answer takes its own number or a click.
    pub enter: bool,
    /// It refuses or ends something (deny, exit, quit): never marked at first and never under
    /// Enter.
    pub refuses: bool,
}

/// A prompt as a form: the harness's question line and its options, numbered from 1 in the
/// order the harness shows them, with the one Enter would take marked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    pub kind: Kind,
    pub question: String,
    pub options: Vec<FormOption>,
    /// The option marked at first: the first one Enter may take, else the first that refuses
    /// nothing, else none.
    pub mark: Option<usize>,
    /// The harness's dialog has choices st cannot send (keep allowing, auto-accept edits): they
    /// are in its terminal, and this says so in the form.
    pub elsewhere: Option<String>,
}

/// Whether Enter may take `answer` when it is marked: it lets through only what the form shows in
/// full (a call, a plan, the person's own choices) or turns something away safely.
pub fn enter_answers(answer: &str) -> bool {
    matches!(answer, "allow" | "approve" | "answer" | "skip")
}

/// Whether `answer` refuses or ends something.
pub fn refuses(answer: &str) -> bool {
    matches!(answer, "deny" | "exit" | "quit")
}

fn file_name(input: &Value) -> Option<&str> {
    let path = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .or_else(|| input.get("path"))?
        .as_str()?;
    path.rsplit('/').find(|part| !part.is_empty())
}

/// The harness's question line for a prompt.
fn question_line(harness: Harness, kind: Kind, prompt: &str, call: Option<Call<'_>>) -> String {
    let first = prompt
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let tool = call.map(|call| call.tool).unwrap_or_default();
    match (harness, kind) {
        (Harness::Claude, Kind::Permission) => match tool {
            "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" => {
                "Allow this read outside the working directories?".into()
            }
            "Edit" | "MultiEdit" | "NotebookEdit" => match call.and_then(|call| file_name(call.input)) {
                Some(name) => format!("Do you want to make this edit to {name}?"),
                None => "Do you want to make this edit?".into(),
            },
            "Write" => match call.and_then(|call| file_name(call.input)) {
                Some(name) => format!("Do you want to create {name}?"),
                None => "Do you want to create this file?".into(),
            },
            "WebFetch" => "Do you want to allow Claude to fetch this content?".into(),
            _ => "Do you want to proceed?".into(),
        },
        (Harness::Codex, Kind::Permission) => {
            let tool = tool.to_ascii_lowercase();
            if tool.contains("patch") || tool.contains("file") || tool.contains("edit") {
                "Would you like to make the following edits?".into()
            } else if tool.contains("permission") {
                "Would you like to grant these permissions?".into()
            } else {
                "Would you like to run the following command?".into()
            }
        }
        (_, Kind::Plan) => "Would you like to proceed?".into(),
        (_, Kind::Question) => "Ready to submit your answers?".into(),
        (_, Kind::Denial) => "Claude's auto mode denied this call. Allow it?".into(),
        (_, Kind::Permission) => "Do you want to proceed?".into(),
        _ => first.to_owned(),
    }
}

/// The harness's words for an answer st sends on a prompt of `kind`.
pub fn answer_label(harness: Harness, kind: Kind, tool: Option<&str>, answer: &str) -> String {
    let read = matches!(tool, Some("Read" | "Glob" | "Grep" | "LS" | "NotebookRead"));
    match (harness, kind, answer) {
        // Claude's read dialog says what a one-time answer is; st's answers are one-time.
        (Harness::Claude, Kind::Permission, "allow") if read => "Yes, but ask again next time".into(),
        (Harness::Claude, Kind::Permission, "deny") if read => "No, and ask again next time".into(),
        (Harness::Codex, Kind::Permission, "allow") => "Yes, proceed".into(),
        (_, Kind::Permission, "allow") => "Yes".into(),
        (_, Kind::Permission, "deny") => "No".into(),
        (_, Kind::Plan, "approve") => "Yes".into(),
        (_, Kind::Plan, "deny") => "No, keep planning".into(),
        (_, Kind::Question, "answer") => "Submit answers".into(),
        (_, Kind::Question, "deny") => "No, decline to answer".into(),
        (_, _, "use") => "Use this MCP server".into(),
        (_, _, "use-all") => "Use this and all future MCP servers in this project".into(),
        (_, _, "skip") => "Continue without using this MCP server".into(),
        (Harness::Codex, _, "trust") => "Yes, continue".into(),
        (_, _, "trust") => "Yes, I trust this folder".into(),
        (_, _, "exit") => "No, exit".into(),
        (_, _, "quit") => "No, quit".into(),
        (_, _, "approve-once") => "Yes, allow it once".into(),
        (_, _, "approve-kind") => "Yes, and allow calls of its kind for this seat".into(),
        (_, _, "dismiss") => "No, leave it denied".into(),
        (_, _, "allow") => "Yes".into(),
        (_, _, "deny") => "No".into(),
        (_, _, other) => other.to_owned(),
    }
}

/// The form for a prompt: `answers` as the daemon lists them, `prompt` its alert's detail, and
/// `calls` the calls it may be about (the one it names, or every unanswered one).
pub fn prompt_form(harness: Harness, answers: &[String], prompt: &str, calls: &[Call<'_>]) -> Form {
    let kind = Kind::of(answers);
    let call = match calls {
        [only] => Some(*only),
        _ => None,
    };
    let mut offered = offered_answers(answers, calls);
    // Refusing comes last, as in the harness's own dialogs.
    offered.sort_by_key(|answer| answer == "deny");
    let options: Vec<FormOption> = offered
        .iter()
        .map(|answer| FormOption {
            answer: answer.clone(),
            label: answer_label(harness, kind, call.map(|call| call.tool), answer),
            enter: enter_answers(answer),
            refuses: refuses(answer),
        })
        .collect();
    let mark = options
        .iter()
        .position(|option| option.enter)
        .or_else(|| options.iter().position(|option| !option.refuses));
    let elsewhere = match (harness, kind) {
        (Harness::Claude, Kind::Permission | Kind::Plan) => {
            Some("Claude's other choices are in its terminal (Ctrl+]).".into())
        }
        (Harness::Codex, Kind::Permission) => {
            Some("Codex's other choices are in its terminal (Ctrl+]).".into())
        }
        _ => None,
    };
    Form {
        kind,
        question: question_line(harness, kind, prompt, call),
        options,
        mark,
        elsewhere,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases web, tui and ios share: each prompt's form, and what it offers.
    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        harness: String,
        answers: Vec<String>,
        prompt: String,
        calls: Vec<CaseCall>,
        form: Expected,
    }

    #[derive(serde::Deserialize)]
    struct CaseCall {
        tool: String,
        input: Value,
    }

    #[derive(serde::Deserialize)]
    struct Expected {
        question: String,
        options: Vec<ExpectedOption>,
        mark: Option<usize>,
        elsewhere: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct ExpectedOption {
        answer: String,
        label: String,
        enter: bool,
    }

    #[test]
    fn every_shared_case_reads_the_same_in_rust() {
        let cases: Vec<Case> = serde_json::from_str(include_str!(
            "../../../clients/typescript/st3-views/tests/fixtures/prompt-forms.json"
        ))
        .unwrap();
        assert!(cases.len() >= 8);
        for case in cases {
            let harness = match case.harness.as_str() {
                "claude" => Harness::Claude,
                "codex" => Harness::Codex,
                _ => Harness::Other,
            };
            let calls: Vec<Call<'_>> = case
                .calls
                .iter()
                .map(|call| Call {
                    tool: &call.tool,
                    input: &call.input,
                })
                .collect();
            let form = prompt_form(harness, &case.answers, &case.prompt, &calls);
            assert_eq!(form.question, case.form.question, "{}", case.name);
            assert_eq!(form.mark, case.form.mark, "{}", case.name);
            assert_eq!(form.elsewhere, case.form.elsewhere, "{}", case.name);
            let shown: Vec<(&str, &str, bool)> = form
                .options
                .iter()
                .map(|option| (option.answer.as_str(), option.label.as_str(), option.enter))
                .collect();
            let wanted: Vec<(&str, &str, bool)> = case
                .form
                .options
                .iter()
                .map(|option| (option.answer.as_str(), option.label.as_str(), option.enter))
                .collect();
            assert_eq!(shown, wanted, "{}", case.name);
        }
    }

    #[test]
    fn allow_needs_one_call_shown_and_answer_needs_its_questions() {
        let answers = |words: &[&str]| words.iter().map(|word| (*word).to_owned()).collect::<Vec<_>>();
        let ls = serde_json::json!({"command": "ls"});
        let empty = serde_json::json!({});
        let bash = Call { tool: "Bash", input: &ls };
        assert_eq!(offered_answers(&answers(&["allow", "deny"]), &[]), ["deny"]);
        assert_eq!(offered_answers(&answers(&["allow", "deny"]), &[bash]), ["allow", "deny"]);
        assert_eq!(offered_answers(&answers(&["allow", "deny"]), &[bash, bash]), ["deny"]);
        assert_eq!(
            offered_answers(&answers(&["allow", "deny"]), &[Call { tool: "Bash", input: &empty }]),
            ["deny"]
        );
        assert_eq!(offered_answers(&answers(&["answer", "deny"]), &[bash]), ["deny"]);
        assert_eq!(offered_answers(&answers(&["trust", "exit"]), &[]), ["trust", "exit"]);
    }

    #[test]
    fn refusing_is_never_marked_first_or_under_enter() {
        let form = prompt_form(Harness::Claude, &["deny".into(), "allow".into()], "", &[]);
        assert_eq!(form.options.len(), 1);
        assert_eq!(form.mark, None, "only deny is offered, so nothing is marked");
        assert!(!form.options[0].enter);
        let trust = prompt_form(Harness::Claude, &["exit".into(), "trust".into()], "Trust?", &[]);
        assert_eq!(trust.mark, Some(1), "trust, not exit, is marked");
        assert!(trust.options.iter().all(|option| !option.enter));
    }
}
