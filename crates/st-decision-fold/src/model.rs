use std::fmt;
use thiserror::Error;
use crate::parsing::{handle, parse_handle};

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum DecisionError {
    #[error("{0}")]
    Validation(String),
}

pub type Result<T, E = DecisionError> = std::result::Result<T, E>;

pub(crate) fn invalid(message: impl Into<String>) -> DecisionError {
    DecisionError::Validation(message.into())
}

// ---------------------------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------------------------

/// A condition the fold deliberately cannot represent as a state (DT-R16, DT-R37). Every defect
/// names the record it is about so `check` can point at a file rather than at the store.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Defect {
    /// The record the defect is attributed to; `None` for store-wide conditions.
    pub about: Option<String>,
    pub code: DefectCode,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DefectCode {
    /// The frontmatter could not be read as a record at all.
    MalformedRecord,
    /// A guard term names `Q<n>` rather than a record id. References are byte-exact identifiers
    /// so a handle collision can never corrupt one (DT-R36).
    GuardReferencesHandle,
    /// A guard term names a record that is not a request in this store.
    DanglingReference,
    /// A guard term names an option the referenced request does not offer.
    UnknownOption,
    /// A guard whose term list is present but empty, or whose term is not `<id> = <option>`.
    MalformedGuard,
    /// A guard cycle. Detected by every guard form, which is why it did not discriminate in the
    /// guard-design eval — it is checked here because it still has to be reported.
    CyclicReference,
    /// A guard term names a decision whose current answer selected no option, so which way it
    /// went is not recoverable from the record and no further answer is coming on its own.
    AnswerSelectsNoOption,
    /// Two answers (or two assumptions) name the same predecessor, so `supersedes` no longer
    /// totally orders the chain (DT-R06).
    ForkedSupersession,
    /// `supersedes` names a record that is not a prior answer/assumption to the same request.
    DanglingSupersession,
    /// Two requests in one tree carry the same handle (DT-R37).
    DuplicateHandle,
    /// A promotion whose `target` no longer resolves. Decision numbers are not unique across the
    /// tree and files are renumbered, so this is an expected condition to detect.
    PromotionTargetUnresolvable,
    /// A record referencing a request that does not exist in this store.
    OrphanRecord,
    /// This decision's own guard is well-formed, but a decision it names has no state. Reported
    /// on the dependent too, so every undecidable decision says why it is undecidable rather
    /// than only the one at the root of the chain.
    DependsOnUndecidable,
    /// More than one record carries the same immutable identifier.
    DuplicateId,
    /// The fold's bounded record, guard-term or guard-depth contract was exceeded.
    LimitExceeded,
    /// Imported historical answers cannot settle a live guard.
    ImportedAnswer,
}

impl DefectCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MalformedRecord => "malformed-record",
            Self::GuardReferencesHandle => "guard-references-handle",
            Self::DanglingReference => "dangling-reference",
            Self::UnknownOption => "unknown-option",
            Self::MalformedGuard => "malformed-guard",
            Self::CyclicReference => "cyclic-reference",
            Self::AnswerSelectsNoOption => "answer-selects-no-option",
            Self::ForkedSupersession => "forked-supersession",
            Self::DanglingSupersession => "dangling-supersession",
            Self::DuplicateHandle => "duplicate-handle",
            Self::PromotionTargetUnresolvable => "promotion-target-unresolvable",
            Self::OrphanRecord => "orphan-record",
            Self::DependsOnUndecidable => "depends-on-undecidable",
            Self::DuplicateId => "duplicate-id",
            Self::LimitExceeded => "limit-exceeded",
            Self::ImportedAnswer => "imported-answer",
        }
    }
}

impl fmt::Display for DefectCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(crate) fn defect(about: Option<&str>, code: DefectCode, detail: impl Into<String>) -> Defect {
    Defect {
        about: about.map(str::to_string),
        code,
        detail: detail.into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Record model
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Kind {
    /// Parks the asking agent until an answer exists. Blocking is derived from `kind` and
    /// nothing else (DT-R11), and `kind` is immutable (DT-T06).
    Blocker,
    /// Queued; the agent proceeds and writes an assumption record as it does.
    Refinement,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::Refinement => "refinement",
        }
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "blocker" => Some(Self::Blocker),
            "refinement" => Some(Self::Refinement),
            _ => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One conjunct of a guard: DQ1 resolved to form B, a conjunction of positive equalities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Term {
    /// The referenced request's record id (DT-R36 — never its handle).
    pub decision: String,
    pub option: String,
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}", self.decision, self.option)
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub id: String,
    pub written_ms: i64,
    pub q: u64,
    pub kind: Kind,
    pub subject: String,
    pub asked_by: String,
    pub about: Option<String>,
    /// Structural hierarchy only. Gating is expressed by `applies_when` and by nothing else.
    pub parent: Option<String>,
    /// Empty when the request carries no guard.
    pub applies_when: Vec<Term>,
    pub body: String,
}

/// Provenance is assigned by the trusted adapter, not inferred from `answered_by`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnswerProvenance {
    Native,
    Imported,
}

