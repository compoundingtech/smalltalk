//! Protected observers derive facts from authenticated providers, never a seat's statement.
//! The transport calls this module with bytes it fetched itself. No signing CLI accepts facts.
use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceReceipt {
    pub version: u32,
    pub repository: String,
    pub event: String,
    pub run_id: String,
    pub run_attempt: String,
    pub source: String,
    pub tree: String,
    pub object_format: String,
    pub change_id: String,
    pub producer_sha256: String,
    pub pack_sha256: String,
    pub pack_bytes: u64,
}

/// Operator-owned binding. Its recipe digest and check list never come from the target tree.
#[derive(Clone)]
pub struct CiBinding {
    pub repository: String,
    pub run: u64,
    pub attempt: u64,
    pub recipe_sha256: String,
    pub accepted_base: String,
    pub workflow_sha256: String,
    pub required_checks: BTreeSet<String>,
}

/// A protected process's signing mode. Throwaway services must select Test and expire;
/// production grants cannot select a mode or key supplied by their caller.
pub enum ObserverSigner<'a> {
    Full {
        principal: &'a str,
        key: &'a Path,
    },
    Test {
        principal: &'a str,
        key: &'a Path,
        expires_at: chrono::DateTime<chrono::Utc>,
    },
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct Produced {
    pub record: String,
    pub tier: TrustTier,
    pub signing_unavailable: bool,
    pub signing_latency_ms: u128,
    /// Replayed job selectors do not inflate production counters.
    pub newly_recorded: bool,
}

fn reference(repo: &Path, name: &str) -> Result<Option<String>> {
    let refs = git_text(
        repo,
        &["for-each-ref", "--format=%(refname) %(objectname)", name],
        None,
    )?;
    if refs.is_empty() {
        return Ok(None);
    }
    let (actual, value) = refs.split_once(' ').context("observer reference")?;
    ensure!(
        actual == name && !value.contains('\n'),
        "ambiguous observer reference"
    );
    oid(value)?;
    Ok(Some(value.into()))
}

/// Production of measured facts is fail-soft; validation above remains fail-closed.
/// A per-job local ref makes retries idempotent, including after a signing outage:
/// the same completed job cannot later become full without a fresh run/attempt.
pub fn produce_ci(
    repo: &Path,
    binding: &CiBinding,
    run: &Value,
    receipt: &SourceReceipt,
    jobs: &[Value],
    signer: &ObserverSigner<'_>,
) -> Result<Vec<Produced>> {
    produce_ci_linked(
        repo,
        binding,
        run,
        receipt,
        jobs,
        signer,
        &ci_reattest::Links::default(),
    )
}

