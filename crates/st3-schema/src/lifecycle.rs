//! Member lifecycle and container capability policy, declared beside the schema registry.
//!
//! Each entry names the claim kinds that are authoritative for a subject family's visibility as an
//! ordered-membership member, and whether the family may contain ordered memberships. Visibility
//! follows shared declarations and retirement, never runtime, PTY, reachability or observation
//! recency. A family without an entry cannot be a member. The store derives its projections and
//! invalidation from these entries; this module holds no storage logic.
use super::{ValidationError, error};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::sync::LazyLock;

/// How a member family's authoritative claims decide whether a member is visible and counted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VisibilityPolicy {
    /// Visible while the latest `intent.desired` declaration is not retired, whether it is
    /// running, stopped/offline or suspended. Retirement hides; redeclaring the same stable ID
    /// restores the retained membership. Observations never declare a subject.
    DesiredDeclaration,
    /// Visible while the selected `mission.published` definition is not retired.
    MissionDeclaration,
    /// Visible while the arrangement exists and is not retired.
    Arrangement,
    /// Visible while the latest glass claim is an upsert, not a deletion.
    Glass,
    /// Visible once the immutable document lineage has a `doc.bound` claim.
    Document,
    /// Visible once the subject has any retained claim. Only for families with no retirement.
    RetainedClaim,
}

/// The claims a visibility policy reads. Changes to them invalidate dependent memberships.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "type", content = "kinds")]
pub enum Sources {
    /// Only these claim kinds on the member subject.
    Kinds(&'static [&'static str]),
    /// Every retained claim on the member subject.
    AnyClaim,
}

impl VisibilityPolicy {
    pub const fn sources(self) -> Sources {
        match self {
            Self::DesiredDeclaration => Sources::Kinds(&["intent.desired"]),
            Self::MissionDeclaration => Sources::Kinds(&["mission.published"]),
            Self::Arrangement => Sources::Kinds(&["arrangement.edited"]),
            Self::Glass => Sources::Kinds(&["glass.upserted", "glass.deleted"]),
            Self::Document => Sources::Kinds(&["doc.bound"]),
            Self::RetainedClaim => Sources::AnyClaim,
        }
    }
}

/// How a container resolves a raw membership bucket into the bucket it is listed under.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectiveBucket {
    /// Buckets are lowercase UUIDv7 folder IDs of a folder-only arrangement at `layout_version`.
    /// A deleted folder lifts to its nearest live ancestor, or the root; the root and a missing
    /// folder resolve to the root bucket.
    ArrangementFolders { layout_version: u8 },
}

/// A family's capability to hold ordered memberships.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContainerCapability {
    /// The append claim on the container that edits its memberships.
    pub claim: &'static str,
    pub effective_bucket: EffectiveBucket,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Entry {
    pub family: &'static str,
    pub visibility: VisibilityPolicy,
    /// Derived from `visibility`; serialized so the schema digest covers policy changes.
    pub sources: Sources,
    pub ordered_membership: Option<ContainerCapability>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct Registry {
    entries: Vec<Entry>,
}

pub const MEMBERSHIP_CLAIM: &str = "ordered-membership.edited";
pub const ARRANGEMENT_MEMBERSHIP_LAYOUT_VERSION: u8 = 2;

impl Registry {
    /// Every entry, ordered by family.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &Entry> + '_ {
        self.entries.iter()
    }

    pub fn family(&self, family: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|entry| entry.family.cmp(family))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// The entry for a subject that passes schema subject validation, if its family has one.
    pub fn subject(&self, subject: &str) -> Option<&Entry> {
        self.member(subject).ok()
    }

    /// The entry for a validated member subject, or why it cannot be an ordered member.
    pub fn member(&self, subject: &str) -> Result<&Entry, ValidationError> {
        let spec = super::registry().validate_subject(subject)?;
        self.family(&spec.family).ok_or_else(|| {
            error(
                "unsupported-membership-member",
                format!("`{}` subjects declare no lifecycle policy and cannot be ordered members", spec.family),
            )
        })
    }

    /// The container capability of a validated container subject.
    pub fn container(&self, subject: &str) -> Result<(&Entry, ContainerCapability), ValidationError> {
        let entry = self.member(subject)?;
        let capability = entry.ordered_membership.ok_or_else(|| {
            error(
                "unsupported-membership-container",
                format!("`{}` subjects do not declare ordered memberships", entry.family),
            )
        })?;
        Ok((entry, capability))
    }

    pub fn container_families(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.iter().filter(|entry| entry.ordered_membership.is_some()).map(|entry| entry.family)
    }

    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("the lifecycle registry is serializable");
        hex::encode(Sha256::digest(bytes))
    }
}

