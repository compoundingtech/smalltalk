use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::model::defect;
use super::*;

/// Maximum total records (including parse defects) accepted by a fold.
pub const MAX_RECORDS: usize = 65_536;
/// Maximum number of dependency edges from any request to a guard leaf.
pub const MAX_GUARD_DEPTH: usize = 64;
/// Maximum conjuncts on one request.
pub const MAX_GUARD_TERMS: usize = 64;
// ---------------------------------------------------------------------------------------------
// The fold
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum State {
    Pending,
    Gated,
    Answered,
    Moot,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Gated => "gated",
            Self::Answered => "answered",
            Self::Moot => "moot",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "gated" => Some(Self::Gated),
            "answered" => Some(Self::Answered),
            "moot" => Some(Self::Moot),
            _ => None,
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The fold is total: every request resolves to exactly one of the four states or to
/// `Undecidable`, which is **not** a fifth state but the reported-defect outcome (DT-R16).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resolution {
    State(State),
    Undecidable,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::State(state) => state.as_str(),
            Self::Undecidable => "undecidable",
        }
    }

    pub fn state(self) -> Option<State> {
        match self {
            Self::State(state) => Some(state),
            Self::Undecidable => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Truth {
    True,
    False,
    Undetermined,
    Invalid,
}

/// `Invalid` dominates so a broken term is never masked by a satisfied one; `False` beats
/// `Undetermined` so a conjunction with one definitively-unsatisfied term is moot rather than
/// waiting on the rest.
pub(super) fn conjoin(truths: &[Truth]) -> Truth {
    if truths.contains(&Truth::Invalid) {
        Truth::Invalid
    } else if truths.contains(&Truth::False) {
        Truth::False
    } else if truths.contains(&Truth::Undetermined) {
        Truth::Undetermined
    } else {
        Truth::True
    }
}

#[derive(Clone, Debug)]
pub struct Fold {
    pub resolutions: BTreeMap<String, Resolution>,
    pub defects: Vec<Defect>,
    /// Currently `pending` or `gated`, and `moot` under some earlier prefix of the answer log
    /// (DT-R17). Revival is a report, never a mark on the immutable request.
    pub revived: BTreeSet<String>,
}

impl Fold {
    pub fn resolution(&self, id: &str) -> Resolution {
        self.resolutions
            .get(id)
            .copied()
            .unwrap_or(Resolution::Undecidable)
    }
}

/// The answer selected for each request under one prefix of the answer log.
pub(super) type AnswerSet<'a> = BTreeMap<&'a str, &'a Answer>;

pub(super) struct Evaluator<'a> {
    requests: BTreeMap<&'a str, &'a Request>,
    answers: AnswerSet<'a>,
    resolutions: BTreeMap<String, Resolution>,
    defects: Vec<Defect>,
    /// Option keys per request, parsed once — the guard checks `unknown-option` against them.
    options: BTreeMap<&'a str, Vec<String>>,
}

impl<'a> Evaluator<'a> {
    fn new(store: &'a Store, answers: AnswerSet<'a>, invalid: &BTreeSet<&str>) -> Self {
        let requests: BTreeMap<_, _> = store.requests.iter()
            .map(|request| (request.id.as_str(), request)).collect();
        let options = requests.iter()
            .filter(|(id, _)| !invalid.contains(**id))
            .map(|(id, request)| (
                *id,
                parse_options(&request.body).into_iter().map(|option| option.key).collect(),
            )).collect();
        Self {
            requests,
            answers,
            resolutions: invalid.iter().map(|id| ((*id).to_string(), Resolution::Undecidable)).collect(),
            defects: Vec::new(),
            options,
        }
    }

    fn report(&mut self, about: &str, code: DefectCode, detail: impl Into<String>) {
        // fold sorts and deduplicates once, avoiding quadratic reporting costs.
        self.defects.push(defect(Some(about), code, detail));
    }

    fn term_truth(&mut self, owner: &str, term: &Term) -> Truth {
        if looks_like_handle(&term.decision) {
            self.report(
                owner,
                DefectCode::GuardReferencesHandle,
                format!(
                    "guard term `{term}` names the handle `{}`; guards carry the record id \
                     so a handle collision cannot corrupt a reference (DT-R36)",
                    term.decision
                ),
            );
            return Truth::Invalid;
        }
        let Some(&referenced) = self.requests.get(term.decision.as_str()) else {
            self.report(
                owner,
                DefectCode::DanglingReference,
                format!("guard term `{term}` names no request in this tree"),
            );
            return Truth::Invalid;
        };
        let known = self
            .options
            .get(term.decision.as_str())
            .is_some_and(|keys| keys.iter().any(|key| key == &term.option));
        if !known {
            self.report(
                owner,
                DefectCode::UnknownOption,
                format!(
                    "guard term `{term}`: {} offers no option `{}`",
                    handle(referenced.q),
                    term.option
                ),
            );
            return Truth::Invalid;
        }
        match self.answers.get(term.decision.as_str()).map(|answer| answer.provenance) {
            Some(AnswerProvenance::Imported) => {
                self.report(owner, DefectCode::ImportedAnswer,
                    format!("guard term `{term}` depends on an imported historical answer"));
                return Truth::Invalid;
            }
            Some(AnswerProvenance::Unknown) => {
                self.report(owner, DefectCode::UnknownAnswerProvenance,
                    format!("guard term `{term}` depends on an answer with unknown provenance"));
                return Truth::Invalid;
            }
            Some(AnswerProvenance::Native) | None => {}
        }
        match self.resolutions.get(term.decision.as_str()).copied()
            .unwrap_or(Resolution::Undecidable)
        {
            Resolution::Undecidable => {
                self.report(
                    owner,
                    DefectCode::DependsOnUndecidable,
                    format!(
                        "guard term `{term}`: {} has no state, so this one cannot have one either",
                        handle(referenced.q)
                    ),
                );
                Truth::Invalid
            }
            // Definitive: a moot decision will never be answered, so the term can never hold.
            Resolution::State(State::Moot) => Truth::False,
            Resolution::State(State::Pending) | Resolution::State(State::Gated) => {
                Truth::Undetermined
            }
            Resolution::State(State::Answered) => {
                let answer = self
                    .answers
                    .get(term.decision.as_str())
                    .expect("an answered decision has a current answer");
                if answer.choice.is_empty() {
                    // The answer exists and selected nothing, so which way it went is not
                    // recoverable and no further answer is coming on its own. Calling that
                    // `gated` would hide a branch that will never become answerable (DT-R16).
                    self.report(
                        owner,
                        DefectCode::AnswerSelectsNoOption,
                        format!(
                            "guard term `{term}`: the current answer to {} selected no option, \
                             so the guard cannot be evaluated from the record",
                            handle(referenced.q)
                        ),
                    );
                    Truth::Invalid
                } else if answer.choice.iter().any(|key| key == &term.option) {
                    Truth::True
                } else {
                    Truth::False
                }
            }
        }
    }

    fn resolve(&mut self, request: &Request) -> Resolution {
        let truths: Vec<_> = request.applies_when.iter()
            .map(|term| self.term_truth(&request.id, term)).collect();
        match conjoin(&truths) {
            Truth::Invalid => Resolution::Undecidable,
            // Moot outranks answered; stored answers remain in history.
            Truth::False => Resolution::State(State::Moot),
            Truth::Undetermined => Resolution::State(State::Gated),
            Truth::True => Resolution::State(if self.answers.contains_key(request.id.as_str()) {
                State::Answered
            } else {
                State::Pending
            }),
        }
    }

    fn run(mut self) -> (BTreeMap<String, Resolution>, Vec<Defect>) {
        // Explicit postorder DFS: even hostile-depth inputs never use the call stack.
        // Sorted roots make cycle diagnostics independent of input vector order.
        let ids: Vec<_> = self.requests.keys().copied().collect();
        for &id in &ids {
            if let Some(answer) = self.answers.get(id).copied() {
                let unknown = answer.choice.iter().any(|choice| {
                    !self.options.get(id).is_some_and(|keys| keys.contains(choice))
                });
                if unknown {
                    self.report(id, DefectCode::UnknownOption,
                        format!("current answer `{}` selects an option not offered by this request", answer.id));
                    self.resolutions.insert(id.to_string(), Resolution::Undecidable);
                }
            }
        }
        let mut completed = BTreeSet::new();
        let mut depths: BTreeMap<&str, usize> = BTreeMap::new();
        for &id in &ids {
            if self.requests[id].applies_when.len() > MAX_GUARD_TERMS {
                self.report(id, DefectCode::LimitExceeded,
                    format!("guard exceeds {MAX_GUARD_TERMS} terms"));
                self.resolutions.insert(id.to_string(), Resolution::Undecidable);
            }
            if self.resolutions.contains_key(id) {
                completed.insert(id);
            }
        }
        for root in ids {
            if completed.contains(root) {
                continue;
            }
            let mut stack = vec![(root, 0usize)];
            let mut active = BTreeMap::from([(root, 0usize)]);
            while let Some(&(id, next)) = stack.last() {
                let request = self.requests[id];
                if let Some(term) = request.applies_when.get(next) {
                    stack.last_mut().expect("nonempty traversal").1 += 1;
                    let Some((&target, _)) = self.requests.get_key_value(term.decision.as_str()) else {
                        continue;
                    };
                    if completed.contains(target) {
                        continue;
                    }
                    if let Some(&start) = active.get(target) {
                        let cycle: Vec<_> = stack[start..].iter().map(|(id, _)| *id).collect();
                        let detail = format!("guard cycle through `{}`", cycle.join(" -> "));
                        for member in cycle {
                            self.report(member, DefectCode::CyclicReference, detail.clone());
                            self.resolutions.insert(member.to_string(), Resolution::Undecidable);
                        }
                    } else {
                        active.insert(target, stack.len());
                        stack.push((target, 0));
                    }
                    continue;
                }
                stack.pop();
                active.remove(id);
                let depth = request.applies_when.iter()
                    .filter_map(|term| depths.get(term.decision.as_str()))
                    .max().map_or(0, |depth| depth.saturating_add(1));
                depths.insert(id, depth);
                if !self.resolutions.contains_key(id) {
                    let resolution = if depth > MAX_GUARD_DEPTH {
                        self.report(id, DefectCode::LimitExceeded,
                            format!("guard dependency depth exceeds {MAX_GUARD_DEPTH} edges"));
                        Resolution::Undecidable
                    } else {
                        self.resolve(request)
                    };
                    self.resolutions.insert(id.to_string(), resolution);
                }
                completed.insert(id);
            }
        }
        (self.resolutions, self.defects)
    }
}

/// The `supersedes` chain for one request, oldest first. Ordering is carried by the records and
/// never inferred from a filename or a clock (DT-R06).
pub(super) fn answer_chain<'a>(
    store: &'a Store,
    request_id: &str,
    defects: &mut Vec<Defect>,
) -> Vec<&'a Answer> {
    if record_count(store) > MAX_RECORDS {
        defects.push(defect(Some(request_id), DefectCode::LimitExceeded,
            format!("store exceeds {MAX_RECORDS} records")));
        return Vec::new();
    }
    let duplicates = duplicate_ids(store);
    let answers = store.answers_to(request_id);
    if duplicates.contains(request_id) || answers.iter().any(|answer| duplicates.contains(answer.id.as_str())) {
        defects.push(defect(Some(request_id), DefectCode::DuplicateId,
            "answer history contains an ambiguous record identifier"));
        return Vec::new();
    }
    ordered_answer_chain(request_id, answers, defects)
}

