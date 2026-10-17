//! Selector-only CI entry point for a protected profile. Configuration is in its private HOME.
//! The profile's PATH supplies installed Git, gh, Python and timeout; never checkout tools.
use super::{observers::*, *};
use base64::Engine as _;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    repository: String,
    scratch_repo: PathBuf,
    recipe_sha256: String,
    accepted_base: String,
    workflow_sha256: String,
    required_checks: BTreeSet<String>,
    /// Configured by the profile operator. The seat's CLI selectors cannot change it.
    tier: TrustTier,
    signer: Option<String>,
    ssh_key: Option<PathBuf>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

struct Github {
    repository: String,
}

impl Github {
    fn command(&self, suffix: &str) -> Command {
        let mut command = Command::new("timeout");
        command.args([
            "--kill-after=2s",
            "30s",
            "gh",
            "api",
            "--hostname",
            "github.com",
            &format!("repos/{}/{suffix}", self.repository),
        ]);
        command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("GH_DEBUG");
        command
    }

    fn json(&self, suffix: &str) -> Result<Value> {
        let output = self.command(suffix).output().context("contact GitHub")?;
        ensure!(output.status.success(), "GitHub observation unavailable");
        ensure!(
            output.stdout.len() <= 4 * 1024 * 1024,
            "GitHub metadata size bound"
        );
        serde_json::from_slice(&output.stdout).context("GitHub observation JSON")
    }

    fn jobs(&self, run: u64, attempt: u64) -> Result<Vec<Value>> {
        let mut jobs = Vec::new();
        for page in 1..=10 {
            let response = self.json(&format!(
                "actions/runs/{run}/attempts/{attempt}/jobs?per_page=100&page={page}"
            ))?;
            let batch = response["jobs"].as_array().context("GitHub jobs list")?;
            jobs.extend(batch.iter().cloned());
            if batch.len() < 100 {
                return Ok(jobs);
            }
        }
        bail!("CI jobs exceed observer pagination bound")
    }
}

// Only two regular files, no extraction of names supplied by an archive. All ZIP bytes are
// authenticated before parsing; limits precede decompression and index-pack sees only pack bytes.
const EXTRACT: &str = r#"
import hashlib, json, pathlib, shutil, stat, sys, zipfile
archive, destination = map(pathlib.Path, sys.argv[1:3])
size, expected = int(sys.argv[3]), sys.argv[4]
if archive.stat().st_size != size:
    raise ValueError('source ZIP byte length mismatch')
with archive.open('rb') as stream:
    if hashlib.file_digest(stream, 'sha256').hexdigest() != expected:
        raise ValueError('source ZIP digest mismatch')
with zipfile.ZipFile(archive) as zipped:
    entries = zipped.infolist()
    if len(entries) != 2 or {entry.filename for entry in entries} != {'receipt.json', 'source.pack'}:
        raise ValueError('source ZIP member set mismatch')
    for entry in entries:
        mode = entry.external_attr >> 16
        if entry.is_dir() or stat.S_IFMT(mode) not in (0, stat.S_IFREG) or entry.flag_bits & 1:
            raise ValueError('source ZIP member type mismatch')
        limit = 65536 if entry.filename == 'receipt.json' else 128 * 1024 * 1024
        if entry.file_size > limit:
            raise ValueError('source ZIP member exceeds bound')
    destination.mkdir(mode=0o700, exist_ok=False)
    for name in ('receipt.json', 'source.pack'):
        with zipped.open(name) as source, (destination / name).open('xb') as target:
            shutil.copyfileobj(source, target, length=65536)
"#;

