use std::collections::{BTreeMap, BTreeSet};

use super::*;
pub fn handle(q: u64) -> String {
    format!("Q{q}")
}

pub fn parse_handle(reference: &str) -> Option<u64> {
    let (prefix, digits) = reference.split_at_checked(1)?;
    if !prefix.eq_ignore_ascii_case("q") || digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

// ---------------------------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------------------------

/// A deliberately small YAML reader: the record grammar is flat scalars plus one flow sequence,
/// and hand-rolling it keeps a subtly-different YAML dialect out of the store's meaning. Anything
/// it does not accept is a reported defect rather than a guess.
#[derive(Debug, Default)]
pub struct Frontmatter {
    fields: Vec<(String, String)>,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn required(&self, key: &str) -> std::result::Result<&str, String> {
        self.get(key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("missing required field `{key}`"))
    }
}

/// Splits `---\n<frontmatter>\n---\n<body>`.
pub fn split_document(text: &str) -> std::result::Result<(Frontmatter, String), String> {
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| "record does not start with a `---` frontmatter fence".to_string())?;
    let mut fence_end = None;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            fence_end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let (frontmatter_end, body_start) =
        fence_end.ok_or_else(|| "frontmatter is not terminated by `---`".to_string())?;
    let frontmatter = parse_frontmatter(&rest[..frontmatter_end])?;
    Ok((frontmatter, rest[body_start..].to_string()))
}

pub fn parse_frontmatter(raw: &str) -> std::result::Result<Frontmatter, String> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            // Continuation lines would need block scalars; the record grammar has none, so an
            // indented line means the writer meant something this reader would misread.
            return Err(format!(
                "line {}: indented frontmatter is not part of the record grammar",
                index + 1
            ));
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| format!("line {}: expected `key: value`", index + 1))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("line {}: empty key", index + 1));
        }
        if fields.iter().any(|(existing, _)| existing == key) {
            return Err(format!("line {}: duplicate field `{key}`", index + 1));
        }
        fields.push((key.to_string(), unquote(value.trim()).to_string()));
    }
    Ok(Frontmatter { fields })
}

pub fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        if (first == b'"' || first == b'\'') && bytes[bytes.len() - 1] == first {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Parses a flow sequence `[a, b]`. A bare scalar is a one-element sequence, which is what makes
/// `applies-when: <id> = <option>` and `applies-when: [<id> = <option>]` one grammar with one
/// meaning rather than two encodings that can disagree (DT-R32).
pub fn parse_flow_sequence(raw: &str) -> std::result::Result<Vec<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return Ok(vec![unquote(trimmed).to_string()]);
    };
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|item| {
            let item = unquote(item.trim()).trim();
            if item.is_empty() {
                Err("empty item in list".to_string())
            } else {
                Ok(item.to_string())
            }
        })
        .collect()
}

pub fn parse_term(raw: &str) -> std::result::Result<Term, String> {
    let (decision, option) = raw
        .split_once('=')
        .ok_or_else(|| format!("guard term `{raw}` is not `<record-id> = <option-key>`"))?;
    let decision = decision.trim();
    let option = option.trim();
    if decision.is_empty() || option.is_empty() {
        return Err(format!(
            "guard term `{raw}` is not `<record-id> = <option-key>`"
        ));
    }
    Ok(Term {
        decision: decision.to_string(),
        option: option.to_string(),
    })
}

/// `Q<n>` is a handle, not a record id. Telling them apart is what lets a guard that references a
/// handle report `guard-references-handle` instead of a generic dangling reference.
pub fn looks_like_handle(reference: &str) -> bool {
    parse_handle(reference).is_some()
}

/// `<epoch-ms>-<rand6>.md`. The `rand6` suffix is the record id; nothing orders records by it.
pub fn parse_filename(name: &str) -> Option<(i64, String)> {
    let stem = name.strip_suffix(".md")?;
    let (millis, id) = stem.split_once('-')?;
    if id.is_empty() || id.contains('-') {
        return None;
    }
    Some((millis.parse().ok()?, id.to_string()))
}

pub enum Record {
    Request(Request),
    Answer(Answer),
    Assumption(Assumption),
    Promotion(Promotion),
}

impl Store {
    pub fn push(&mut self, record: Record) {
        match record {
            Record::Request(request) => self.requests.push(request),
            Record::Answer(answer) => self.answers.push(answer),
            Record::Assumption(assumption) => self.assumptions.push(assumption),
            Record::Promotion(promotion) => self.promotions.push(promotion),
        }
    }
}