fn ordered_answer_chain<'a>(
    request_id: &str,
    mut answers: Vec<&'a Answer>,
    defects: &mut Vec<Defect>,
) -> Vec<&'a Answer> {
    if answers.is_empty() {
        return Vec::new();
    }
    // Sort diagnostics, never choose a winner: a valid chain has exactly one root
    // and one successor per predecessor, otherwise there is no usable history.
    answers.sort_by_key(|answer| &answer.id);
    let ids: BTreeSet<_> = answers.iter().map(|answer| answer.id.as_str()).collect();
    let roots: Vec<_> = answers.iter().copied().filter(|answer| answer.supersedes.is_none()).collect();
    let mut successors: BTreeMap<&str, Vec<&Answer>> = BTreeMap::new();
    let mut invalid = false;
    for answer in &answers {
        if let Some(target) = &answer.supersedes {
            if !ids.contains(target.as_str()) {
                defects.push(defect(Some(&answer.id), DefectCode::DanglingSupersession,
                    format!("`supersedes: {target}` names no prior answer to this request")));
                invalid = true;
            }
            successors.entry(target).or_default().push(answer);
        }
    }
    if ids.len() != answers.len() {
        defects.push(defect(Some(request_id), DefectCode::DuplicateId,
            "answer chain contains duplicate record identifiers"));
        invalid = true;
    }
    if roots.len() != 1 {
        defects.push(defect(Some(request_id), DefectCode::ForkedSupersession,
            format!("{} answers name no predecessor; the chain requires exactly one root", roots.len())));
        invalid = true;
    }
    for (target, next) in &successors {
        if next.len() > 1 {
            defects.push(defect(Some(target), DefectCode::ForkedSupersession,
                format!("{} answers supersede `{target}` ({}); which is later is undefined",
                    next.len(), next.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", "))));
            invalid = true;
        }
    }
    if invalid {
        return Vec::new();
    }
    let mut chain = Vec::with_capacity(answers.len());
    let mut seen = BTreeSet::new();
    let mut current = roots[0];
    loop {
        if !seen.insert(current.id.as_str()) {
            defects.push(defect(Some(request_id), DefectCode::ForkedSupersession,
                "supersession chain is cyclic"));
            return Vec::new();
        }
        chain.push(current);
        let Some(next) = successors.get(current.id.as_str()) else {
            break;
        };
        current = next[0];
    }
    if chain.len() != answers.len() {
        defects.push(defect(Some(request_id), DefectCode::ForkedSupersession,
            "answer chain contains disconnected records or a cycle"));
        return Vec::new();
    }
    chain
}