/// Numeric selectors only. No caller-provided repository, path, verdict, statement or key.
pub fn observe_ci(run: u64, attempt: u64, artifact: u64, predecessors: &[String]) -> Result<Value> {
    for id in predecessors {
        oid(id)?;
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("protected profile HOME")?);
    let directory = st3_client::product_dirs::resolve(
        home.join(".config/st"),
        home.join(".config/fractal"),
    );
    let configuration: Configuration = serde_json::from_slice(&std::fs::read(
        directory.join("evidence-ci.json"),
    )?)
    .context("private CI observer configuration")?;
    ensure!(
        configuration.scratch_repo.is_absolute(),
        "private scratch path must be absolute"
    );
    if let Some(key) = &configuration.ssh_key {
        ensure!(key.is_absolute(), "private key path must be absolute");
    }
    let signer = match configuration.tier {
        TrustTier::Full => {
            ensure!(
                configuration.expires_at.is_none(),
                "production signer cannot be a test service"
            );
            ObserverSigner::Full {
                principal: configuration
                    .signer
                    .as_deref()
                    .context("configured signing principal")?,
                key: configuration
                    .ssh_key
                    .as_deref()
                    .context("configured signing key")?,
            }
        }
        TrustTier::Test => ObserverSigner::Test {
            principal: configuration
                .signer
                .as_deref()
                .context("configured test principal")?,
            key: configuration
                .ssh_key
                .as_deref()
                .context("configured throwaway key")?,
            expires_at: configuration
                .expires_at
                .context("throwaway service expiration")?,
        },
        TrustTier::Degraded => {
            ensure!(
                configuration.signer.is_none()
                    && configuration.ssh_key.is_none()
                    && configuration.expires_at.is_none(),
                "degraded profile has no signer or key"
            );
            ObserverSigner::Unavailable
        }
    };
    if let ObserverSigner::Full { principal, .. } | ObserverSigner::Test { principal, .. } = &signer
    {
        ensure!(
            !principal.is_empty() && !principal.chars().any(char::is_whitespace),
            "invalid configured signer principal"
        );
    }
    let binding = CiBinding {
        repository: configuration.repository.clone(),
        run,
        attempt,
        recipe_sha256: configuration.recipe_sha256,
        accepted_base: configuration.accepted_base,
        workflow_sha256: configuration.workflow_sha256,
        required_checks: configuration.required_checks,
    };
    binding.validate()?;
    ensure!(
        predecessors.len() <= binding.required_checks.len(),
        "too many predecessor selectors"
    );
    let repo = &configuration.scratch_repo;
    ensure!(
        git_text(repo, &["rev-parse", "--is-bare-repository"], None)? == "true",
        "observer requires its private bare repository"
    );
    let github = Github {
        repository: binding.repository.clone(),
    };
    // Attempt-specific metadata stays authoritative even after another rerun begins.
    let observed_run = github.json(&format!("actions/runs/{run}/attempts/{attempt}"))?;
    binding.validate_run(&observed_run)?;
    let jobs = github.jobs(run, attempt)?;
    binding.validate_jobs(&jobs)?;
    let metadata = github.json(&format!("actions/artifacts/{artifact}"))?;
    let (size, hash) = binding.validate_artifact(&metadata, artifact)?;
    let scratch = tempfile::tempdir_in(repo).context("private source transfer directory")?;
    let archive = scratch.path().join("source.zip");
    let stream = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&archive)?;
    let status = github
        .command(&format!("actions/artifacts/{artifact}/zip"))
        .stdout(Stdio::from(stream))
        .status()?;
    ensure!(status.success(), "source artifact transfer unavailable");
    let destination = scratch.path().join("verified");
    let status = Command::new("python3")
        .args(["-c", EXTRACT])
        .arg(&archive)
        .arg(&destination)
        .arg(size.to_string())
        .arg(hash)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(status.success(), "source artifact verification failed");
    let receipt: SourceReceipt =
        serde_json::from_slice(&std::fs::read(destination.join("receipt.json"))?)?;
    let pack = std::fs::read(destination.join("source.pack"))?;
    ensure!(
        receipt.pack_bytes == pack.len() as u64
            && receipt.pack_sha256 == hex::encode(sha2::Sha256::digest(&pack)),
        "retained pack byte length/digest mismatch"
    );
    // Acquire the accepted baseline and platform head with their parents before strict
    // import. Source retention packs intentionally contain no history. For PR merge
    // artifacts the fixed main branch supplies the base history; they remain degraded.
    let platform_head = observed_run["head_sha"]
        .as_str()
        .context("platform run head")?;
    oid(platform_head)?;
    fetch_source(
        repo,
        &binding.repository,
        &[&binding.accepted_base, platform_head, "main"],
    )?;
    git(
        repo,
        &[
            "-c",
            "transfer.fsckObjects=true",
            "index-pack",
            "--strict",
            "--stdin",
        ],
        Some(&pack),
    )?;
    receipt.validate(repo, &binding, &observed_run)?;
    let full_eligible = binding.full_eligible(repo, &receipt, &observed_run)?;
    let effective_signer = if matches!(signer, ObserverSigner::Full { .. }) && !full_eligible {
        &ObserverSigner::Unavailable
    } else {
        &signer
    };
    let mut old_jobs = Vec::new();
    if !predecessors.is_empty() {
        let refs = predecessors
            .iter()
            .map(|id| format!("{NOTES_PREFIX}{id}"))
            .collect::<Vec<_>>();
        fetch_source(
            repo,
            &binding.repository,
            &refs.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
        let mut attempts = BTreeMap::<(u64, u64), Vec<(String, u64)>>::new();
        for id in predecessors {
            let old = ci_reattest::prior(repo, &binding.repository, id)?;
            let Fact::Tests { runner, .. } = old.predicate.fact else {
                unreachable!()
            };
            let (run, attempt, job) = ci_reattest::identity(&binding.repository, &runner)?;
            attempts
                .entry((run, attempt))
                .or_default()
                .push((id.clone(), job));
        }
        // One immutable provider read per distinct old attempt, shared by its selected jobs.
        for ((run, attempt), selected) in attempts {
            let old_run = github.json(&format!("actions/runs/{run}/attempts/{attempt}"))?;
            let mut old_binding = binding.clone();
            old_binding.run = run;
            old_binding.attempt = attempt;
            old_binding.validate_run(&old_run)?;
            let fetched = github.jobs(run, attempt)?;
            for (id, job) in selected {
                let matching = fetched
                    .iter()
                    .filter(|value| value["id"].as_u64() == Some(job))
                    .collect::<Vec<_>>();
                ensure!(matching.len() == 1, "missing or ambiguous predecessor job");
                old_jobs.push((id, old_run.clone(), matching[0].clone()));
            }
        }
    }
    let current = ci_statements(repo, &binding, &observed_run, &receipt, &jobs)?;
    let links = ci_reattest::prepare(repo, &binding.repository, &current, &jobs, &old_jobs)?;
    let produced = produce_ci_linked(
        repo,
        &binding,
        &observed_run,
        &receipt,
        &jobs,
        effective_signer,
        &links,
    )?;
    let re_attestations = ci_reattest::measurements(repo, &produced)?;
    let re_attestation_elapsed_ms = re_attestations
        .iter()
        .filter_map(|value| value["elapsed_ms"].as_u64())
        .max();
    // Export data only. The caller imports it with index-pack and verifies/puts each record;
    // the private profile need not write the caller's read-only checkout or expose its key.
    let mut objects = format!("{}\n", receipt.source).into_bytes();
    objects.extend(git(
        repo,
        &["rev-list", "--objects", "--no-object-names", &receipt.tree],
        None,
    )?);
    let mut exported_records = produced
        .iter()
        .map(|observation| observation.record.clone())
        .collect::<BTreeSet<_>>();
    for observation in &produced {
        if let Some(prior) = show(repo, &observation.record)?.predicate.re_attests {
            exported_records.insert(prior);
        }
    }
    for record in exported_records {
        let statement = show(repo, &record)?;
        objects.extend(format!("{}\n", statement.subject[0].name).as_bytes());
        objects.extend(format!("{record}\n").as_bytes());
        objects.extend(git(
            repo,
            &[
                "rev-list",
                "--objects",
                "--no-object-names",
                &format!("{record}^{{tree}}"),
            ],
            None,
        )?);
    }
    let object_pack = git(repo, &["pack-objects", "--stdout"], Some(&objects))?;
    let fresh = produced
        .iter()
        .filter(|result| result.newly_recorded)
        .collect::<Vec<_>>();
    let full = fresh
        .iter()
        .filter(|result| result.tier == TrustTier::Full)
        .count();
    let signing_latency_ms = fresh
        .iter()
        .map(|result| result.signing_latency_ms)
        .sum::<u128>();
    Ok(serde_json::json!({
        "repository":binding.repository, "run":run, "attempt":attempt,
        "source":receipt.source, "tree":receipt.tree,
        "full_trust_eligible":full_eligible,
        "trust_limitation":(!full_eligible).then_some("PR source is self-reported or retained workflow differs from accepted pin"),
        "records":produced, "re_attestations":re_attestations,
        "object_pack_base64":base64::engine::general_purpose::STANDARD.encode(object_pack),
        "metrics":{
            "fresh_records":fresh.len(), "full_records":full,
            "full_trust_share":(!fresh.is_empty()).then(|| full as f64 / fresh.len() as f64),
            "signing_latency_ms":signing_latency_ms,
            "original_ci_signing_latency_ms":0,
            "re_attested_records":re_attestations.len(),
            "re_attestation_elapsed_ms":re_attestation_elapsed_ms
        }
    }))
}

use sha2::Digest as _;

/// Fixed public repository and validated revisions only, under the installed profile PATH.
fn fetch_source(repo: &Path, repository: &str, revisions: &[&str]) -> Result<()> {
    let status = Command::new("timeout")
        .args(["--kill-after=2s", "30s", "git", "-C"])
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "transfer.fsckObjects=true",
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            "--no-write-fetch-head",
            "--",
        ])
        .arg(format!("https://github.com/{repository}.git"))
        .args(revisions)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(
        status.success(),
        "accepted baseline/platform source unavailable"
    );
    Ok(())
}