pub fn produce_ci_linked(
    repo: &Path,
    binding: &CiBinding,
    run: &Value,
    receipt: &SourceReceipt,
    jobs: &[Value],
    signer: &ObserverSigner<'_>,
    links: &ci_reattest::Links,
) -> Result<Vec<Produced>> {
    let statements = ci_statements(repo, binding, run, receipt, jobs)?;
    if matches!(signer, ObserverSigner::Full { .. }) {
        ensure!(
            binding.full_eligible(repo, receipt, run)?,
            "source is not eligible for full CI trust: need a platform-bound queue/manual run and accepted workflow/recipe"
        );
    }
    // These checks happen before any write, never halfway through a set of observations.
    if let ObserverSigner::Test { expires_at, .. } = signer {
        let now = chrono::Utc::now();
        ensure!(
            *expires_at > now && *expires_at <= now + chrono::Duration::minutes(15),
            "throwaway signer must have a remaining lifetime of at most 15 minutes"
        );
    }
    match signer {
        ObserverSigner::Full { principal, .. } | ObserverSigner::Test { principal, .. } => {
            ensure!(
                !principal.is_empty() && !principal.chars().any(char::is_whitespace),
                "invalid configured signer principal"
            );
        }
        ObserverSigner::Unavailable => {}
    }
    statements
        .into_iter()
        .map(|mut statement| {
            let Fact::Tests { ref runner, .. } = statement.predicate.fact else {
                unreachable!();
            };
            if matches!(signer, ObserverSigner::Full { .. }) {
                statement.predicate.re_attests = links.for_runner(runner).cloned();
            }
            let identity = hex::encode(Sha256::digest(runner.as_bytes()));
            let observed_ref = format!("refs/evidence-observed/ci/{identity}");
            if let Some(record) = reference(repo, &observed_ref)? {
                let old = show(repo, &record)?;
                if old.predicate.tier == TrustTier::Full && statement.predicate.re_attests.is_some() {
                    ensure!(old.predicate.re_attests == statement.predicate.re_attests,
                        "completed job already has a different re-attestation link; use a fresh run/attempt");
                }
                if old.predicate.tier == TrustTier::Full {
                    ensure!(
                        binding.full_eligible(repo, receipt, run)?,
                        "old full observation lacks current source/workflow trust"
                    );
                }
                if matches!(signer, ObserverSigner::Test { .. }) {
                    ensure!(
                        old.predicate.tier != TrustTier::Full,
                        "test service must use an isolated observation repository"
                    );
                }
                ensure!(
                    old.subject[0].name == statement.subject[0].name
                        && old.predicate.change_id == statement.predicate.change_id
                        && serde_json::to_value(&old.predicate.fact)?
                            == serde_json::to_value(&statement.predicate.fact)?,
                    "completed job changed after observation"
                );
                return Ok(Produced {
                    tier: old.predicate.tier,
                    signing_unavailable: old.predicate.tier == TrustTier::Degraded,
                    signing_latency_ms: 0,
                    newly_recorded: false,
                    record,
                });
            }
            let started = std::time::Instant::now();
            let signed = match signer {
                ObserverSigner::Unavailable => None,
                ObserverSigner::Full { principal, key } => {
                    statement.predicate.tier = TrustTier::Full;
                    statement.predicate.signer = Some((*principal).into());
                    Some(sign_observation(repo, &statement, key))
                }
                ObserverSigner::Test { principal, key, .. } => {
                    statement.predicate.tier = TrustTier::Test;
                    statement.predicate.signer = Some((*principal).into());
                    Some(sign_observation(repo, &statement, key))
                }
            };
            let record = match signed {
                Some(Ok(record)) => record,
                Some(Err(_)) | None => {
                    statement.predicate.tier = TrustTier::Degraded;
                    statement.predicate.signer = None;
                    statement.predicate.re_attests = None;
                    record_degraded(repo, &statement)?
                }
            };
            let signing_latency_ms = started.elapsed().as_millis();
            let tier = show(repo, &record)?.predicate.tier;
            // Never overwrite a concurrently created observation of the same job.
            let notes_ref = format!("{NOTES_PREFIX}{record}");
            let note = match reference(repo, &notes_ref)? {
                Some(existing) => {
                    ensure!(existing == record, "existing notes ref points elsewhere");
                    format!("verify {notes_ref} {record}\n")
                }
                None => format!("create {notes_ref} {record}\n"),
            };
            git(
                repo,
                &["update-ref", "--stdin"],
                Some(
                    format!("start\ncreate {observed_ref} {record}\n{note}prepare\ncommit\n")
                        .as_bytes(),
                ),
            )?;
            Ok(Produced {
                record,
                tier,
                signing_unavailable: tier == TrustTier::Degraded,
                signing_latency_ms,
                newly_recorded: true,
            })
        })
        .collect()
}

