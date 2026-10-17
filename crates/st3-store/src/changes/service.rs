//! One configured registry per daemon. Only its named owner performs Git network
//! reads; an unavailable owner preserves graph reads and the existing PR path.
use super::config::{Config, Repository as RepositoryConfig};
use super::discovery::{Observation, Owner, SyncAdmission, Transport};
use super::transport::{Advertisement, Authentication, Repository};
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct AuthorizedTransport {
    repository: Repository,
    directory: PathBuf,
    remote: String,
    destination: String,
}

impl AuthorizedTransport {
    fn check_destination(&self) -> Result<()> {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.directory).args([
            "config",
            "--get",
            &format!("remote.{}.url", self.remote),
        ]);
        for key in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_NAMESPACE",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
        ] {
            command.env_remove(key);
        }
        let output = command.output()?;
        ensure!(
            output.status.success(),
            "configured Git remote is unavailable"
        );
        ensure!(
            String::from_utf8(output.stdout)?.trim() == self.destination,
            "Git destination changed from the operator-authorized configuration"
        );
        Ok(())
    }
}

impl Transport for AuthorizedTransport {
    fn discover(&self) -> Result<Advertisement> {
        self.check_destination()?;
        self.repository.discover(&self.remote)
    }

    fn sync(&self) -> Result<()> {
        self.check_destination()?;
        self.repository.sync(&self.remote)
    }
}

pub struct Entry {
    pub configuration: RepositoryConfig,
    pub owner: Option<Owner>,
    pub service_user: Option<String>,
    repository: Option<Repository>,
    unavailable: Option<String>,
    indexing: Arc<tokio::sync::Mutex<()>>,
    pins: std::sync::Mutex<VecDeque<Pin>>,
}