pub fn parse_record(
    id: &str,
    written_ms: i64,
    text: &str,
) -> std::result::Result<Record, String> {
    let (frontmatter, body) = split_document(text)?;
    match frontmatter.required("record")? {
        "request" => {
            let q = frontmatter
                .required("q")?
                .parse::<u64>()
                .map_err(|_| "`q` is not a number".to_string())?;
            let kind_raw = frontmatter.required("kind")?;
            let kind = Kind::parse(kind_raw)
                .ok_or_else(|| format!("`kind` must be blocker or refinement, got `{kind_raw}`"))?;
            let applies_when = match frontmatter.get("applies-when") {
                Some(raw) if !raw.trim().is_empty() => {
                    let items = parse_flow_sequence(raw)?;
                    if items.is_empty() {
                        return Err("`applies-when` is present but has no terms".to_string());
                    }
                    items
                        .iter()
                        .map(|item| parse_term(item))
                        .collect::<std::result::Result<Vec<_>, _>>()?
                }
                _ => Vec::new(),
            };
            Ok(Record::Request(Request {
                id: id.to_string(),
                written_ms,
                q,
                kind,
                subject: frontmatter.required("subject")?.to_string(),
                asked_by: frontmatter.required("asked-by")?.to_string(),
                about: frontmatter
                    .get("about")
                    .filter(|v| !v.is_empty())
                    .map(String::from),
                parent: frontmatter
                    .get("parent")
                    .filter(|v| !v.is_empty())
                    .map(String::from),
                applies_when,
                body,
            }))
        }
        "answer" => Ok(Record::Answer(Answer {
            id: id.to_string(),
            written_ms,
            answers: frontmatter.required("answers")?.to_string(),
            answered_by: frontmatter.required("answered-by")?.to_string(),
            provenance: match frontmatter.get("provenance") {
                Some("native") => AnswerProvenance::Native,
                Some("imported") => AnswerProvenance::Imported,
                None | Some("unknown") => AnswerProvenance::Unknown,
                Some(other) => return Err(format!("unknown answer provenance `{other}`")),
            },
            choice: match frontmatter.get("choice") {
                Some(raw) => parse_flow_sequence(raw)?,
                None => Vec::new(),
            },
            capture_key: frontmatter.get("capture-key").map(String::from),
            supersedes: frontmatter
                .get("supersedes")
                .filter(|v| !v.is_empty())
                .map(String::from),
            body,
        })),
        "assumption" => Ok(Record::Assumption(Assumption {
            id: id.to_string(),
            written_ms,
            assumes: frontmatter.required("assumes")?.to_string(),
            assumed_by: frontmatter.required("assumed-by")?.to_string(),
            supersedes: frontmatter
                .get("supersedes")
                .filter(|v| !v.is_empty())
                .map(String::from),
            body,
        })),
        "promotion" => Ok(Record::Promotion(Promotion {
            id: id.to_string(),
            written_ms,
            promotes: frontmatter.required("promotes")?.to_string(),
            target: frontmatter.required("target")?.to_string(),
            body,
        })),
        other => Err(format!("unknown `record` discriminator `{other}`")),
    }
}

// ---------------------------------------------------------------------------------------------
// Options and their supportive material
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct OptionBlock {
    /// Human shorthand derived from immutable declaration order; accepted as input while answer
    /// records still use `key`.
    pub id: String,
    pub key: String,
    pub label: Option<String>,
    /// Everything before the first labelled element — what SM-R01 requires to be present.
    pub material: String,
    pub implications: Option<String>,
    pub reversibility: Option<String>,
    pub grounded_by: Option<String>,
    pub not_grounded: Option<String>,
}

pub const LABELLED_ELEMENTS: [&str; 4] = [
    "**Implications:**",
    "**Reversibility:**",
    "**Grounded by:**",
    "**Not grounded:**",
];

pub fn labelled_element(line: &str) -> Option<&'static str> {
    LABELLED_ELEMENTS
        .iter()
        .copied()
        .find(|label| line.trim_start().starts_with(label))
}

/// Extracts the `## Options` section's `### <key> — <label>` blocks.
pub fn parse_options(body: &str) -> Vec<OptionBlock> {
    let mut blocks: Vec<OptionBlock> = Vec::new();
    let mut in_options = false;
    let mut current: Option<(String, Option<String>, Vec<String>)> = None;

    let flush = |blocks: &mut Vec<OptionBlock>,
                 current: Option<(String, Option<String>, Vec<String>)>| {
        if let Some((key, label, lines)) = current {
            blocks.push(build_option_block(key, label, &lines));
        }
    };

    for line in body.lines() {
        if let Some(heading) = line.strip_prefix("### ")
            && in_options
        {
            flush(&mut blocks, current.take());
            let (key, label) = split_option_heading(heading);
            current = Some((key, label, Vec::new()));
            continue;
        }
        if line.starts_with("## ") {
            flush(&mut blocks, current.take());
            in_options = line.trim_end().eq_ignore_ascii_case("## Options");
            continue;
        }
        if let Some((_, _, lines)) = current.as_mut() {
            lines.push(line.to_string());
        }
    }
    flush(&mut blocks, current.take());
    blocks
        .into_iter()
        .enumerate()
        .map(|(index, mut option)| {
            option.id = option_id(index);
            option
        })
        .collect()
}