impl CiBinding {
    pub fn validate(&self) -> Result<()> {
        let (owner, repository) = self
            .repository
            .split_once('/')
            .context("repository selector")?;
        let valid = |part: &str| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                && part != "."
                && part != ".."
        };
        ensure!(
            valid(owner) && valid(repository),
            "invalid repository selector"
        );
        ensure!(
            self.run > 0 && self.attempt > 0,
            "invalid run/attempt selector"
        );
        sha256(&self.recipe_sha256)?;
        oid(&self.accepted_base)?;
        sha256(&self.workflow_sha256)?;
        ensure!(
            !self.required_checks.is_empty()
                && self
                    .required_checks
                    .iter()
                    .all(|name| !name.trim().is_empty()),
            "operator must name required checks"
        );
        Ok(())
    }

    /// An artifact from a PR workflow cannot prove its effective tested source.
    /// Full trust additionally pins both executed workflow and producer recipe to
    /// an operator-selected accepted commit, never the tree supplied by the artifact.
    pub fn full_eligible(&self, repo: &Path, receipt: &SourceReceipt, run: &Value) -> Result<bool> {
        receipt.validate(repo, self, run)?;
        if run["event"] == "pull_request" {
            return Ok(false);
        }
        let base = commit(repo, &self.accepted_base)?;
        let base_recipe = git(
            repo,
            &["show", &format!("{base}:scripts/ci-evidence-source")],
            None,
        )?;
        let base_workflow = git(
            repo,
            &["show", &format!("{base}:.github/workflows/fleet.yml")],
            None,
        )?;
        ensure!(
            hex::encode(Sha256::digest(&base_recipe)) == self.recipe_sha256
                && hex::encode(Sha256::digest(&base_workflow)) == self.workflow_sha256,
            "accepted base does not match operator recipe/workflow pins"
        );
        let source_recipe = git(
            repo,
            &[
                "show",
                &format!("{}:scripts/ci-evidence-source", receipt.source),
            ],
            None,
        )?;
        let source_workflow = git(
            repo,
            &[
                "show",
                &format!("{}:.github/workflows/fleet.yml", receipt.source),
            ],
            None,
        )?;
        Ok(source_recipe == base_recipe && source_workflow == base_workflow)
    }

    pub fn validate_run(&self, run: &Value) -> Result<()> {
        self.validate()?;
        ensure!(
            run["id"].as_u64() == Some(self.run)
                && run["run_attempt"].as_u64() == Some(self.attempt)
                && run["repository"]["full_name"].as_str() == Some(self.repository.as_str())
                && run["name"].as_str() == Some("Workspace CI")
                && run["path"].as_str() == Some(".github/workflows/fleet.yml")
                && run["status"].as_str() == Some("completed"),
            "completed run does not match the configured observer"
        );
        ensure!(
            matches!(
                run["event"].as_str(),
                Some("pull_request" | "merge_group" | "workflow_dispatch")
            ),
            "unsupported CI event"
        );
        Ok(())
    }

    /// An artifact ID is authenticated separately; its metadata must bind this run/attempt.
    pub fn validate_artifact(&self, metadata: &Value, artifact: u64) -> Result<(u64, String)> {
        self.validate()?;
        ensure!(
            artifact > 0
                && metadata["id"].as_u64() == Some(artifact)
                && metadata["expired"].as_bool() == Some(false)
                && metadata["name"].as_str()
                    == Some(format!("ci-evidence-source-{}", self.attempt).as_str())
                && metadata["workflow_run"]["id"].as_u64() == Some(self.run),
            "source artifact does not match the configured run/attempt"
        );
        let size = metadata["size_in_bytes"]
            .as_u64()
            .context("artifact byte length")?;
        ensure!(
            size > 0 && size <= 128 * 1024 * 1024,
            "source artifact size bound"
        );
        let hash = metadata["digest"]
            .as_str()
            .and_then(|value| value.strip_prefix("sha256:"))
            .context("authenticated source artifact SHA-256")?;
        sha256(hash)?;
        Ok((size, hash.to_owned()))
    }

    /// All job bytes are fetched from the attempt-specific jobs endpoint. Reject duplicate
    /// required names rather than choosing a convenient success from a different execution.
    pub fn validate_jobs<'a>(&self, jobs: &'a [Value]) -> Result<Vec<&'a Value>> {
        self.validate()?;
        for job in jobs {
            ensure!(
                job["run_id"].as_u64() == Some(self.run)
                    && job["run_attempt"].as_u64() == Some(self.attempt),
                "job belongs to a different run/attempt"
            );
        }
        let named = |name: &str| -> Result<&Value> {
            let matches = jobs
                .iter()
                .filter(|job| job["name"].as_str() == Some(name))
                .collect::<Vec<_>>();
            ensure!(matches.len() == 1, "missing or ambiguous CI job: {name}");
            ensure!(
                matches[0]["status"] == "completed",
                "CI job is not completed: {name}"
            );
            ensure!(
                matches[0]["id"].as_u64().is_some_and(|id| id > 0),
                "invalid job ID"
            );
            Ok(matches[0])
        };
        let producer = named("evidence-source")?;
        for step in [
            "Retain effective tested source",
            "Upload effective tested source",
        ] {
            let steps = producer["steps"]
                .as_array()
                .context("source producer steps")?;
            let found = steps
                .iter()
                .filter(|item| item["name"].as_str() == Some(step))
                .collect::<Vec<_>>();
            ensure!(
                found.len() == 1
                    && found[0]["status"] == "completed"
                    && found[0]["conclusion"] == "success",
                "source production did not complete successfully: {step}"
            );
        }
        self.required_checks
            .iter()
            .map(|name| named(name))
            .collect()
    }
}