const fn entry(family: &'static str, visibility: VisibilityPolicy) -> Entry {
    Entry { family, visibility, sources: visibility.sources(), ordered_membership: None }
}

fn build_registry() -> Registry {
    use VisibilityPolicy::{Arrangement, DesiredDeclaration, Document, Glass, MissionDeclaration, RetainedClaim};
    let mut entries = vec![
        entry("account", DesiredDeclaration),
        entry("agent", DesiredDeclaration),
        Entry {
            ordered_membership: Some(ContainerCapability {
                claim: MEMBERSHIP_CLAIM,
                effective_bucket: EffectiveBucket::ArrangementFolders {
                    layout_version: ARRANGEMENT_MEMBERSHIP_LAYOUT_VERSION,
                },
            }),
            ..entry("arrangement", Arrangement)
        },
        entry("attention", RetainedClaim),
        entry("doc", Document),
        entry("exec", DesiredDeclaration),
        entry("external", RetainedClaim),
        entry("glass", Glass),
        entry("host", DesiredDeclaration),
        entry("lane", DesiredDeclaration),
        entry("message", RetainedClaim),
        entry("mission", MissionDeclaration),
        entry("mission-run", RetainedClaim),
        entry("observer", DesiredDeclaration),
        entry("person", RetainedClaim),
        entry("planning-session", RetainedClaim),
        entry("resource", DesiredDeclaration),
        entry("schedule", DesiredDeclaration),
        entry("subscription", DesiredDeclaration),
    ];
    entries.sort_by_key(|entry| entry.family);
    Registry { entries }
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(build_registry);

pub fn registry() -> &'static Registry {
    &REGISTRY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_unique_registered_families_with_registered_sources() {
        let schema = super::super::registry();
        let families = registry().entries().map(|entry| entry.family).collect::<Vec<_>>();
        assert_eq!(
            families,
            [
                "account", "agent", "arrangement", "attention", "doc", "exec", "external", "glass", "host", "lane",
                "message", "mission", "mission-run", "observer", "person", "planning-session", "resource", "schedule",
                "subscription",
            ]
        );
        for entry in registry().entries() {
            assert!(schema.subjects.contains_key(entry.family), "{} is not a subject family", entry.family);
            assert_eq!(entry.sources, entry.visibility.sources());
            if let Sources::Kinds(kinds) = entry.sources {
                for kind in kinds {
                    assert!(schema.claims.contains_key(*kind), "{kind} is not a registered claim");
                }
            }
            assert_eq!(registry().family(entry.family), Some(entry));
        }
    }

    #[test]
    fn lookups_validate_subjects_and_exclude_transient_runtimes() {
        let registry = registry();
        assert_eq!(registry.subject("agent/fleet/seat").unwrap().visibility, VisibilityPolicy::DesiredDeclaration);
        assert_eq!(registry.subject("resource/repo").unwrap().visibility, VisibilityPolicy::DesiredDeclaration);
        assert_eq!(registry.subject("message/m1").unwrap().visibility, VisibilityPolicy::RetainedClaim);
        assert_eq!(registry.subject("mission/m1").unwrap().visibility, VisibilityPolicy::MissionDeclaration);
        assert_eq!(registry.subject("doc/readme").unwrap().visibility, VisibilityPolicy::Document);
        for transient in ["pty/run/one", "daemon/node", "checkpoint/2026-01-01"] {
            assert_eq!(registry.member(transient).unwrap_err().code, "unsupported-membership-member");
            assert!(registry.subject(transient).is_none());
        }
        assert!(registry.subject("session/abc").is_none());
        assert!(registry.subject("agent/").is_none());
        assert!(registry.family("pty").is_none());
        let (entry, capability) =
            registry.container("arrangement/person/ada/019a0000-0000-7000-8000-000000000001").unwrap();
        assert_eq!(entry.family, "arrangement");
        assert_eq!(capability.claim, MEMBERSHIP_CLAIM);
        assert_eq!(capability.effective_bucket, EffectiveBucket::ArrangementFolders { layout_version: 2 });
        assert_eq!(registry.container("agent/fleet/seat").unwrap_err().code, "unsupported-membership-container");
        assert_eq!(registry.container_families().collect::<Vec<_>>(), ["arrangement"]);
        assert_eq!(super::super::registry().claims[MEMBERSHIP_CLAIM].subjects, ["arrangement"]);
        assert_eq!(registry.digest().len(), 64);
    }
}
