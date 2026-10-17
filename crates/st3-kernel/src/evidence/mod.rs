//! Portable gate observations. Every immutable notes commit has its own ref and explicit trust tier.
//! No CLI accepts a caller-authored statement or signing key: protected observers construct
//! statements from authenticated observations and call `sign_observation` in their account.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub mod cli;
mod ci_reattest;
mod ci_transport;
pub mod observers;
#[cfg(test)]
mod tests;

pub const NOTES_PREFIX: &str = "refs/notes/evidence/";
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
pub const FRACTAL_PREDICATE_TYPE: &str = "https://compounding.tech/fractal/evidence/gate/v1";
// Permanent signed-history decoder exception: evidence predicate URI, #3032.
const LEGACY_PREDICATE_TYPE: &str = "https://smalltalk.dev/evidence/gate/v1";
// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-evidence-predicate-uri — DELETE at contraction — https://github.com/compoundingtech/smalltalk/issues/3032
// Keep observers writing the old URI until the accepting verifier release is on main.
pub const PREDICATE_TYPE: &str = LEGACY_PREDICATE_TYPE;
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-evidence-predicate-uri

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    #[serde(rename = "_type")]
    pub statement_type: String,
    pub subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    pub predicate: Observation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    /// The observed commit, for provenance; applicability is keyed to its tree.
    pub name: String,
    pub digest: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub schema_version: u32,
    pub change_id: String,
    pub signer: Option<String>,
    pub tier: TrustTier,
    /// A fresh full-trust rerun may cite its degraded predecessor; never mutate/promote it.
    pub re_attests: Option<String>,
    /// SHA-256 of the observer binary or script, measured by the observer itself.
    pub producer_sha256: String,
    pub observed_at: String,
    #[serde(rename = "observed")]
    pub fact: Fact,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Fact {
    Tests {
        runner: String,
        /// The observer records each required check, including failures, from its own run.
        checks: BTreeMap<String, Check>,
        /// Opaque future per-target input keys. v1 still requires exact-tree applicability.
        #[serde(default)]
        inputs: BTreeMap<String, String>,
    },
    Review {
        reviewer: String,
        verdict: Verdict,
        base_commit: String,
        /// Complete changed-path set against base. None records a deleted path.
        files: BTreeMap<String, Option<String>>,
        /// Immutable signed notes commit IDs of the tests the reviewer considered.
        tests: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub command: Vec<String>,
    pub exit_code: i32,
    pub output_sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    Full,
    Degraded,
    Test,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Verdict {
    GO,
    STOP,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub risk_class: String,
    pub required_checks: BTreeSet<String>,
    pub allowed_tiers: BTreeSet<TrustTier>,
    pub test_signers: BTreeSet<String>,
    pub review_signers: BTreeSet<String>,
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.risk_class == "daemon",
            "unsupported evidence policy version or risk class"
        );
        ensure!(
            self.allowed_tiers == BTreeSet::from([TrustTier::Full]),
            "daemon policy accepts only full trust; test keys never satisfy production policy"
        );
        ensure!(
            !self.required_checks.is_empty(),
            "policy must require at least one check"
        );
        ensure!(
            !self.test_signers.is_empty() && !self.review_signers.is_empty(),
            "policy must name test and review signers"
        );
        for value in self
            .required_checks
            .iter()
            .chain(&self.test_signers)
            .chain(&self.review_signers)
        {
            ensure!(!value.trim().is_empty(), "empty policy entry");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct Verified {
    pub tree: String,
    pub review: String,
    pub tests: Vec<String>,
    pub risk_class: String,
    pub tier: TrustTier,
}

/// All Git calls ignore replace objects and override signature executables/configuration.
fn git(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "gpg.format=ssh",
            "-c",
            "gpg.ssh.program=ssh-keygen",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run git")?;
    if let Some(bytes) = input {
        child.stdin.take().context("git stdin")?.write_all(bytes)?;
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn git_text(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<String> {
    Ok(String::from_utf8(git(repo, args, input)?)?
        .trim()
        .to_owned())
}

fn oid(value: &str) -> Result<()> {
    ensure!(
        (value.len() == 40 || value.len() == 64)
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected a full lowercase Git object ID"
    );
    Ok(())
}

fn sha256(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected a SHA-256 digest"
    );
    Ok(())
}

pub fn commit(repo: &Path, revision: &str) -> Result<String> {
    // End-of-options prevents a revision from being interpreted as a Git flag.
    let value = git_text(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
        None,
    )?;
    oid(&value)?;
    Ok(value)
}

pub fn tree(repo: &Path, revision: &str) -> Result<String> {
    let commit = commit(repo, revision)?;
    git_text(repo, &["rev-parse", &format!("{commit}^{{tree}}")], None)
}

pub fn changed_files(
    repo: &Path,
    base: &str,
    revision: &str,
) -> Result<BTreeMap<String, Option<String>>> {
    let base = commit(repo, base)?;
    let revision = commit(repo, revision)?;
    git(
        repo,
        &["merge-base", "--is-ancestor", &base, &revision],
        None,
    )
    .context("review base must be an ancestor of its target")?;
    let bytes = git(
        repo,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "-z",
            &base,
            &revision,
            "--",
        ],
        None,
    )?;
    let mut files = BTreeMap::new();
    for path in bytes.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8(path.to_vec()).context("review paths must be UTF-8")?;
        // ls-tree's literal pathspec handles spaces, punctuation, and glob-like filenames.
        let entries = git(
            repo,
            &[
                "--literal-pathspecs",
                "ls-tree",
                "-z",
                &revision,
                "--",
                &path,
            ],
            None,
        )?;
        let blob = if entries.is_empty() {
            None
        } else {
            let entry = String::from_utf8(entries)?;
            let (metadata, found) = entry
                .trim_end_matches('\0')
                .split_once('\t')
                .context("git tree entry")?;
            let parts: Vec<_> = metadata.split_whitespace().collect();
            ensure!(
                parts.len() == 3 && found == path && (parts[1] == "blob" || parts[1] == "commit"),
                "unsupported reviewed tree entry"
            );
            // Submodule entries use their Git commit hash as content identity.
            Some(parts[2].to_owned())
        };
        files.insert(path, blob);
    }
    Ok(files)
}

impl Statement {
    pub fn validate(&self, repo: &Path) -> Result<&str> {
        ensure!(
            self.statement_type == STATEMENT_TYPE
                && matches!(
                    self.predicate_type.as_str(),
                    FRACTAL_PREDICATE_TYPE | LEGACY_PREDICATE_TYPE
                ),
            "unsupported evidence format"
        );
        ensure!(
            self.subject.len() == 1,
            "evidence needs exactly one subject"
        );
        let subject = &self.subject[0];
        oid(&subject.name)?;
        ensure!(subject.digest.len() == 1, "unexpected subject digest");
        let observed_tree = subject
            .digest
            .get("gitTree")
            .context("missing gitTree digest")?;
        oid(observed_tree)?;
        ensure!(
            tree(repo, &subject.name)? == *observed_tree,
            "subject commit/tree mismatch"
        );
        let observation = &self.predicate;
        ensure!(
            observation.schema_version == 1,
            "unsupported observation schema version"
        );
        ensure!(
            !observation.change_id.trim().is_empty(),
            "missing change ID"
        );
        match (&observation.signer, observation.tier) {
            (None, TrustTier::Degraded) => {}
            (Some(signer), TrustTier::Full | TrustTier::Test) => {
                ensure!(
                    !signer.is_empty() && !signer.chars().any(char::is_whitespace),
                    "missing or invalid signer"
                );
            }
            _ => bail!(
                "full/test observations require a signer; degraded observations must be explicitly unsigned"
            ),
        }
        if let Some(prior) = &observation.re_attests {
            oid(prior)?;
            ensure!(
                observation.tier == TrustTier::Full
                    && matches!(observation.fact, Fact::Tests { .. }),
                "only a full-trust test rerun can re-attest"
            );
        }
        sha256(&observation.producer_sha256)?;
        chrono::DateTime::parse_from_rfc3339(&observation.observed_at)
            .context("invalid observation timestamp")?;
        match &observation.fact {
            Fact::Tests {
                runner,
                checks,
                inputs,
            } => {
                ensure!(
                    !runner.trim().is_empty() && !checks.is_empty(),
                    "missing runner or checks"
                );
                ensure!(
                    inputs
                        .iter()
                        .all(|(key, value)| !key.trim().is_empty() && !value.trim().is_empty()),
                    "empty test input key or digest"
                );
                for (name, check) in checks {
                    ensure!(
                        !name.trim().is_empty()
                            && !check.command.is_empty()
                            && !check.command[0].is_empty(),
                        "empty check or command"
                    );
                    sha256(&check.output_sha256)?;
                }
            }
            Fact::Review {
                reviewer,
                base_commit,
                files,
                tests,
                ..
            } => {
                ensure!(!reviewer.trim().is_empty(), "missing reviewer");
                oid(base_commit)?;
                ensure!(
                    changed_files(repo, base_commit, &subject.name)? == *files,
                    "review must cover every changed file with its exact blob hash"
                );
                ensure!(!tests.is_empty(), "review must cite tests");
                let mut unique = BTreeSet::new();
                for test in tests {
                    oid(test)?;
                    ensure!(unique.insert(test), "duplicate test citation");
                }
            }
        }
        Ok(observed_tree)
    }
}

/// Protected observer API. The caller must own the signing key and construct `statement`
/// from facts it measured itself. This is deliberately unavailable as an agent CLI verb.
/// The signed commit contains the note and retains the subject as its only parent, so
/// pushing its ref carries the observed source objects as well as the signed statement.
pub fn sign_observation(repo: &Path, statement: &Statement, ssh_key: &Path) -> Result<String> {
    ensure!(
        statement.predicate.tier != TrustTier::Degraded,
        "degraded observations are unsigned"
    );
    write_observation(repo, statement, Some(ssh_key))
}

/// Fail-soft producer API: preserve measured facts when signing is unavailable. This
/// creates explicitly degraded unsigned evidence and grants no production gate authority.
pub fn record_degraded(repo: &Path, statement: &Statement) -> Result<String> {
    ensure!(
        statement.predicate.tier == TrustTier::Degraded && statement.predicate.signer.is_none(),
        "degraded producer requires explicit unsigned tier"
    );
    write_observation(repo, statement, None)
}

/// The running process has one producer identity. Measure it once, from the actual
/// executable image on Linux; trust-list and evidence verification are never cached.
/// Concurrent first callers wait for the one measurement instead of each hashing the image.
fn producer_digest() -> Result<&'static str> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    static DIGEST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    static MEASURING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    if let Some(value) = DIGEST.get() {
        return Ok(value);
    }
    let _measuring = MEASURING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(value) = DIGEST.get() {
        return Ok(value);
    }
    #[cfg(test)]
    MEASUREMENTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    #[cfg(target_os = "linux")]
    let path = PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let path = std::env::current_exe()?;
    let mut executable = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = executable.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let _ = DIGEST.set(hex::encode(hash.finalize()));
    DIGEST
        .get()
        .map(String::as_str)
        .context("measured observer digest")
}

/// How many times this process hashed its own image; the single-flight test pins it at one.
#[cfg(test)]
static MEASUREMENTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn write_observation(repo: &Path, statement: &Statement, ssh_key: Option<&Path>) -> Result<String> {
    let mut statement = statement.clone();
    statement.predicate.producer_sha256 = producer_digest()?.to_owned();
    let observed_tree = statement.validate(repo)?;
    let bytes = serde_json::to_vec(&statement)?;
    let blob = git_text(repo, &["hash-object", "-w", "--stdin"], Some(&bytes))?;
    let notes_tree = git_text(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\t{observed_tree}\n").as_bytes()),
    )?;
    let mut args = vec![
        "-c",
        "user.name=Evidence observer",
        "-c",
        "user.email=observer@example.invalid",
    ];
    let key_config;
    if let Some(key) = ssh_key {
        key_config = format!(
            "user.signingKey={}",
            key.to_str().context("SSH key path must be UTF-8")?
        );
        args.extend(["-c", &key_config]);
    }
    args.push("commit-tree");
    if ssh_key.is_some() {
        args.push("-S");
    }
    args.extend([&notes_tree, "-p", &statement.subject[0].name]);
    let signed = git_text(repo, &args, Some(b"st gate observation v1\n"))?;
    oid(&signed)?;
    Ok(signed)
}

/// Read the sole note from an immutable signed observation commit. No trust is implied.
pub fn show(repo: &Path, id: &str) -> Result<Statement> {
    oid(id)?;
    let entries = git(repo, &["ls-tree", "-r", "-z", id], None)?;
    let entries: Vec<_> = entries
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    ensure!(
        entries.len() == 1,
        "evidence commit must contain exactly one note"
    );
    let entry = std::str::from_utf8(entries[0])?;
    let (metadata, path) = entry.split_once('\t').context("notes tree entry")?;
    let parts: Vec<_> = metadata.split_whitespace().collect();
    ensure!(
        parts.len() == 3 && parts[0] == "100644" && parts[1] == "blob",
        "invalid evidence note mode/type"
    );
    oid(path)?;
    oid(parts[2])?;
    let statement: Statement =
        serde_json::from_slice(&git(repo, &["cat-file", "blob", parts[2]], None)?)
            .context("decode evidence statement")?;
    ensure!(
        statement.validate(repo)? == path,
        "note attached to wrong tree"
    );
    let parents = git_text(repo, &["show", "-s", "--format=%P", id], None)?;
    ensure!(
        parents == statement.subject[0].name,
        "observation must have exactly its subject commit as parent"
    );
    Ok(statement)
}

/// Verify using only repository objects and an OpenSSH allowed-signers file. Never use
/// the repository's trust list. The statement's signer must equal the verified principal.
pub fn verify_record(repo: &Path, id: &str, trusted_signers: &Path) -> Result<Statement> {
    let statement = show(repo, id)?;
    if statement.predicate.tier == TrustTier::Degraded {
        let status = git_text(repo, &["show", "-s", "--format=%G?", id], None)?;
        ensure!(status == "N", "degraded record must be unsigned");
        return Ok(statement);
    }
    let signers = std::fs::canonicalize(trusted_signers).context("read trusted signer list")?;
    let signers = signers.to_str().context("signer path must be UTF-8")?;
    let config = format!("gpg.ssh.allowedSignersFile={signers}");
    // An empty revocation file overrides repository config without granting extra trust.
    let revocations = tempfile::NamedTempFile::new()?;
    let revocation_config = format!("gpg.ssh.revocationFile={}", revocations.path().display());
    git(
        repo,
        &["-c", &config, "-c", &revocation_config, "verify-commit", id],
        None,
    )?;
    let identity = git_text(
        repo,
        &[
            "-c",
            &config,
            "-c",
            &revocation_config,
            "show",
            "-s",
            "--format=%G?%x00%GS",
            id,
        ],
        None,
    )?;
    let (status, signer) = identity
        .split_once('\0')
        .context("SSH signature identity")?;
    ensure!(
        status == "G" && Some(signer) == statement.predicate.signer.as_deref(),
        "signature principal does not match record signer"
    );
    if let Some(prior_id) = &statement.predicate.re_attests {
        let prior = show(repo, prior_id)?;
        ensure!(
            prior.predicate.tier == TrustTier::Degraded,
            "re-attestation must cite degraded evidence"
        );
        verify_record(repo, prior_id, trusted_signers)?;
        ensure!(
            prior.predicate.change_id == statement.predicate.change_id
                && prior.subject[0].digest == statement.subject[0].digest,
            "re-attestation changed its keyed subject"
        );
        let (
            Fact::Tests {
                checks: old,
                inputs: old_inputs,
                ..
            },
            Fact::Tests {
                checks: new,
                inputs: new_inputs,
                ..
            },
        ) = (&prior.predicate.fact, &statement.predicate.fact)
        else {
            bail!("re-attestation must rerun tests")
        };
        let inputs = |checks: &BTreeMap<String, Check>| {
            checks
                .iter()
                .map(|(name, check)| (name.clone(), check.command.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        ensure!(
            old_inputs == new_inputs && inputs(old) == inputs(new),
            "re-attestation changed its keyed checks"
        );
        let before = chrono::DateTime::parse_from_rfc3339(&prior.predicate.observed_at)?;
        let after = chrono::DateTime::parse_from_rfc3339(&statement.predicate.observed_at)?;
        ensure!(
            after >= before,
            "re-attestation predates original observation"
        );
    }
    Ok(statement)
}

/// Validate first, then create the immutable ref. Distinct writers never share a ref;
/// importing the same record twice is idempotent, including simultaneous imports.
pub fn put(repo: &Path, id: &str, trusted_signers: &Path) -> Result<String> {
    verify_record(repo, id, trusted_signers)?;
    let name = format!("{NOTES_PREFIX}{id}");
    let existing = git_text(
        repo,
        &["for-each-ref", "--format=%(objectname)", &name],
        None,
    )?;
    if !existing.is_empty() {
        ensure!(existing == id, "evidence ref was replaced");
        return Ok(name);
    }
    // update-ref's transaction verifies absence, rather than overwriting another writer.
    match git(
        repo,
        &["update-ref", &name, id, &"0".repeat(id.len())],
        None,
    ) {
        Ok(_) => Ok(name),
        Err(error) => {
            let current = git_text(
                repo,
                &["for-each-ref", "--format=%(objectname)", &name],
                None,
            )?;
            if current == id { Ok(name) } else { Err(error) }
        }
    }
}

fn records(repo: &Path) -> Result<Vec<String>> {
    let refs = git_text(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            NOTES_PREFIX,
        ],
        None,
    )?;
    let mut records = Vec::new();
    for line in refs.lines() {
        let (name, value) = line.split_once(' ').context("evidence ref")?;
        let id = name
            .strip_prefix(NOTES_PREFIX)
            .context("evidence namespace")?;
        oid(id)?;
        ensure!(id == value, "evidence ref is not immutable: {name}");
        records.push(id.to_owned());
    }
    Ok(records)
}

/// Require both checks and a GO whose complete changed-path set and cited signed tests
/// still apply. A changed tree cannot reuse v1 tests; scoped test reuse is a later version.
pub fn verify(
    repo: &Path,
    revision: &str,
    base: &str,
    change_id: &str,
    trusted_signers: &Path,
    policy: &Policy,
) -> Result<Verified> {
    policy.validate()?;
    let revision = commit(repo, revision)?;
    let target_tree = tree(repo, &revision)?;
    let files = changed_files(repo, base, &revision)?;
    let mut valid = BTreeMap::new();
    for id in records(repo)? {
        // Invalid/untrusted records cannot supply evidence, nor poison unrelated valid ones.
        if let Ok(statement) = verify_record(repo, &id, trusted_signers)
            && statement.predicate.change_id == change_id
        {
            valid.insert(id, statement);
        }
    }
    // A newer applicable STOP supersedes a GO; an older passing review cannot hide
    // a newer review with missing/failed citations. STOP wins equal-time ties.
    let latest = valid
        .iter()
        .filter_map(|(id, statement)| {
            let observation = &statement.predicate;
            if observation.tier != TrustTier::Full
                || !observation
                    .signer
                    .as_ref()
                    .is_some_and(|signer| policy.review_signers.contains(signer))
            {
                return None;
            }
            let Fact::Review {
                verdict,
                files: reviewed,
                ..
            } = &observation.fact
            else {
                return None;
            };
            if *reviewed != files {
                return None;
            }
            let time = chrono::DateTime::parse_from_rfc3339(&observation.observed_at).ok()?;
            Some(((time, *verdict == Verdict::STOP, id), statement))
        })
        .max_by(|a, b| a.0.cmp(&b.0));
    if let Some(((_, _, review_id), statement)) = latest {
        let Fact::Review { verdict, tests, .. } = &statement.predicate.fact else {
            unreachable!()
        };
        ensure!(
            *verdict == Verdict::GO,
            "latest applicable trusted review is STOP"
        );
        let mut passed = BTreeSet::new();
        for id in tests {
            let test = valid
                .get(id)
                .context("review cites missing or untrusted tests")?;
            ensure!(
                policy.allowed_tiers.contains(&test.predicate.tier)
                    && test
                        .predicate
                        .signer
                        .as_ref()
                        .is_some_and(|signer| policy.test_signers.contains(signer))
                    && test.subject[0].digest["gitTree"] == target_tree,
                "cited tests do not apply to this tree, signer policy, or full trust tier"
            );
            let Fact::Tests { checks, .. } = &test.predicate.fact else {
                bail!("review citation is not tests")
            };
            ensure!(
                checks.values().all(|check| check.exit_code == 0),
                "cited checks failed"
            );
            passed.extend(checks.keys().cloned());
        }
        ensure!(
            policy.required_checks.is_subset(&passed),
            "required checks missing from cited tests"
        );
        return Ok(Verified {
            tree: target_tree,
            review: review_id.clone(),
            tests: tests.clone(),
            risk_class: policy.risk_class.clone(),
            tier: TrustTier::Full,
        });
    }
    bail!(
        "required evidence missing or inapplicable: need a trusted GO covering current files and unchanged cited tests passing {:?}",
        policy.required_checks
    )
}

/// Read policy from an accepted base commit, never the change being judged. The caller
/// pins its accepted base; trust roots remain external to repository policy.
pub fn load_policy(repo: &Path, revision: &str) -> Result<Policy> {
    let revision = commit(repo, revision)?;
    let policy = serde_json::from_slice(&git(
        repo,
        &["show", &format!("{revision}:.st/evidence-policy.json")],
        None,
    )?)?;
    Ok(policy)
}