impl SourceReceipt {
    pub fn validate(&self, repo: &Path, binding: &CiBinding, run: &Value) -> Result<()> {
        binding.validate_run(run)?;
        oid(&self.source)?;
        oid(&self.tree)?;
        sha256(&self.producer_sha256)?;
        sha256(&self.pack_sha256)?;
        ensure!(
            self.version == 1
                && self.repository == binding.repository
                && self.run_id == binding.run.to_string()
                && self.run_attempt == binding.attempt.to_string()
                && self.producer_sha256 == binding.recipe_sha256
                && Some(self.event.as_str()) == run["event"].as_str()
                && self.pack_bytes > 0
                && self.pack_bytes <= 128 * 1024 * 1024,
            "source receipt provenance mismatch"
        );
        ensure!(
            matches!(
                (
                    self.object_format.as_str(),
                    self.source.len(),
                    self.tree.len()
                ),
                ("sha1", 40, 40) | ("sha256", 64, 64)
            ),
            "source receipt object format mismatch"
        );
        ensure!(
            tree(repo, &self.source)? == self.tree,
            "retained source/tree mismatch"
        );
        let recipe = git(
            repo,
            &[
                "show",
                &format!("{}:scripts/ci-evidence-source", self.source),
            ],
            None,
        )?;
        ensure!(
            hex::encode(Sha256::digest(recipe)) == binding.recipe_sha256,
            "retained source producer differs from the operator's pinned recipe"
        );
        let prefix = format!("github:{}#", binding.repository);
        match self.event.as_str() {
            "pull_request" => {
                let number = self
                    .change_id
                    .strip_prefix(&prefix)
                    .context("PR change identity")?;
                positive(number)?;
                // A platform PR entry independently binds the stable placeholder when present.
                // The source SHA deliberately need not equal run.head_sha (the author head).
                let pulls = run["pull_requests"]
                    .as_array()
                    .context("run PR identity list")?;
                if !pulls.is_empty() {
                    ensure!(
                        pulls.iter().any(|pull| pull["number"]
                            .as_u64()
                            .map(|n| n.to_string())
                            .as_deref()
                            == Some(number)),
                        "receipt names a different PR"
                    );
                }
            }
            "merge_group" => {
                ensure!(
                    run["head_sha"].as_str() == Some(self.source.as_str()),
                    "queue source mismatch"
                );
                let branch = run["head_branch"].as_str().context("queue head branch")?;
                let entry = branch
                    .strip_prefix("gh-readonly-queue/main/pr-")
                    .context("queue branch identity")?;
                let (number, suffix) = entry.split_once('-').context("queue entry identity")?;
                positive(number)?;
                oid(suffix)?;
                ensure!(
                    self.change_id == format!("{prefix}{number}"),
                    "queue change identity mismatch"
                );
            }
            "workflow_dispatch" => {
                ensure!(
                    self.change_id == format!("github:{}@{}", binding.repository, self.source)
                        && run["head_sha"].as_str() == Some(self.source.as_str()),
                    "manual run identity mismatch"
                );
            }
            _ => bail!("unsupported receipt event"),
        }
        Ok(())
    }
}

fn positive(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.as_bytes()[0] != b'0'
            && value.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid positive decimal identity"
    );
    Ok(())
}

/// Construct one record per configured check from independently fetched job bytes. These
/// describe observed GitHub outcomes, not a claim that this signer executed the tests itself.
pub fn ci_statements(
    repo: &Path,
    binding: &CiBinding,
    run: &Value,
    receipt: &SourceReceipt,
    jobs: &[Value],
) -> Result<Vec<Statement>> {
    receipt.validate(repo, binding, run)?;
    binding
        .validate_jobs(jobs)?
        .into_iter()
        .map(|job| {
            let name = job["name"].as_str().context("job name")?;
            let id = job["id"].as_u64().context("job ID")?;
            let conclusion = job["conclusion"]
                .as_str()
                .context("completed job conclusion")?;
            ensure!(
                matches!(
                    conclusion,
                    "success"
                        | "failure"
                        | "cancelled"
                        | "timed_out"
                        | "action_required"
                        | "neutral"
                        | "skipped"
                        | "startup_failure"
                ),
                "unknown job conclusion"
            );
            let observed_at = job["completed_at"]
                .as_str()
                .context("job completion time")?;
            chrono::DateTime::parse_from_rfc3339(observed_at)?;
            let observed_bytes = serde_json::to_vec(job)?;
            Ok(Statement {
                statement_type: STATEMENT_TYPE.into(),
                subject: vec![Subject {
                    name: receipt.source.clone(),
                    digest: BTreeMap::from([("gitTree".into(), receipt.tree.clone())]),
                }],
                predicate_type: PREDICATE_TYPE.into(),
                predicate: Observation {
                    schema_version: 1,
                    change_id: receipt.change_id.clone(),
                    signer: None,
                    tier: TrustTier::Degraded,
                    re_attests: None,
                    producer_sha256: String::new(),
                    observed_at: observed_at.into(),
                    fact: Fact::Tests {
                        runner: format!(
                            "github:{}/actions/runs/{}/attempts/{}/jobs/{id}",
                            binding.repository, binding.run, binding.attempt
                        ),
                        checks: BTreeMap::from([(
                            name.into(),
                            Check {
                                command: vec![
                                    "github-actions".into(),
                                    binding.repository.clone(),
                                    ".github/workflows/fleet.yml".into(),
                                    name.into(),
                                ],
                                exit_code: if conclusion == "success" { 0 } else { 1 },
                                output_sha256: hex::encode(Sha256::digest(observed_bytes)),
                            },
                        )]),
                        inputs: BTreeMap::new(),
                    },
                },
            })
        })
        .collect()
}