struct Pin {
    id: String,
    observed: Observation,
    created: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SyncOutcome {
    Completed { import: ImportReport },
    InFlight,
    Deferred { retry_after_ms: u64 },
    Unavailable { reader_node: String, reason: String },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ImportReport {
    pub indexed_objects: usize,
    pub visited_objects: usize,
    pub elapsed_ms: u64,
    pub skipped_refs: usize,
    pub issues: Vec<ImportIssue>,
    pub omitted_issues: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImportIssue {
    pub ref_change_id: Option<String>,
    pub record_change_id: Option<String>,
    pub object_id: String,
    pub reason: &'static str,
}

impl ImportReport {
    fn skip(&mut self, issue: ImportIssue) {
        self.skipped_refs += 1;
        if self.issues.len() < 50 {
            self.issues.push(issue);
        } else {
            self.omitted_issues += 1;
        }
    }
}

pub struct Registry {
    entries: BTreeMap<String, Arc<Entry>>,
}

impl Registry {
    pub fn new(configuration: &Config, node: &str) -> Result<Self> {
        configuration.validate()?;
        let mut entries = BTreeMap::new();
        if !configuration.enabled {
            return Ok(Self { entries });
        }
        let mut destinations = BTreeSet::new();
        for configured in &configuration.repositories {
            ensure!(
                destinations.insert(configured.remote_url.clone()),
                "duplicate configured Git destination"
            );
            let mut entry = Entry {
                configuration: configured.clone(),
                owner: None,
                service_user: None,
                repository: None,
                unavailable: None,
                indexing: Arc::new(tokio::sync::Mutex::new(())),
                pins: std::sync::Mutex::new(VecDeque::new()),
            };
            if configured.reader_node != node {
                entry.unavailable = Some(format!(
                    "Git discovery is owned by node {}; use that node or the PR path",
                    configured.reader_node
                ));
            } else {
                entry.service_user = Command::new("id")
                    .arg("-un")
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .and_then(|output| String::from_utf8(output.stdout).ok())
                    .map(|user| user.trim().to_owned());
                let opened = (|| {
                    let directory = std::fs::canonicalize(&configured.transport_directory)?;
                    let repository = Repository::open(
                        &directory,
                        Authentication {
                            helper: configured.credential_helper.clone(),
                            ssh_command: configured.ssh_command.clone(),
                        },
                    )?;
                    let transport = AuthorizedTransport {
                        repository: repository.clone(),
                        directory,
                        remote: configured.remote.clone(),
                        destination: configured.remote_url.clone(),
                    };
                    transport.check_destination()?;
                    Ok::<_, anyhow::Error>((repository, transport))
                })();
                match opened {
                    Ok((repository, transport)) => {
                        entry.repository = Some(repository);
                        entry.owner = Some(Owner::new(Arc::new(transport)));
                    }
                    Err(error) => {
                        entry.unavailable = Some(format!(
                            "Git transport unavailable: {error}; use the PR path"
                        ))
                    }
                }
            }
            entries.insert(configured.name.clone(), Arc::new(entry));
        }
        Ok(Self { entries })
    }

    pub fn entry(&self, name: &str) -> Option<Arc<Entry>> {
        self.entries.get(name).cloned()
    }
}

impl Entry {
    pub async fn snapshot(
        &self,
        offline: bool,
        pin: Option<&str>,
    ) -> Result<(String, Observation)> {
        if let Some(pin) = pin {
            let pins = self
                .pins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let pinned = pins
                .iter()
                .find(|entry| {
                    entry.id == pin
                        && entry.observed.offline == offline
                        && entry.created.elapsed() < Duration::from_secs(300)
                })
                .context("change cursor expired; restart the list")?;
            let mut observed = pinned.observed.clone();
            observed.reused = true;
            return Ok((pin.to_owned(), observed));
        }
        let observed = self.observation(offline).await?;
        // A pin names this bounded snapshot; hashing the full ref inventory would
        // repeat repository-sized work for every first-page request.
        let id = super::random_hex::<32>()?;
        let mut pins = self
            .pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pins.retain(|pin| pin.created.elapsed() < Duration::from_secs(300));
        if !pins.iter().any(|pin| pin.id == id) {
            while pins.len() >= 32 {
                pins.pop_front();
            }
            pins.push_back(Pin {
                id: id.clone(),
                observed: observed.clone(),
                created: Instant::now(),
            });
        }
        Ok((id, observed))
    }

    pub async fn observation(&self, offline: bool) -> Result<Observation> {
        if let Some(owner) = &self.owner {
            return owner.observe(offline).await;
        }
        let mut observation = Observation::default();
        observation.error = self.unavailable.clone();
        observation.offline = offline;
        observation.reused = true;
        Ok(observation)
    }

    /// Explicit, budgeted namespace fetch followed by finite local object validation.
    /// No SQLite read snapshot or writer is held during Git work.
    pub async fn sync(&self, store: Arc<Store>) -> Result<SyncOutcome> {
        let Some(owner) = &self.owner else {
            return Ok(SyncOutcome::Unavailable {
                reader_node: self.configuration.reader_node.clone(),
                reason: self
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "Git reader unavailable; use the PR path".into()),
            });
        };
        match owner.sync().await? {
            SyncAdmission::InFlight => return Ok(SyncOutcome::InFlight),
            SyncAdmission::Deferred { retry_after_ms } => {
                return Ok(SyncOutcome::Deferred { retry_after_ms });
            }
            SyncAdmission::Completed => {}
        }
        let repository = self
            .repository
            .clone()
            .context("Git repository unavailable")?;
        let configured = self.configuration.clone();
        let import = self
            .run_import(move || import_tracking(&store, &repository, &configured))
            .await?;
        Ok(SyncOutcome::Completed { import })
    }
    async fn run_import<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let indexing = self.indexing.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            // Cancellation drops the join handle, but the blocking importer keeps
            // its owned slot until it has actually finished.
            let _indexing = indexing;
            operation()
        })
        .await
        .context("Git index importer stopped")?
    }
}

