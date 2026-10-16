//! Host repository suggestions from declarations and replicated workspace observations.
use anyhow::Result;
use st3_client::{AgentRepository, HostRepositories};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A graph-only read: paths belong to the selected host, even through another member.
pub fn host_repositories(store: &crate::store::Store, host: &str) -> Result<HostRepositories> {
    let host = host.trim_start_matches("host/");
    let mut repositories = BTreeMap::<String, (BTreeSet<String>, BTreeSet<String>)>::new();
    for agent in store.agent_repository_subjects(host)? {
        let Some(member) = agent
            .member
            .as_ref()
            .filter(|member| agent.kind == "agent" && member.host == host)
        else {
            continue;
        };
        let declared = crate::checkout::Checkout::from_desired(&agent.desired)
            .map(|checkout| checkout.repository.display().to_string());
        let observed = store.workspace_observation_from_host(&agent.subject, host)?;
        let observed = observed.as_ref().filter(|claim| {
            claim.body["fields"]["workspace"] == member.workspace
                && claim.body["fields"]["host"] == host
        });
        for path in declared
            .as_deref()
            .into_iter()
            .chain(observed.and_then(|claim| claim.body["fields"]["repository"].as_str()))
        {
            let (workspaces, agents) = repositories.entry(path.to_owned()).or_default();
            workspaces.insert(member.workspace.clone());
            agents.insert(agent.subject.clone());
        }
    }
    Ok(HostRepositories {
        host_id: format!("host/{host}"),
        repositories: repositories
            .into_iter()
            .map(|(path, (workspaces, agent_ids))| AgentRepository {
                path,
                workspaces: workspaces.into_iter().collect(),
                agent_ids: agent_ids.into_iter().collect(),
            })
            .collect(),
    })
}

/// Read Git's directory pointers on the owning host during reconciliation. Checks only the
/// declared workspace and its ancestors, with no search through other host directories.
pub(crate) fn workspace_repository(workspace: &Path) -> Option<PathBuf> {
    let workspace = std::fs::canonicalize(workspace).ok()?;
    for root in workspace.ancestors() {
        let dot_git = root.join(".git");
        if dot_git.is_dir() {
            return Some(root.to_path_buf());
        }
        if dot_git.is_file() {
            let pointer = std::fs::read_to_string(dot_git).ok()?;
            let git_dir = root.join(pointer.strip_prefix("gitdir:")?.trim());
            if let Ok(common) = std::fs::read_to_string(git_dir.join("commondir")) {
                let common = std::fs::canonicalize(git_dir.join(common.trim())).ok()?;
                return Some(if common.file_name().is_some_and(|name| name == ".git") {
                    common.parent()?.to_path_buf()
                } else {
                    // Bare repositories keep their shared Git directory at the repository root.
                    common
                });
            }
            return Some(root.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkout::test_support::{git, repository};
    #[test]
    fn git_workspaces_and_linked_worktrees_name_the_shared_repository() {
        let root = tempfile::tempdir().unwrap();
        let repository = repository(root.path());
        std::fs::create_dir(repository.join("nested")).unwrap();
        assert_eq!(
            workspace_repository(&repository.join("nested")),
            Some(repository.clone())
        );
        let linked = root.path().join("linked");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "parser",
                linked.to_str().unwrap(),
            ],
        );
        assert_eq!(workspace_repository(&linked), Some(repository));
        let bare = root.path().join("origin.git");
        let bare_linked = root.path().join("bare-linked");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "bare-parser",
                bare_linked.to_str().unwrap(),
                "main",
            ],
        );
        assert_eq!(workspace_repository(&bare_linked), Some(bare));
        assert_eq!(workspace_repository(root.path()), None);
    }

    #[test]
    fn repository_suggestions_use_only_the_selected_hosts_graph_evidence() {
        let store = crate::store::Store::open_memory("other").unwrap();
        let source = "version 2\nagent \"example/parser\" { host \"example\"; workspace \"/absent/parser\"; checkout \"/absent/repo\" base=\"origin/main\" branch=\"parser\"; command \"true\" }\nagent \"example/plain\" { host \"other\"; workspace \"/absent/plain\"; command \"true\" }\n";
        let intent = crate::graph::parse_intent(source, "other").unwrap();
        let preview = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &preview.subject_tokens, "repositories")
            .unwrap();
        let answer = host_repositories(&store, "host/example").unwrap();
        assert_eq!(answer.host_id, "host/example");
        assert_eq!(answer.repositories[0].path, "/absent/repo");
        assert_eq!(answer.repositories[0].workspaces, ["/absent/parser"]);
        assert_eq!(answer.repositories[0].agent_ids, ["agent/example/parser"]);
        assert!(
            host_repositories(&store, "other")
                .unwrap()
                .repositories
                .is_empty()
        );
        store
            .append_claim(&crate::model::ClaimInput {
                subject: "agent/example/plain".into(),
                kind: "workspace.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("host".into(), "other".into()),
                    ("workspace".into(), "/absent/plain".into()),
                    ("repository".into(), "/absent/other-repo".into()),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert_eq!(
            host_repositories(&store, "other").unwrap().repositories[0].path,
            "/absent/other-repo"
        );
        assert_eq!(host_repositories(&store, "example").unwrap(), answer);
        assert!(
            host_repositories(&store, "unknown")
                .unwrap()
                .repositories
                .is_empty()
        );
        let gateway = crate::store::Store::open_memory("gateway").unwrap();
        gateway
            .import_replication("other", &store.export_replication(0).unwrap())
            .unwrap();
        assert_eq!(
            host_repositories(&gateway, "other").unwrap(),
            host_repositories(&store, "other").unwrap()
        );
        assert_eq!(host_repositories(&gateway, "example").unwrap(), answer);
    }
}
