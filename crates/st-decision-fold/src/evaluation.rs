use super::parsing::*;
use super::*;
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
    store: &'a Store,
    answers: AnswerSet<'a>,
    resolutions: BTreeMap<String, Resolution>,
    defects: Vec<Defect>,
    /// Option keys per request, parsed once — the guard checks `unknown-option` against them.
    options: BTreeMap<&'a str, Vec<String>>,
}

impl<'a> Evaluator<'a> {
    fn new(store: &'a Store, answers: AnswerSet<'a>) -> Self {
        let options = store
            .requests
            .iter()
            .map(|request| {
                (
                    request.id.as_str(),
                    parse_options(&request.body)
                        .into_iter()
                        .map(|option| option.key)
                        .collect(),
                )
            })
            .collect();
        Self {
            store,
            answers,
            resolutions: BTreeMap::new(),
            defects: Vec::new(),
            options,
        }
    }

    fn report(&mut self, about: &str, code: DefectCode, detail: impl Into<String>) {
        let candidate = defect(Some(about), code, detail);
        if !self.defects.contains(&candidate) {
            self.defects.push(candidate);
        }
    }

    fn term_truth(&mut self, owner: &str, term: &Term, visiting: &mut Vec<String>) -> Truth {
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
        let Some(referenced) = self.store.request(&term.decision) else {
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
        match self.resolve(&term.decision.clone(), visiting) {
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

    fn resolve(&mut self, id: &str, visiting: &mut Vec<String>) -> Resolution {
        if let Some(cached) = self.resolutions.get(id) {
            return *cached;
        }
        if visiting.iter().any(|seen| seen == id) {
            let mut cycle = visiting.clone();
            cycle.push(id.to_string());
            self.report(
                id,
                DefectCode::CyclicReference,
                format!("guard cycle through `{}`", cycle.join(" -> ")),
            );
            return Resolution::Undecidable;
        }
        let Some(request) = self.store.request(id) else {
            return Resolution::Undecidable;
        };
        let terms = request.applies_when.clone();
        let answered = self.answers.contains_key(id);

        let resolution = if terms.is_empty() {
            Resolution::State(if answered {
                State::Answered
            } else {
                State::Pending
            })
        } else {
            visiting.push(id.to_string());
            let truths: Vec<Truth> = terms
                .iter()
                .map(|term| self.term_truth(id, term, visiting))
                .collect();
            visiting.pop();
            match conjoin(&truths) {
                Truth::Invalid => Resolution::Undecidable,
                // Moot outranks answered: a decision must never read as live under a branch its
                // own guard says is dead, and that invariant is what the guard-form eval turned
                // on. The answer itself is never destroyed — `read` still shows it.
                Truth::False => Resolution::State(State::Moot),
                Truth::Undetermined => Resolution::State(State::Gated),
                Truth::True => Resolution::State(if answered {
                    State::Answered
                } else {
                    State::Pending
                }),
            }
        };
        self.resolutions.insert(id.to_string(), resolution);
        resolution
    }

    fn run(mut self) -> (BTreeMap<String, Resolution>, Vec<Defect>) {
        let ids: Vec<String> = self
            .store
            .requests
            .iter()
            .map(|request| request.id.clone())
            .collect();
        for id in ids {
            self.resolve(&id, &mut Vec::new());
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
    let answers = store.answers_to(request_id);
    if answers.is_empty() {
        return Vec::new();
    }
    let ids: BTreeSet<&str> = answers.iter().map(|a| a.id.as_str()).collect();
    let roots: Vec<&&Answer> = answers.iter().filter(|a| a.supersedes.is_none()).collect();

    for answer in &answers {
        if let Some(target) = &answer.supersedes
            && !ids.contains(target.as_str())
        {
            defects.push(defect(
                Some(&answer.id),
                DefectCode::DanglingSupersession,
                format!("`supersedes: {target}` names no prior answer to this request"),
            ));
        }
    }
    if roots.len() > 1 {
        defects.push(defect(
            Some(request_id),
            DefectCode::ForkedSupersession,
            format!(
                "{} answers name no predecessor ({}), so the chain has no single starting point",
                roots.len(),
                roots
                    .iter()
                    .map(|a| a.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let mut chain: Vec<&Answer> = Vec::new();
    let Some(root) = roots.first() else {
        defects.push(defect(
            Some(request_id),
            DefectCode::ForkedSupersession,
            "every answer names a predecessor, so the chain has no starting point".to_string(),
        ));
        return chain;
    };
    let mut current: &Answer = root;
    loop {
        chain.push(current);
        let successors: Vec<&&Answer> = answers
            .iter()
            .filter(|a| a.supersedes.as_deref() == Some(current.id.as_str()))
            .collect();
        match successors.as_slice() {
            [] => break,
            [next] => {
                if chain.iter().any(|seen| seen.id == next.id) {
                    defects.push(defect(
                        Some(&next.id),
                        DefectCode::ForkedSupersession,
                        "supersession chain is cyclic".to_string(),
                    ));
                    break;
                }
                current = next;
            }
            many => {
                defects.push(defect(
                    Some(&current.id),
                    DefectCode::ForkedSupersession,
                    format!(
                        "{} answers supersede `{}` ({}); which is later is undefined",
                        many.len(),
                        current.id,
                        many.iter()
                            .map(|a| a.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
                break;
            }
        }
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
) -> Fold {
    let mut answers: AnswerSet = BTreeMap::new();
    for (request_id, chain) in chains {
        if chain.is_empty() || depth == 0 {
            continue;
        }
        let index = depth.min(chain.len()) - 1;
        answers.insert(request_id, chain[index]);
    }
    let (resolutions, defects) = Evaluator::new(store, answers).run();
    Fold {
        resolutions,
        defects,
        revived: BTreeSet::new(),
    }
}

pub fn fold(store: &Store) -> Fold {
    let mut chain_defects: Vec<Defect> = Vec::new();
    let mut chains: BTreeMap<&str, Vec<&Answer>> = BTreeMap::new();
    for request in &store.requests {
        let chain = answer_chain(store, &request.id, &mut chain_defects);
        chains.insert(request.id.as_str(), chain);
    }
    let longest = chains.values().map(Vec::len).max().unwrap_or(0);
    let mut current = evaluate_at(store, &chains, longest.max(1));

    // Revival needs the answer HISTORY, not the current answer set: a decision is revived when it
    // is live now and some earlier prefix left it moot (DT-R17).
    if longest > 1 {
        let mut ever_moot: BTreeSet<String> = BTreeSet::new();
        for depth in 1..longest {
            let earlier = evaluate_at(store, &chains, depth);
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
    current.defects.sort();
    current.defects.dedup();
    current
}

pub(super) fn duplicate_handle_defects(store: &Store) -> Vec<Defect> {
    let mut by_handle: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
    for request in &store.requests {
        by_handle.entry(request.q).or_default().push(&request.id);
    }
    by_handle
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(handle, ids)| {
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
    for request in &store.requests {
        let assumptions = store.assumptions_to(&request.id);
        if assumptions.len() < 2 {
            continue;
        }
        let mut predecessors: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for assumption in &assumptions {
            if let Some(target) = &assumption.supersedes {
                predecessors
                    .entry(target.as_str())
                    .or_default()
                    .push(&assumption.id);
            }
        }
        for (target, ids) in predecessors {
            if ids.len() > 1 {
                defects.push(defect(
                    Some(target),
                    DefectCode::ForkedSupersession,
                    format!(
                        "{} assumptions supersede `{target}` ({})",
                        ids.len(),
                        ids.join(", ")
                    ),
                ));
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

