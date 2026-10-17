//! Private U0 source contract, with no production producer or activation.
//!
//! These checks compare owned, keyed evidence; they do not collect it, maintain mutation
//! tokens, certify a producer, or make arbitrary SQL/callbacks short. In particular the source
//! must account for negative predicates, same-index changes, raw/autocommit writes, schema,
//! hooks and native authority. No current Store adapter satisfies that complete contract here.
//! Pure decoded content (U1) is separate and does not require mutable publication evidence.
#![allow(dead_code)] // Private source checkpoint; producer/native adapters are not approved.

pub(crate) use super::transaction_finalizer::{
    ResolutionKnowledge, RollbackResolution, ValidatedCommitOutcome as CommitKnowledge,
};
use std::collections::{BTreeMap, BTreeSet};

/// Fixed-size storage-issued, producer-carried non-reused mutation identity. No such marker is
/// installed by this module. A rowid/timestamp, claim index or process-local counter alone is
/// not sufficient. The producer must qualify lifetime/reopen/ABA and every mutation path.
/// Coverage includes logical visibility/generation/mask changes at the same index, not just
/// physical row writes. Inactive staging/hidden reclamation require separate exact predicate
/// domains: neither global invalidation of unrelated logical facts nor skipped invalidation
/// of actual physical consumers is certified here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MutationToken(pub [u8; 32]);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PredicateKey {
    /// A source-owned predicate, including an exact key, range or negative membership scope.
    pub predicate: String,
    pub key: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceIdentity {
    /// Must change/refuse after restore, copy, source reinstall or unsupported schema change.
    pub lifetime: [u8; 32],
    pub schema: [u8; 32],
    pub rules: [u8; 32],
    pub producer: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Fact {
    Present {
        mutation: MutationToken,
        value: [u8; 32],
    },
    /// The token covers insertion, removal and ABA for the whole predicate, not just rows
    /// currently present. An absent row cannot supply its own mutation history.
    Absent { mutation: MutationToken },
}

impl Fact {
    /// Fallible boundary for an incomplete source observation; accepted facts never contain
    /// an optional token. This does not manufacture a storage-issued token or certify it.
    pub(crate) fn absent(
        key: &PredicateKey,
        mutation: Option<MutationToken>,
    ) -> Result<Self, Refusal> {
        mutation
            .map(|mutation| Self::Absent { mutation })
            .ok_or_else(|| Refusal::MissingMutation(key.clone()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    EmptyContract,
    DuplicatePredicate(PredicateKey),
    MissingPredicate(PredicateKey),
    MissingMutation(PredicateKey),
    SourceChanged,
    Stale(PredicateKey),
}

/// Owned observations for a producer's declared read set. Construction checks completeness
/// relative to that declaration only. A declaration can omit semantic dependencies: native
/// source/algorithm review is still required; a successful comparison is NOT certification.
pub(crate) struct ReadSet {
    identity: SourceIdentity,
    facts: BTreeMap<PredicateKey, Fact>,
}

impl ReadSet {
    pub(crate) fn capture(
        identity: SourceIdentity,
        required: BTreeSet<PredicateKey>,
        observed: impl IntoIterator<Item = (PredicateKey, Fact)>,
    ) -> Result<Self, Refusal> {
        if required.is_empty() {
            return Err(Refusal::EmptyContract);
        }
        let mut facts = BTreeMap::new();
        for (key, fact) in observed {
            if facts.insert(key.clone(), fact).is_some() {
                return Err(Refusal::DuplicatePredicate(key));
            }
        }
        for key in required {
            if !facts.contains_key(&key) {
                return Err(Refusal::MissingPredicate(key));
            }
        }
        Ok(Self { identity, facts })
    }

    /// `current` must be freshly collected on the actual managed transaction, after loan
    /// acquisition and again after finalizers. Comparing two caller-supplied snapshots does
    /// not prove that either is current or that SQL/trigger/callback cost is bounded.
    pub(crate) fn validate(&self, current: &Self) -> Result<(), Refusal> {
        if self.identity != current.identity {
            return Err(Refusal::SourceChanged);
        }
        for (key, expected) in &self.facts {
            let actual = current
                .facts
                .get(key)
                .ok_or_else(|| Refusal::MissingPredicate(key.clone()))?;
            if expected != actual {
                return Err(Refusal::Stale(key.clone()));
            }
        }
        Ok(())
    }
}

/// Own the pure payload and mutable read evidence separately. This is deliberately not a
/// callable SQL closure or a generation/root manifest. No execute/legacy-fallback API exists.
pub(crate) struct PreparedPublication<P> {
    pub payload: P,
    pub reads: ReadSet,
}

/// Open architecture findings, never completed by supplying a matching token/boolean. The
/// private checkpoint installs no production mutation tracker, journal, outbox or return hook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnprovidedContract {
    TransactionalAllMutatorEvidence,
    CompletePredicateMappingAndPerWriteCost,
    DurableCommandOutcomeRecovery,
    PerEffectReopenRecovery,
    BoundedNativeAuthority,
    ExecutorPhysicalReturnReceipt,
}

impl<P> PreparedPublication<P> {
    pub(crate) fn certification(&self) -> Result<(), UnprovidedContract> {
        // F1 is deliberately fail-closed. No source adapter or unsupported-interval fence
        // was installed; a matching declared read set is not a production certificate.
        Err(UnprovidedContract::TransactionalAllMutatorEvidence)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionGap {
    NoCommittedAuthorityReceipt,
    /// No executor-owned committed-authority completion receipt is wired here.
    AuthorityUnobserved,
}

impl CommitKnowledge {
    /// A successful COMMIT alone can NEVER create a success acknowledgement through this
    /// private API. Native observer/actual queue-return receipts remain unsupported rather
    /// than manufactured by model transitions or a cloned boolean.
    pub(crate) fn acknowledgement(&self) -> Result<(), CompletionGap> {
        match self {
            Self::Committed => Err(CompletionGap::AuthorityUnobserved),
            _ => Err(CompletionGap::NoCommittedAuthorityReceipt),
        }
    }
}

/// Separate unresolved obligations; neither guard drop nor a completed commit result creates
/// evidence of actual queue reacquisition/dispatch readiness. Future integration must issue
/// native receipts at the executor, preserve detached work, and review post-return cleanup.
pub(crate) struct UncertifiedCompletion {
    pub commit: CommitKnowledge,
    pub missing: BTreeSet<NativeObligation>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum NativeObligation {
    SynchronousAuthority,
    PhysicalQueueReturn,
    RealChildAndBackingCompletion,
}

impl UncertifiedCompletion {
    pub(crate) fn after_validated_commit(commit: CommitKnowledge) -> Self {
        Self {
            commit,
            missing: BTreeSet::from([
                NativeObligation::SynchronousAuthority,
                NativeObligation::PhysicalQueueReturn,
                NativeObligation::RealChildAndBackingCompletion,
            ]),
        }
    }
}

#[cfg(test)]
mod tests;