#[derive(Clone, Debug)]
pub struct Answer {
    pub id: String,
    pub written_ms: i64,
    pub answers: String,
    pub answered_by: String,
    pub provenance: AnswerProvenance,
    /// Zero, one or several option keys (DT-R27). Empty means the answer is entirely free text.
    pub choice: Vec<String>,
    /// Stable tool-call event marker; never a supersession key.
    pub capture_key: Option<String>,
    pub supersedes: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct Assumption {
    pub id: String,
    pub written_ms: i64,
    pub assumes: String,
    pub assumed_by: String,
    pub supersedes: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct Promotion {
    pub id: String,
    pub written_ms: i64,
    pub promotes: String,
    pub target: String,
    pub body: String,
}

#[derive(Clone, Debug, Default)]
pub struct Store {
    pub requests: Vec<Request>,
    pub answers: Vec<Answer>,
    pub assumptions: Vec<Assumption>,
    pub promotions: Vec<Promotion>,
    /// Defects found while reading the store, before any fold runs.
    pub parse_defects: Vec<Defect>,
}

impl Store {
    pub fn request(&self, id: &str) -> Option<&Request> {
        let mut matches = self.requests.iter().filter(|r| r.id == id);
        let one = matches.next()?;
        if matches.next().is_some()
            || self.answers.iter().any(|r| r.id == id)
            || self.assumptions.iter().any(|r| r.id == id)
            || self.promotions.iter().any(|r| r.id == id)
        {
            return None;
        }
        Some(one)
    }

    /// Resolves `Q<n>` (case-insensitively) or a record id to a request. A handle that matches more than one request
    /// is ambiguous rather than arbitrarily resolved — that is the DT-R37 defect surfacing at
    /// lookup time.
    pub fn resolve(&self, reference: &str) -> Result<&Request> {
        if let Some(q) = parse_handle(reference) {
            let matches: Vec<&Request> = self.requests.iter().filter(|r| r.q == q).collect();
            return match matches.as_slice() {
                [one] => self.request(&one.id)
                    .ok_or_else(|| invalid(format!("decision identifier `{}` is ambiguous", one.id))),
                [] => Err(invalid(format!("no decision with handle `{}`", handle(q)))),
                many => Err(invalid(format!(
                    "handle `{}` is ambiguous across {} requests ({}); \
                         run `axe decision check`",
                    handle(q),
                    many.len(),
                    many.iter()
                        .map(|r| r.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
            };
        }
        self.request(reference)
            .ok_or_else(|| invalid(format!("no decision `{reference}`")))
    }

    /// One above the highest handle; gaps are permanent. Exhaustion is an error,
    /// never wrapping or returning a handle that is already allocated.
    pub fn next_handle(&self) -> Result<u64> {
        self.requests
            .iter()
            .map(|r| r.q)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("decision handles exhausted at u64::MAX"))
    }

    pub(crate) fn answers_to<'a>(&'a self, request_id: &str) -> Vec<&'a Answer> {
        self.answers
            .iter()
            .filter(|a| a.answers == request_id)
            .collect()
    }

    pub(crate) fn assumptions_to<'a>(&'a self, request_id: &str) -> Vec<&'a Assumption> {
        let mut assumptions: Vec<_> = self
            .assumptions
            .iter()
            .filter(|a| a.assumes == request_id)
            .collect();
        for index in 0..assumptions.len() {
            let (ordered, remaining) = assumptions.split_at_mut(index);
            // The predecessor link, not the millisecond filename or random id, defines order.
            let next = ordered
                .last()
                .and_then(|previous| {
                    remaining
                        .iter()
                        .position(|a| a.supersedes.as_deref() == Some(previous.id.as_str()))
                })
                .or_else(|| remaining.iter().position(|a| a.supersedes.is_none()))
                // Keep unmatched records visible instead of discarding them.
                .unwrap_or(0);
            remaining[..=next].rotate_right(1);
        }
        assumptions
    }

    pub(crate) fn promotions_to<'a>(&'a self, request_id: &str) -> Vec<&'a Promotion> {
        self.promotions
            .iter()
            .filter(|p| p.promotes == request_id)
            .collect()
    }
}