/// Computes every decision's state, the defects the states cannot represent, and which decisions
/// were revived. `depth` selects a prefix of every answer chain, which is how revival is replayed
/// without appealing to a clock: prefix `k` takes each chain's `k`th answer, or its last if the
/// chain is shorter.
pub(super) fn evaluate_at(
    store: &Store,
    chains: &BTreeMap<&str, Vec<&Answer>>,
    depth: usize,
    invalid: &BTreeSet<&str>,
) -> Fold {
    let mut answers: AnswerSet = BTreeMap::new();
    for (request_id, chain) in chains {
        if chain.is_empty() || depth == 0 {
            continue;
        }
        let index = depth.min(chain.len()) - 1;
        answers.insert(request_id, chain[index]);
    }
    let (resolutions, defects) = Evaluator::new(store, answers, invalid).run();
    Fold {
        resolutions,
        defects,
        revived: BTreeSet::new(),
    }
}

pub fn fold(store: &Store) -> Fold {
    if record_count(store) > MAX_RECORDS {
        return Fold {
            // A single store-wide defect bounds output allocation too. resolution()
            // returns Undecidable for every id when the resolution map is empty.
            resolutions: BTreeMap::new(),
            defects: vec![defect(None, DefectCode::LimitExceeded,
                format!("store exceeds {MAX_RECORDS} records"))],
            revived: BTreeSet::new(),
        };
    }
    let duplicates = duplicate_ids(store);
    let mut invalid = BTreeSet::new();
    let mut chain_defects: Vec<Defect> = duplicates.iter().map(|id| {
        defect(Some(id), DefectCode::DuplicateId, "identifier is carried by multiple records")
    }).collect();
    let requests: BTreeSet<_> = store.requests.iter().map(|r| r.id.as_str()).collect();
    let mut by_request: BTreeMap<&str, Vec<&Answer>> = BTreeMap::new();
    for answer in &store.answers {
        by_request.entry(&answer.answers).or_default().push(answer);
        if duplicates.contains(answer.id.as_str()) {
            invalid.insert(answer.answers.as_str());
        }
    }
    let mut chains: BTreeMap<&str, Vec<&Answer>> = BTreeMap::new();
    for request_id in requests {
        if duplicates.contains(request_id) || invalid.contains(request_id) {
            invalid.insert(request_id);
            chain_defects.push(defect(Some(request_id), DefectCode::DuplicateId,
                "request or its answer history has an ambiguous record identifier"));
            continue;
        }
        let answers = by_request.remove(request_id).unwrap_or_default();
        let has_answers = !answers.is_empty();
        let chain = ordered_answer_chain(request_id, answers, &mut chain_defects);
        if has_answers && chain.is_empty() {
            invalid.insert(request_id);
            chain_defects.push(defect(Some(request_id), DefectCode::ForkedSupersession,
                "answer history is not a single complete supersession chain"));
        }
        chains.insert(request_id, chain);
    }
    let longest = chains.values().map(Vec::len).max().unwrap_or(0);
    let mut current = evaluate_at(store, &chains, longest.max(1), &invalid);

    // Revival needs the answer HISTORY, not the current answer set: a decision is revived when it
    // is live now and some earlier prefix left it moot (DT-R17).
    if longest > 1 {
        let mut ever_moot: BTreeSet<String> = BTreeSet::new();
        for depth in 1..longest {
            let earlier = evaluate_at(store, &chains, depth, &invalid);
            for (id, resolution) in &earlier.resolutions {
                if *resolution == Resolution::State(State::Moot) {
                    ever_moot.insert(id.clone());
                }
            }
        }
        for (id, resolution) in &current.resolutions {
            let live = matches!(
                resolution,
                Resolution::State(State::Pending) | Resolution::State(State::Gated)
            );
            if live && ever_moot.contains(id) {
                current.revived.insert(id.clone());
            }
        }
    }

    current.defects.extend(chain_defects);
    current.defects.extend(store.parse_defects.iter().cloned());
    current.defects.extend(duplicate_handle_defects(store));
    current.defects.extend(orphan_defects(store));
    current.defects.extend(assumption_chain_defects(store));
    current.defects.extend(answer_choice_defects(store, &duplicates));
    current.defects.sort();
    current.defects.dedup();
    current
}