fn import_tracking(
    store: &Store,
    repository: &Repository,
    configuration: &RepositoryConfig,
) -> Result<ImportReport> {
    let started = Instant::now();
    let namespace = format!(
        "refs/st/tracking/{}/",
        hex::encode(Sha256::digest(configuration.remote_url.as_bytes()))
    );
    let inventory = repository.tracking_inventory(&configuration.remote_url)?;
    ensure!(
        inventory.len() <= 1024 * 1024,
        "tracking inventory exceeds import budget"
    );
    let mut report = ImportReport::default();
    let mut pending = Vec::new();
    for line in inventory.lines() {
        let (name, object) = line.split_once(' ').context("malformed tracking ref")?;
        if name.ends_with("/current") {
            super::oid(object)?;
            let suffix = name
                .strip_prefix(&namespace)
                .context("tracking ref outside isolated namespace")?;
            let (id, marker) = suffix
                .split_once('/')
                .context("malformed tracking current ref")?;
            if marker != "current" || super::change_id(id).is_err() {
                report.skip(ImportIssue {
                    ref_change_id: None,
                    record_change_id: None,
                    object_id: object.into(),
                    reason: "malformed_current_ref",
                });
                continue;
            }
            pending.push((id.to_owned(), object.to_owned()));
        }
    }
    ensure!(
        pending.len() <= 5000,
        "tracking inventory exceeds change budget"
    );
    let mut visited = 0usize;
    let mut mismatched_objects = BTreeMap::<String, String>::new();
    'currents: while let Some((expected_id, current)) = pending.pop() {
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cursor = Some(current);
        while let Some(object) = cursor {
            if let Some(actual_id) = mismatched_objects.get(&object)
                && actual_id != &expected_id
            {
                report.skip(ImportIssue {
                    ref_change_id: Some(expected_id),
                    record_change_id: Some(actual_id.clone()),
                    object_id: object,
                    reason: "change_id_mismatch",
                });
                continue 'currents;
            }
            let known =
                store.indexed_git_changes(&configuration.name, std::slice::from_ref(&object))?;
            if let Some(known) = known.get(&object) {
                if known.revision.record.change_id != expected_id {
                    report.skip(ImportIssue {
                        ref_change_id: Some(expected_id),
                        record_change_id: Some(known.revision.record.change_id.clone()),
                        object_id: object,
                        reason: "change_id_mismatch",
                    });
                    continue 'currents;
                }
                break;
            }
            ensure!(
                seen.insert(object.clone()),
                "metadata history cycle or repeated unindexed object"
            );
            visited += 1;
            ensure!(
                visited <= 10000,
                "metadata history exceeds import job budget; use the PR path"
            );
            let revision = repository.load(&object)?;
            if revision.record.change_id != expected_id {
                mismatched_objects.insert(object.clone(), revision.record.change_id.clone());
                report.skip(ImportIssue {
                    ref_change_id: Some(expected_id),
                    record_change_id: Some(revision.record.change_id),
                    object_id: object,
                    reason: "change_id_mismatch",
                });
                continue 'currents;
            }
            cursor = revision.record.previous_record.clone();
            chain.push(revision);
        }
        // Do not publish a current record before its otherwise unknown lineage validates.
        // Each receipt remains a separate short transaction, without a Git-held writer.
        for revision in chain.into_iter().rev() {
            store.index_git_change(&configuration.name, &revision)?;
            report.indexed_objects += 1;
        }
    }
    report.visited_objects = visited;
    report.elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> Config {
        Config {
            enabled: true,
            repositories: vec![RepositoryConfig {
                name: "fixture/garden".into(),
                reader_node: "fixture-owner".into(),
                transport_directory: PathBuf::from("/missing/fixture/transport"),
                remote: "origin".into(),
                remote_url: "https://example.invalid/fixture/garden.git".into(),
                pilot_started_at: "2026-01-01T00:00:00Z".into(),
                ..RepositoryConfig::default()
            }],
        }
    }

    #[tokio::test]
    async fn nonowner_and_missing_transport_preserve_unavailable_observations() {
        for node in ["fixture-other", "fixture-owner"] {
            let registry = Registry::new(&configured(), node).unwrap();
            let entry = registry.entry("fixture/garden").unwrap();
            assert!(entry.owner.is_none());
            let store = Arc::new(Store::open_memory("fixture-owner").unwrap());
            assert!(
                matches!(entry.sync(store).await.unwrap(), SyncOutcome::Unavailable { reader_node, reason } if reader_node == "fixture-owner" && reason.contains("PR path"))
            );
            let observed = entry.observation(false).await.unwrap();
            assert!(observed.advertisement().is_none());
            assert!(observed.error.as_deref().unwrap().contains("PR path"));
            let offline = entry.observation(true).await.unwrap();
            assert!(offline.offline);
            assert!(offline.advertisement().is_none());
        }
        assert!(
            Registry::new(&Config::default(), "fixture-owner")
                .unwrap()
                .entry("fixture/garden")
                .is_none()
        );
    }

    #[tokio::test]
    async fn observation_cursor_is_pinned_and_cannot_switch_offline_mode() {
        let registry = Registry::new(&configured(), "fixture-other").unwrap();
        let entry = registry.entry("fixture/garden").unwrap();
        let (pin, first) = entry.snapshot(false, None).await.unwrap();
        let (same, next) = entry.snapshot(false, Some(&pin)).await.unwrap();
        assert_eq!(same, pin);
        assert_eq!(next.attempted_at, first.attempted_at);
        assert_eq!(next.error, first.error);
        assert!(entry.snapshot(true, Some(&pin)).await.is_err());
        assert!(entry.snapshot(false, Some("missing")).await.is_err());
    }
    #[tokio::test]
    async fn explicit_fetch_indexes_exact_current_and_rejects_mismatched_ref_identity() {
        use crate::changes::{Record, State, TrailerStatus, new_change_id};
        use std::path::Path;
        fn git(path: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(path)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let remote = root.path().join("remote.git");
        let reader = root.path().join("reader");
        for directory in [&source, &remote, &reader] {
            std::fs::create_dir(directory).unwrap();
        }
        git(&source, &["init", "-q"]);
        git(&remote, &["init", "--bare", "-q"]);
        git(&reader, &["init", "-q"]);
        let destination = remote.to_str().unwrap();
        git(&source, &["remote", "add", "origin", destination]);
        git(&reader, &["remote", "add", "origin", destination]);
        git(
            &source,
            &["commit", "--allow-empty", "-qm", "Synthetic base"],
        );
        let base = git(&source, &["rev-parse", "HEAD"]);
        let id = new_change_id().unwrap();
        git(
            &source,
            &[
                "commit",
                "--allow-empty",
                "-qm",
                &format!("Synthetic change\n\nChange-Id: {id}"),
            ],
        );
        let repository = Repository::open(&source, Authentication::default()).unwrap();
        let (source_commit, source_tree) = repository.source("HEAD").unwrap();
        let revision = repository
            .prepare(Record {
                schema_version: 1,
                change_id: id.clone(),
                generation: 1,
                previous_record: None,
                source_commit,
                source_tree,
                base_commit: base,
                trailer_status: TrailerStatus::Present,
                state: State::Draft,
                next: None,
                depends_on: vec![],
                actor: "agent/fixture/builder".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                claim: None,
                mirror: None,
            })
            .unwrap();
        // A valid object for B cannot enter B's history through a ref labelled A.
        let wrong_id = format!("I{}", "f".repeat(40));
        assert_ne!(wrong_id, id);
        let prefix = format!(
            "refs/st/tracking/{}/",
            hex::encode(Sha256::digest(destination.as_bytes()))
        );
        git(
            &source,
            &[
                "update-ref",
                &format!("{prefix}{wrong_id}/current"),
                &revision.oid,
            ],
        );
        let mut local_configuration = configured().repositories.remove(0);
        local_configuration.transport_directory = source.clone();
        local_configuration.remote_url = destination.into();
        let isolated = Store::open_memory("fixture-owner").unwrap();
        let refused = import_tracking(&isolated, &repository, &local_configuration).unwrap();
        assert_eq!(refused.indexed_objects, 0);
        assert_eq!(refused.skipped_refs, 1);
        assert_eq!(refused.issues[0].reason, "change_id_mismatch");
        assert!(
            isolated
                .latest_indexed_git_change("fixture/garden", &id)
                .unwrap()
                .is_none()
        );
        assert!(
            isolated
                .latest_indexed_git_change("fixture/garden", &wrong_id)
                .unwrap()
                .is_none()
        );
        // Both refs coexist: rejecting the first alias must not suppress valid B.
        git(
            &source,
            &[
                "update-ref",
                &format!("{prefix}{id}/current"),
                &revision.oid,
            ],
        );
        let mixed = import_tracking(&isolated, &repository, &local_configuration).unwrap();
        assert_eq!(mixed.indexed_objects, 1);
        assert_eq!(mixed.skipped_refs, 1);
        assert_eq!(
            isolated
                .latest_indexed_git_change("fixture/garden", &id)
                .unwrap()
                .unwrap()
                .revision,
            revision
        );
        repository.publish("origin", None, &revision).unwrap();
        let mut configuration = configured();
        configuration.repositories[0].transport_directory = reader;
        configuration.repositories[0].remote_url = destination.to_owned();
        let registry = Registry::new(&configuration, "fixture-owner").unwrap();
        let entry = registry.entry("fixture/garden").unwrap();
        let store = Arc::new(Store::open_memory("fixture-owner").unwrap());
        assert!(
            matches!(entry.sync(store.clone()).await.unwrap(), SyncOutcome::Completed { import } if import.indexed_objects == 1 && import.skipped_refs == 0)
        );
        let indexed = store
            .indexed_git_changes("fixture/garden", std::slice::from_ref(&revision.oid))
            .unwrap();
        assert_eq!(indexed[&revision.oid].revision, revision);
        // Explicit fetch uses the same budget as discovery, so this immediate
        // observation must not start a second network operation.
        assert!(
            entry
                .observation(false)
                .await
                .unwrap()
                .advertisement()
                .is_none()
        );
        let observed =
            Observation::from_advertisement(repository.discover("origin").unwrap()).unwrap();
        let detail = super::super::view::show(&store, "fixture/garden", observed, &id).unwrap();
        assert!(matches!(
            detail.item.status,
            super::super::view::Status::Indexed
        ));
        assert_eq!(
            entry
                .owner
                .as_ref()
                .unwrap()
                .diagnostics()
                .await
                .counters
                .discoveries_started,
            0
        );
        assert_eq!(
            entry
                .owner
                .as_ref()
                .unwrap()
                .diagnostics()
                .await
                .counters
                .fetches_started,
            1
        );
        // A changed checkout URL cannot redirect an authorized subsequent operation.
        git(
            &configuration.repositories[0].transport_directory,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/other.git",
            ],
        );
        let guarded = AuthorizedTransport {
            repository: entry.repository.clone().unwrap(),
            directory: configuration.repositories[0].transport_directory.clone(),
            remote: "origin".into(),
            destination: destination.into(),
        };
        assert!(
            guarded
                .discover()
                .unwrap_err()
                .to_string()
                .contains("authorized")
        );
    }
    #[tokio::test]
    async fn canceled_import_keeps_its_slot_until_blocking_validation_finishes() {
        let registry = Registry::new(&configured(), "fixture-other").unwrap();
        let entry = registry.entry("fixture/garden").unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let client = tokio::spawn({
            let entry = entry.clone();
            let started = started.clone();
            let gate = gate.clone();
            async move {
                entry
                    .run_import(move || {
                        started.notify_one();
                        let (lock, condition) = &*gate;
                        let mut released = lock.lock().unwrap();
                        while !*released {
                            released = condition.wait(released).unwrap();
                        }
                        Ok(())
                    })
                    .await
            }
        });
        started.notified().await;
        client.abort();
        assert!(entry.indexing.try_lock().is_err());
        let (lock, condition) = &*gate;
        *lock.lock().unwrap() = true;
        condition.notify_all();
        let completed = entry.indexing.lock().await;
        drop(completed);
        assert!(entry.indexing.try_lock().is_ok());
    }
}