pub fn option_id(mut index: usize) -> String {
    let mut bytes = Vec::new();
    loop {
        bytes.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    bytes.reverse();
    String::from_utf8(bytes).expect("option ids are ASCII")
}

pub fn split_option_heading(heading: &str) -> (String, Option<String>) {
    for separator in [" — ", " – ", " - "] {
        if let Some((key, label)) = heading.split_once(separator) {
            return (key.trim().to_string(), Some(label.trim().to_string()));
        }
    }
    (heading.trim().to_string(), None)
}

pub fn build_option_block(
    key: String,
    label: Option<String>,
    lines: &[String],
) -> OptionBlock {
    let mut material: Vec<&str> = Vec::new();
    let mut elements: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut current_label: Option<&'static str> = None;

    for line in lines {
        if let Some(found) = labelled_element(line) {
            current_label = Some(found);
            let value = line.trim_start()[found.len()..].trim().to_string();
            elements.entry(found).or_insert(value);
            continue;
        }
        match current_label {
            // A hard-wrapped continuation belongs to the element above it, not to the material.
            Some(label) if !line.trim().is_empty() => {
                if let Some(existing) = elements.get_mut(label) {
                    existing.push(' ');
                    existing.push_str(line.trim());
                }
            }
            Some(_) => {}
            None => material.push(line),
        }
    }

    OptionBlock {
        id: String::new(),
        key,
        label,
        material: material.join("\n").trim().to_string(),
        implications: elements.get("**Implications:**").cloned(),
        reversibility: elements.get("**Reversibility:**").cloned(),
        grounded_by: elements.get("**Grounded by:**").cloned(),
        not_grounded: elements.get("**Not grounded:**").cloned(),
    }
}

/// Presence-and-shape validation for `axe decision ask`. It never judges content: whether the
/// stated grounding is real, or sufficient to have ruled the option out, is not mechanically
/// decidable and is not claimed here (SM-T02).
pub fn validate_request_body(body: &str) -> std::result::Result<(), Vec<String>> {
    let mut problems: Vec<String> = Vec::new();
    let options = parse_options(body);
    if options.is_empty() {
        problems.push(
            "no options found; a request needs a `## Options` section with `### <key>` blocks"
                .to_string(),
        );
        return Err(problems);
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for option in &options {
        let key = option.key.as_str();
        if !seen.insert(key) {
            problems.push(format!("option `{key}`: duplicate option key"));
        }
        if !is_option_key(key) {
            problems.push(format!(
                "option `{key}`: option keys are lowercase `[a-z0-9][a-z0-9-]*`"
            ));
        }
        if option.material.is_empty() {
            problems.push(format!("option `{key}`: no supportive material (SM-R01)"));
        } else if is_bare_reference(&option.material) {
            problems.push(format!(
                "option `{key}`: material is only a link or a path; it must be self-contained \
                 so it is readable wherever the decision is answered (SM-R07)"
            ));
        }
        if option.implications.as_ref().is_none_or(|v| v.is_empty()) {
            problems.push(format!(
                "option `{key}`: no `**Implications:**` line (SM-R02)"
            ));
        }
        if option.reversibility.as_ref().is_none_or(|v| v.is_empty()) {
            problems.push(format!(
                "option `{key}`: no `**Reversibility:**` line (DT-R13)"
            ));
        }
        let grounded = option.grounded_by.as_ref().is_some_and(|v| !v.is_empty());
        let not_grounded = option.not_grounded.as_ref().is_some_and(|v| !v.is_empty());
        if !grounded && !not_grounded {
            problems.push(format!(
                "option `{key}`: neither `**Grounded by:**` nor `**Not grounded:**` (SM-R11, SM-R12)"
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

pub fn is_option_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Material whose entire content is one link or one absolute path is a pointer, not material.
pub fn is_bare_reference(material: &str) -> bool {
    let trimmed = material.trim();
    if trimmed.lines().count() != 1 {
        return false;
    }
    if trimmed.starts_with('/') && !trimmed.contains(char::is_whitespace) {
        return true;
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return !trimmed.contains(char::is_whitespace);
    }
    // A lone `[text](target)` with nothing around it.
    trimmed.starts_with('[') && trimmed.ends_with(')') && trimmed.matches('[').count() == 1 && {
        let after_link = trimmed
            .split_once("](")
            .map(|(_, rest)| rest.trim_end_matches(')'));
        after_link.is_some_and(|target| !target.contains(']'))
    }
}