fn record_count(store: &Store) -> usize {
    [store.requests.len(), store.answers.len(), store.assumptions.len(),
        store.promotions.len(), store.parse_defects.len()]
        .into_iter().fold(0usize, usize::saturating_add)
}

fn duplicate_ids(store: &Store) -> BTreeSet<&str> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for id in store.requests.iter().map(|r| r.id.as_str())
        .chain(store.answers.iter().map(|r| r.id.as_str()))
        .chain(store.assumptions.iter().map(|r| r.id.as_str()))
        .chain(store.promotions.iter().map(|r| r.id.as_str()))
    {
        *counts.entry(id).or_default() += 1;
    }
    counts.into_iter().filter_map(|(id, count)| (count > 1).then_some(id)).collect()
}

fn answer_choice_defects(store: &Store, duplicates: &BTreeSet<&str>) -> Vec<Defect> {
    let options: BTreeMap<_, Vec<_>> = store.requests.iter()
        .filter(|r| !duplicates.contains(r.id.as_str()))
        .map(|r| (r.id.as_str(), parse_options(&r.body).into_iter().map(|o| o.key).collect()))
        .collect();
    let mut defects = Vec::new();
    for answer in &store.answers {
        if let Some(keys) = options.get(answer.answers.as_str()) {
            for choice in &answer.choice {
                if !keys.contains(choice) {
                    defects.push(defect(Some(&answer.id), DefectCode::UnknownOption,
                        format!("answer selects `{choice}`, which request `{}` does not offer", answer.answers)));
                }
            }
        }
    }
    defects
}

