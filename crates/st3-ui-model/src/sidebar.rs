//! Folder-only sidebar projection and intent validation, without rendering or wire writes.
//!
//! Resolved maps are authoritative; inventory adds known subjects but never hides placements.
//! Revisions remain opaque. Ordering is ordinal, with separate folder/placed/unplaced domains.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use st3_client::Arrangement;

/// A stable folder or subject identity; prefixes keep their key domains disjoint.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum NodeRef {
    Folder { id: String },
    Subject { subject: String },
}

/// Subjects are accepted in intents only so the planner can explicitly refuse them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum Parent {
    Root {},
    Folder { id: String },
    Subject { subject: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Person,
    Fallback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedNode {
    pub node: NodeRef,
    /// The raw register parent, even when resolved placement lifts it elsewhere.
    pub parent: Parent,
    /// The authoritative resolved parent used for display and ancestry.
    pub shown_under: Parent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Registers are person-owned, including lifted ones; inventory-only nodes are fallback.
    pub source: Source,
    pub children: Vec<NodeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidebarTree {
    pub roots: Vec<NodeRef>,
    pub nodes: BTreeMap<String, ProjectedNode>,
    pub arrangement_revision: String,
    pub deleted_folders: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum ProjectionError {
    NotReady,
    ArrangementUnavailable,
    Unsupported { required: u8, advertised: u8 },
    InvalidResolution,
}

/// Stable, injective keys. Identity strings are not parsed or normalized.
pub fn node_key(node: &NodeRef) -> String {
    match node {
        NodeRef::Folder { id } => format!("folder:{id}"),
        NodeRef::Subject { subject } => format!("subject:{subject}"),
    }
}

fn folder_parent(id: &Option<String>) -> Parent {
    match id {
        Some(id) => Parent::Folder { id: id.clone() },
        None => Parent::Root {},
    }
}

/// Build only from a live v1 arrangement with complete, acyclic resolved maps.
/// Tombstones are omitted, and every placement survives regardless of inventory membership.
pub fn build(
    arrangement: &Arrangement,
    inventory: &[String],
) -> Result<SidebarTree, ProjectionError> {
    if arrangement.deleted {
        return Err(ProjectionError::ArrangementUnavailable);
    }
    if arrangement.body.version != 1 {
        return Err(ProjectionError::Unsupported {
            required: 1,
            advertised: arrangement.body.version,
        });
    }
    let resolved = arrangement
        .resolved
        .as_ref()
        .ok_or(ProjectionError::NotReady)?;
    let mut tree = SidebarTree {
        roots: Vec::new(),
        nodes: BTreeMap::new(),
        arrangement_revision: arrangement.header.revision.clone(),
        deleted_folders: Vec::new(),
    };
    for (id, folder) in &arrangement.body.folders {
        if folder.tombstone.is_some() {
            tree.deleted_folders.push(id.clone());
            continue;
        }
        let shown_under = resolved.parents.get(id).ok_or(ProjectionError::NotReady)?;
        let node = NodeRef::Folder { id: id.clone() };
        tree.nodes.insert(
            node_key(&node),
            ProjectedNode {
                node,
                parent: folder_parent(&folder.position.value.parent),
                shown_under: folder_parent(shown_under),
                key: Some(folder.position.value.key.clone()),
                revision: Some(folder.position.revision.clone()),
                source: Source::Person,
                children: Vec::new(),
            },
        );
    }
    for (subject, register) in &arrangement.body.placements {
        let shown_under = resolved
            .folders
            .get(subject)
            .ok_or(ProjectionError::NotReady)?;
        let node = NodeRef::Subject {
            subject: subject.clone(),
        };
        tree.nodes.insert(
            node_key(&node),
            ProjectedNode {
                node,
                parent: folder_parent(&register.value.folder),
                shown_under: folder_parent(shown_under),
                key: Some(register.value.key.clone()),
                revision: Some(register.revision.clone()),
                source: Source::Person,
                children: Vec::new(),
            },
        );
    }
    for subject in inventory {
        tree.nodes
            .entry(format!("subject:{subject}"))
            .or_insert_with(|| ProjectedNode {
                node: NodeRef::Subject {
                    subject: subject.clone(),
                },
                parent: Parent::Root {},
                shown_under: Parent::Root {},
                key: None,
                revision: None,
                source: Source::Fallback,
                children: Vec::new(),
            });
    }
    // Walk each ancestry once. A visiting state detects cycles without recursive stack growth.
    let mut states = BTreeMap::<&str, bool>::new();
    let mut path = Vec::new();
    for key in tree.nodes.keys() {
        path.clear();
        let mut cursor = key.as_str();
        loop {
            match states.get(cursor) {
                Some(true) => break,
                Some(false) => return Err(ProjectionError::InvalidResolution),
                None => {}
            }
            states.insert(cursor, false);
            path.push(cursor);
            match &tree.nodes[cursor].shown_under {
                Parent::Root {} => break,
                Parent::Folder { id } => {
                    // Borrow the stored key for the next iteration, avoiding owned ancestry copies.
                    let parent_key = format!("folder:{id}");
                    let Some((key, _)) = tree.nodes.get_key_value(&parent_key) else {
                        return Err(ProjectionError::InvalidResolution);
                    };
                    cursor = key;
                }
                Parent::Subject { .. } => return Err(ProjectionError::InvalidResolution),
            }
        }
        for key in &path {
            states.insert(*key, true);
        }
    }
    let mut ordered: Vec<_> = tree.nodes.values().collect();
    ordered.sort_by(|a, b| {
        fn rank(node: &ProjectedNode) -> u8 {
            match node.node {
                NodeRef::Folder { .. } => 0,
                NodeRef::Subject { .. } if node.key.is_some() => 1,
                NodeRef::Subject { .. } => 2,
            }
        }
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.node.cmp(&b.node))
    });
    let mut groups = BTreeMap::<Parent, Vec<NodeRef>>::new();
    for node in ordered {
        groups
            .entry(node.shown_under.clone())
            .or_default()
            .push(node.node.clone());
    }
    tree.roots = groups.remove(&Parent::Root {}).unwrap_or_default();
    for (parent, children) in groups {
        if let Parent::Folder { id } = parent {
            tree.nodes
                .get_mut(&node_key(&NodeRef::Folder { id }))
                .expect("resolved parents validated above")
                .children = children;
        }
    }
    Ok(tree)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveIntent {
    pub node: NodeRef,
    pub parent: Parent,
    pub before: Option<NodeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleStep {
    pub node: NodeRef,
    pub source: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParentMissingCause {
    Unknown,
    FolderDeleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParentNotAllowedCause {
    SubjectParentUnsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum MoveRefusal {
    NodeMissing {
        node: NodeRef,
    },
    ParentNotAllowed {
        parent: Parent,
        cause: ParentNotAllowedCause,
    },
    #[serde(rename = "Self")]
    SelfMove {
        node: NodeRef,
    },
    ParentMissing {
        parent: Parent,
        cause: ParentMissingCause,
    },
    Cycle {
        node: NodeRef,
        parent: Parent,
        path: Vec<CycleStep>,
    },
    AnchorMoved {
        before: NodeRef,
        parent: Parent,
        #[serde(skip_serializing_if = "Option::is_none")]
        found_under: Option<Parent>,
    },
    NoChange {
        node: NodeRef,
        parent: Parent,
    },
}

fn same_kind(a: &NodeRef, b: &NodeRef) -> bool {
    matches!(
        (a, b),
        (NodeRef::Folder { .. }, NodeRef::Folder { .. })
            | (NodeRef::Subject { .. }, NodeRef::Subject { .. })
    )
}

/// Validate without generating ordering keys or changing the tree.
/// Cycle paths run moved node → proposed parent → ancestors → moved node; each step
/// carries that projected node's own source. No-change requires raw and shown parents to agree.
pub fn plan_move(tree: &SidebarTree, intent: &MoveIntent) -> Result<MoveIntent, MoveRefusal> {
    validate_move(tree, intent)?;
    Ok(intent.clone())
}

fn validate_move(tree: &SidebarTree, intent: &MoveIntent) -> Result<(), MoveRefusal> {
    let Some(node) = tree.nodes.get(&node_key(&intent.node)) else {
        return Err(MoveRefusal::NodeMissing {
            node: intent.node.clone(),
        });
    };
    if matches!(intent.parent, Parent::Subject { .. }) {
        return Err(MoveRefusal::ParentNotAllowed {
            parent: intent.parent.clone(),
            cause: ParentNotAllowedCause::SubjectParentUnsupported,
        });
    }
    if let (NodeRef::Folder { id: node_id }, Parent::Folder { id: parent_id }) =
        (&intent.node, &intent.parent)
        && node_id == parent_id
    {
        return Err(MoveRefusal::SelfMove {
            node: intent.node.clone(),
        });
    }
    let parent = if let Parent::Folder { id } = &intent.parent {
        let Some(parent) = tree.nodes.get(&format!("folder:{id}")) else {
            return Err(MoveRefusal::ParentMissing {
                parent: intent.parent.clone(),
                cause: if tree.deleted_folders.binary_search(id).is_ok() {
                    ParentMissingCause::FolderDeleted
                } else {
                    ParentMissingCause::Unknown
                },
            });
        };
        Some(parent)
    } else {
        None
    };
    if matches!(intent.node, NodeRef::Folder { .. }) {
        let ancestors = || {
            std::iter::successors(parent, |ancestor| match &ancestor.shown_under {
                Parent::Folder { id } => tree.nodes.get(&format!("folder:{id}")),
                _ => None,
            })
        };
        if ancestors().any(|ancestor| ancestor.node == intent.node) {
            // Materialize diagnostic identities only for an actual cycle refusal.
            let mut path = vec![CycleStep {
                node: node.node.clone(),
                source: node.source,
            }];
            for ancestor in ancestors() {
                path.push(CycleStep {
                    node: ancestor.node.clone(),
                    source: ancestor.source,
                });
                if ancestor.node == intent.node {
                    break;
                }
            }
            return Err(MoveRefusal::Cycle {
                node: intent.node.clone(),
                parent: intent.parent.clone(),
                path,
            });
        }
    }
    if let Some(before) = &intent.before {
        let anchor = tree.nodes.get(&node_key(before));
        if before == &intent.node
            || !same_kind(before, &intent.node)
            || anchor.is_none_or(|anchor| anchor.shown_under != intent.parent)
        {
            return Err(MoveRefusal::AnchorMoved {
                before: before.clone(),
                parent: intent.parent.clone(),
                found_under: anchor.map(|anchor| anchor.shown_under.clone()),
            });
        }
    }
    if node.parent == intent.parent && node.shown_under == intent.parent {
        let siblings = parent.map_or(tree.roots.as_slice(), |parent| parent.children.as_slice());
        let mut following = siblings
            .iter()
            .skip_while(|sibling| *sibling != &intent.node)
            .skip(1);
        let next = following.find(|sibling| same_kind(sibling, &intent.node));
        if next == intent.before.as_ref() {
            return Err(MoveRefusal::NoChange {
                node: intent.node.clone(),
                parent: intent.parent.clone(),
            });
        }
    }
    Ok(())
}

/// The same checks, precedence, and no-change semantics as [`plan_move`].
pub fn can_place(tree: &SidebarTree, intent: &MoveIntent) -> Result<(), MoveRefusal> {
    validate_move(tree, intent)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollupDisplay {
    Always,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rollup {
    pub needs_you: usize,
    pub display: RollupDisplay,
}

/// Count each marked descendant subject once in every containing folder. Subjects have no entry.
pub fn rollup(tree: &SidebarTree, needs_you: &BTreeSet<String>) -> BTreeMap<String, Rollup> {
    let mut result: BTreeMap<_, _> = tree
        .nodes
        .iter()
        .filter(|(_, node)| matches!(node.node, NodeRef::Folder { .. }))
        .map(|(key, _)| {
            (
                key.clone(),
                Rollup {
                    needs_you: 0,
                    display: RollupDisplay::Always,
                },
            )
        })
        .collect();
    for subject in needs_you {
        let Some(node) = tree.nodes.get(&format!("subject:{subject}")) else {
            continue;
        };
        let mut parent = &node.shown_under;
        while let Parent::Folder { id } = parent {
            let key = format!("folder:{id}");
            result
                .get_mut(&key)
                .expect("folder from built tree")
                .needs_you += 1;
            parent = &tree.nodes[&key].shown_under;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn folder(id: &str) -> NodeRef {
        NodeRef::Folder { id: id.into() }
    }

    fn subject(id: &str) -> NodeRef {
        NodeRef::Subject { subject: id.into() }
    }

    fn under(id: &str) -> Parent {
        Parent::Folder { id: id.into() }
    }

    fn arrangement() -> Arrangement {
        serde_json::from_value(json!({
            "id": "arrangement/person/test/id", "kind": "arrangement",
            "revision": "opaque/head", "updated_at": "", "owner": "person/test",
            "deleted": false,
            "body": {
                "version": 1, "name": {"value": "Sidebar", "revision": "opaque/name"},
                "folders": {}, "placements": {}
            },
            "resolved": {"parents": {}, "folders": {}}
        }))
        .unwrap()
    }

    fn add_folder(arrangement: &mut Arrangement, id: &str, parent: Option<&str>, key: &str) {
        arrangement.body.folders.insert(
            id.into(),
            serde_json::from_value(json!({
                "name": {"value": id, "revision": "opaque/name"},
                "position": {
                    "value": {"parent": parent, "key": key}, "revision": "opaque/position"
                },
                "tombstone": null
            }))
            .unwrap(),
        );
        arrangement
            .resolved
            .as_mut()
            .unwrap()
            .parents
            .insert(id.into(), parent.map(str::to_owned));
    }

    fn place(arrangement: &mut Arrangement, id: &str, parent: Option<&str>, key: &str) {
        arrangement.body.placements.insert(
            id.into(),
            serde_json::from_value(json!({
                "value": {"folder": parent, "key": key}, "revision": "opaque/placement"
            }))
            .unwrap(),
        );
        arrangement
            .resolved
            .as_mut()
            .unwrap()
            .folders
            .insert(id.into(), parent.map(str::to_owned));
    }

    #[test]
    fn shared_vectors() {
        let vectors: Value =
            serde_json::from_str(include_str!("../../../fixtures/clients/sidebar-v2.json"))
                .unwrap();
        assert_eq!(vectors["scope"], "folder-only-v1");
        for case in vectors["cases"].as_array().unwrap() {
            let arrangement = serde_json::from_value(case["arrangement"].clone()).unwrap();
            let inventory: Vec<String> = serde_json::from_value(case["inventory"].clone()).unwrap();
            let tree = build(&arrangement, &inventory).unwrap();
            assert_eq!(
                serde_json::to_value(&tree).unwrap(),
                case["expected"],
                "{}",
                case["name"]
            );
            for vector in case["moves"].as_array().unwrap() {
                let intent: MoveIntent = serde_json::from_value(vector["intent"].clone()).unwrap();
                let result = plan_move(&tree, &intent);
                assert_eq!(can_place(&tree, &intent), result.clone().map(|_| ()));
                let serialized = match result {
                    Ok(intent) => json!({"_tag": "Planned", "intent": intent}),
                    Err(refusal) => json!({"_tag": "Refused", "refusal": refusal}),
                };
                assert_eq!(serialized, vector["result"], "{}: {intent:?}", case["name"]);
            }
            let needs_you: BTreeSet<String> =
                serde_json::from_value(case["needsYou"].clone()).unwrap();
            assert_eq!(
                serde_json::to_value(rollup(&tree, &needs_you)).unwrap(),
                case["rollup"],
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn rejects_unavailable_unsupported_and_incomplete_projection() {
        let mut arrangement = arrangement();
        arrangement.deleted = true;
        arrangement.body.version = 2;
        arrangement.resolved = None;
        assert_eq!(
            build(&arrangement, &[]),
            Err(ProjectionError::ArrangementUnavailable)
        );
        arrangement.deleted = false;
        assert_eq!(
            build(&arrangement, &[]),
            Err(ProjectionError::Unsupported {
                required: 1,
                advertised: 2
            })
        );
        arrangement.body.version = 1;
        assert_eq!(build(&arrangement, &[]), Err(ProjectionError::NotReady));
        arrangement.resolved = Some(st3_client::ArrangementResolved {
            parents: BTreeMap::new(),
            folders: BTreeMap::new(),
        });
        add_folder(&mut arrangement, "a", None, "a0");
        arrangement.resolved.as_mut().unwrap().parents.clear();
        assert_eq!(build(&arrangement, &[]), Err(ProjectionError::NotReady));
        arrangement.body.folders.clear();
        place(&mut arrangement, "dormant", None, "a0");
        arrangement.resolved.as_mut().unwrap().folders.clear();
        assert_eq!(build(&arrangement, &[]), Err(ProjectionError::NotReady));
    }

    #[test]
    fn rejects_nonlive_parents_and_resolved_cycles() {
        let mut arrangement = arrangement();
        add_folder(&mut arrangement, "a", Some("b"), "a0");
        assert_eq!(
            build(&arrangement, &[]),
            Err(ProjectionError::InvalidResolution)
        );
        add_folder(&mut arrangement, "b", Some("a"), "a0");
        assert_eq!(
            build(&arrangement, &[]),
            Err(ProjectionError::InvalidResolution)
        );
        arrangement
            .resolved
            .as_mut()
            .unwrap()
            .parents
            .insert("a".into(), Some("a".into()));
        assert_eq!(
            build(&arrangement, &[]),
            Err(ProjectionError::InvalidResolution)
        );
        arrangement
            .resolved
            .as_mut()
            .unwrap()
            .parents
            .insert("a".into(), None);
        arrangement.body.folders.get_mut("a").unwrap().tombstone =
            Some(st3_client::ArrangementRegister {
                value: true,
                revision: "opaque/delete".into(),
            });
        assert_eq!(
            build(&arrangement, &[]),
            Err(ProjectionError::InvalidResolution)
        );
        arrangement.body.folders.clear();
        place(&mut arrangement, "subject", Some("missing"), "a0");
        assert_eq!(
            build(&arrangement, &[]),
            Err(ProjectionError::InvalidResolution)
        );
    }

    #[test]
    fn ordinal_ordering_domains_and_inventory_union() {
        let mut arrangement = arrangement();
        add_folder(&mut arrangement, "z", None, "a0");
        add_folder(&mut arrangement, "a", None, "a0");
        place(&mut arrangement, "\u{10000}", None, "a0");
        place(&mut arrangement, "\u{e000}", None, "a0");
        let tree = build(
            &arrangement,
            &["aaa".into(), "aaa".into(), "\u{10000}".into()],
        )
        .unwrap();
        assert_eq!(
            tree.roots,
            vec![
                folder("a"),
                folder("z"),
                subject("\u{e000}"),
                subject("\u{10000}"),
                subject("aaa")
            ]
        );
        assert_eq!(tree.nodes.len(), 5);
        assert_eq!(tree.arrangement_revision, "opaque/head");
        assert_eq!(
            tree.nodes["subject:\u{10000}"].revision.as_deref(),
            Some("opaque/placement")
        );
        assert_eq!(node_key(&folder("subject:x")), "folder:subject:x");
        assert_eq!(node_key(&subject("folder:x")), "subject:folder:x");
        assert_eq!(tree.nodes["subject:aaa"].source, Source::Fallback);
        assert_eq!(
            rollup(&tree, &BTreeSet::from(["aaa".into(), "absent".into()])),
            BTreeMap::from([
                (
                    "folder:a".into(),
                    Rollup {
                        needs_you: 0,
                        display: RollupDisplay::Always
                    }
                ),
                (
                    "folder:z".into(),
                    Rollup {
                        needs_you: 0,
                        display: RollupDisplay::Always
                    }
                ),
            ])
        );
    }

    #[test]
    fn refusal_precedence_cycle_sources_and_anchor_checks() {
        let mut arrangement = arrangement();
        add_folder(&mut arrangement, "a", None, "a0");
        add_folder(&mut arrangement, "b", Some("a"), "a0");
        add_folder(&mut arrangement, "c", Some("b"), "a0");
        place(&mut arrangement, "leaf", Some("b"), "a0");
        let mut tree = build(&arrangement, &["unplaced".into()]).unwrap();
        let intent = MoveIntent {
            node: folder("absent"),
            parent: Parent::Subject {
                subject: "leaf".into(),
            },
            before: Some(subject("absent")),
        };
        assert_eq!(
            plan_move(&tree, &intent),
            Err(MoveRefusal::NodeMissing {
                node: folder("absent")
            })
        );
        let intent = MoveIntent {
            node: folder("a"),
            ..intent
        };
        assert!(matches!(
            plan_move(&tree, &intent),
            Err(MoveRefusal::ParentNotAllowed { .. })
        ));
        let intent = MoveIntent {
            parent: under("a"),
            ..intent
        };
        assert_eq!(
            plan_move(&tree, &intent),
            Err(MoveRefusal::SelfMove { node: folder("a") })
        );
        tree.nodes.get_mut("folder:b").unwrap().source = Source::Fallback;
        let intent = MoveIntent {
            parent: under("c"),
            ..intent
        };
        assert_eq!(
            plan_move(&tree, &intent),
            Err(MoveRefusal::Cycle {
                node: folder("a"),
                parent: under("c"),
                path: vec![
                    CycleStep {
                        node: folder("a"),
                        source: Source::Person
                    },
                    CycleStep {
                        node: folder("c"),
                        source: Source::Person
                    },
                    CycleStep {
                        node: folder("b"),
                        source: Source::Fallback
                    },
                    CycleStep {
                        node: folder("a"),
                        source: Source::Person
                    },
                ],
            })
        );
        for (before, found_under) in [
            (subject("absent"), None),
            (subject("leaf"), Some(under("b"))),
            (folder("a"), Some(Parent::Root {})),
            (subject("unplaced"), Some(Parent::Root {})),
        ] {
            let intent = MoveIntent {
                node: subject("unplaced"),
                parent: Parent::Root {},
                before: Some(before.clone()),
            };
            assert_eq!(
                plan_move(&tree, &intent),
                Err(MoveRefusal::AnchorMoved {
                    before,
                    parent: Parent::Root {},
                    found_under,
                })
            );
        }
    }

    #[test]
    fn lifted_parent_is_actionable_and_deleted_parent_is_distinct() {
        let mut arrangement = arrangement();
        add_folder(&mut arrangement, "gone", None, "a0");
        arrangement.body.folders.get_mut("gone").unwrap().tombstone =
            Some(st3_client::ArrangementRegister {
                value: false,
                revision: "opaque/delete".into(),
            });
        place(&mut arrangement, "leaf", Some("gone"), "a0");
        arrangement
            .resolved
            .as_mut()
            .unwrap()
            .folders
            .insert("leaf".into(), None);
        let tree = build(&arrangement, &[]).unwrap();
        let intent = MoveIntent {
            node: subject("leaf"),
            parent: Parent::Root {},
            before: None,
        };
        assert_eq!(plan_move(&tree, &intent), Ok(intent.clone()));
        for (id, cause) in [
            ("gone", ParentMissingCause::FolderDeleted),
            ("unknown", ParentMissingCause::Unknown),
        ] {
            let intent = MoveIntent {
                parent: under(id),
                ..intent.clone()
            };
            assert_eq!(
                plan_move(&tree, &intent),
                Err(MoveRefusal::ParentMissing {
                    parent: under(id),
                    cause,
                })
            );
        }
        arrangement
            .body
            .placements
            .get_mut("leaf")
            .unwrap()
            .value
            .folder = None;
        let tree = build(&arrangement, &[]).unwrap();
        assert_eq!(
            plan_move(&tree, &intent),
            Err(MoveRefusal::NoChange {
                node: intent.node.clone(),
                parent: intent.parent.clone(),
            })
        );
    }
}