pub(super) fn duplicate_handle_defects(store: &Store) -> Vec<Defect> {
    let mut by_handle: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
    for request in &store.requests {
        by_handle.entry(request.q).or_default().push(&request.id);
    }
    by_handle
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(handle, mut ids)| {
            ids.sort_unstable();
            defect(
                Some(ids[0]),
                DefectCode::DuplicateHandle,
                format!(
                    "{} is carried by {} requests ({}); allocation raced and is never \
                     auto-resolved (DT-R37)",
                    crate::handle(handle),
                    ids.len(),
                    ids.join(", ")
                ),
            )
        })
        .collect()
}

pub(super) fn orphan_defects(store: &Store) -> Vec<Defect> {
    let known: BTreeSet<&str> = store.requests.iter().map(|r| r.id.as_str()).collect();
    let mut defects = Vec::new();
    let mut check = |id: &str, field: &str, target: &str| {
        if !known.contains(target) {
            defects.push(defect(
                Some(id),
                DefectCode::OrphanRecord,
                format!("`{field}: {target}` names no request in this tree"),
            ));
        }
    };
    for answer in &store.answers {
        check(&answer.id, "answers", &answer.answers);
    }
    for assumption in &store.assumptions {
        check(&assumption.id, "assumes", &assumption.assumes);
    }
    for promotion in &store.promotions {
        check(&promotion.id, "promotes", &promotion.promotes);
    }
    defects
}

pub(super) fn assumption_chain_defects(store: &Store) -> Vec<Defect> {
    let mut defects = Vec::new();
    let mut by_request: BTreeMap<&str, Vec<&Assumption>> = BTreeMap::new();
    for assumption in &store.assumptions {
        by_request.entry(&assumption.assumes).or_default().push(assumption);
    }
    for assumptions in by_request.values() {
        let known: BTreeSet<_> = assumptions.iter().map(|a| a.id.as_str()).collect();
        let mut predecessors: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for assumption in assumptions {
            if let Some(target) = &assumption.supersedes {
                if !known.contains(target.as_str()) {
                    defects.push(defect(Some(&assumption.id), DefectCode::DanglingSupersession,
                        format!("`supersedes: {target}` names no prior assumption to this request")));
                }
                predecessors.entry(target.as_str()).or_default().push(&assumption.id);
            }
        }
        for (target, mut ids) in predecessors {
            if ids.len() > 1 {
                ids.sort_unstable();
                defects.push(defect(Some(target), DefectCode::ForkedSupersession,
                    format!("{} assumptions supersede `{target}` ({})", ids.len(), ids.join(", "))));
            }
        }
    }
    defects
}

/// The current answer to a request: the one no other answer supersedes.
pub fn current_answer<'a>(store: &'a Store, request_id: &str) -> Option<&'a Answer> {
    let mut ignored = Vec::new();
    answer_chain(store, request_id, &mut ignored)
        .last()
        .copied()
}

pub fn answer_history<'a>(store: &'a Store, request_id: &str) -> Vec<&'a Answer> {
    let mut ignored = Vec::new();
    answer_chain(store, request_id, &mut ignored)
}

pub fn assumptions_for<'a>(store: &'a Store, request_id: &str) -> Vec<&'a Assumption> {
    store.assumptions_to(request_id)
}

pub fn promotions_for<'a>(store: &'a Store, request_id: &str) -> Vec<&'a Promotion> {
    store.promotions_to(request_id)
}

