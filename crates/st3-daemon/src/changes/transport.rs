//! Exact-OID Git transactions. Network failure supplies no claim ownership; callers
//! reconcile the candidate record/nonce before resuming. No GitHub API is used.
use super::*;
use anyhow::{Context, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Credentials/SSH commands are supplied by the operator, never by a checkout.
/// The daemon adapter must derive these from its trusted service configuration.
#[derive(Clone, Default)]
pub struct Authentication {
    pub helper: Option<String>,
    pub ssh_command: Option<String>,
}

#[derive(Clone)]
pub struct Repository {
    root: PathBuf,
    authentication: Authentication,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Advertisement {
    pub observed_at: String,
    pub refs: BTreeMap<String, String>,
}

impl Repository {
    pub fn open(root: &Path, authentication: Authentication) -> Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let this = Self {
            root,
            authentication,
        };
        this.text(&["rev-parse", "--git-dir"], None)?;
        Ok(this)
    }

    fn reset_keys(&self) -> Result<Vec<String>> {
        // Reading configuration invokes no helper. Reset helpers and authentication
        // headers in every scope, including included host-qualified configuration.
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args([
                "config",
                "--includes",
                "--name-only",
                "--get-regexp",
                "^(credential(\\..*)?\\.helper|http(\\..*)?\\.(extraheader|followredirects))$",
            ])
            .env_remove("GIT_DIR")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_NAMESPACE")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .output()?;
        ensure!(
            output.status.success() || output.status.code() == Some(1),
            "cannot read Git credential configuration"
        );
        Ok(String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    fn command(&self) -> Result<Command> {
        let mut command = Command::new("git");
        command
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(&self.root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.askPass=/bin/false",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "push.followTags=false",
                "-c",
                "push.recurseSubmodules=no",
                "-c",
                "fetch.recurseSubmodules=false",
                "-c",
                "credential.helper=",
                "-c",
                "credential.https://github.com.helper=",
                "-c",
                "http.extraHeader=",
                "-c",
                "http.followRedirects=false",
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.http.allow=always",
                "-c",
                "protocol.https.allow=always",
                "-c",
                "protocol.ssh.allow=always",
                "-c",
                "protocol.file.allow=always",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "core.sshCommand=ssh",
                "-c",
                "user.name=st",
                "-c",
                "user.email=changes@example.invalid",
            ]);
        for key in self.reset_keys()? {
            let value = if key.ends_with(".followredirects") {
                "false"
            } else {
                ""
            };
            command.arg("-c").arg(format!("{key}={value}"));
        }
        if let Some(helper) = &self.authentication.helper {
            command.arg("-c").arg(format!("credential.helper={helper}"));
            command
                .arg("-c")
                .arg(format!("credential.https://github.com.helper={helper}"));
        }
        if let Some(ssh) = &self.authentication.ssh_command {
            command.arg("-c").arg(format!("core.sshCommand={ssh}"));
        }
        // Worktree/environment overrides must not redirect the selected object store.
        for key in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_NAMESPACE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
        ] {
            command.env_remove(key);
        }
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("GIT_ASKPASS", "/bin/false");
        command.env("SSH_ASKPASS", "/bin/false");
        command.env("SSH_ASKPASS_REQUIRE", "never");
        command.process_group(0);
        Ok(command)
    }

    fn git(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let mut command = self.command()?;
        command
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::from(stdout.try_clone()?))
            .stderr(Stdio::from(stderr.try_clone()?));
        let mut child = command.spawn().context("run git for change transport")?;
        if let Some(input) = input {
            let result = child.stdin.take().context("git stdin")?.write_all(input);
            if let Err(error) = result {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                return Err(error.into());
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if std::time::Instant::now() >= deadline
                || stdout.metadata()?.len() > 16 * 1024 * 1024
                || stderr.metadata()?.len() > 16 * 1024 * 1024
            {
                // Kill helpers in the same process group too. A timed-out push has
                // an uncertain remote result, which the caller must reconcile.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                bail!(
                    "Git operation exceeded transport time/output budget; reconcile uncertain writes"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        ensure!(
            stdout.metadata()?.len() <= 16 * 1024 * 1024
                && stderr.metadata()?.len() <= 16 * 1024 * 1024,
            "Git output exceeds budget"
        );
        stdout.seek(SeekFrom::Start(0))?;
        stderr.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes)?;
        let mut diagnostic = String::new();
        stderr.take(4096).read_to_string(&mut diagnostic)?;
        ensure!(
            status.success(),
            "git {}: {}",
            args.first().unwrap_or(&""),
            diagnostic.trim()
        );
        Ok(bytes)
    }

    fn text(&self, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        Ok(String::from_utf8(self.git(args, input)?)?.trim().to_owned())
    }

    /// One patterned discovery only. Object fetching is a separate synchronization operation.
    pub fn discover(&self, remote: &str) -> Result<Advertisement> {
        let remote = self.remote(remote)?;
        let raw = self.text(
            &["ls-remote", "--refs", &remote, "refs/st/changes/v1/*"],
            None,
        )?;
        let mut refs = BTreeMap::new();
        for line in raw.lines() {
            let (target, name) = line
                .split_once('\t')
                .context("malformed Git advertisement")?;
            oid(target)?;
            ensure!(
                name.starts_with(PREFIX),
                "unexpected ref outside requested namespace"
            );
            ensure!(
                refs.insert(name.into(), target.into()).is_none(),
                "duplicate advertised ref"
            );
        }
        Ok(Advertisement {
            observed_at: chrono::Utc::now().to_rfc3339(),
            refs,
        })
    }

    /// Explicit fetch into an isolated tracking namespace, not the writer's active refs.
    pub fn sync(&self, remote: &str) -> Result<()> {
        let remote = self.remote(remote)?;
        let identity = hex::encode(sha2::Sha256::digest(remote.as_bytes()));
        self.git(
            &[
                "fetch",
                "--no-tags",
                &remote,
                &format!("+{PREFIX}*:refs/st/tracking/{identity}/*"),
            ],
            None,
        )?;
        Ok(())
    }

    fn remote(&self, name: &str) -> Result<String> {
        ensure!(!name.is_empty() && !name.starts_with('-'), "invalid remote");
        let value = self.text(&["config", "--get", &format!("remote.{name}.url")], None)?;
        ensure!(
            !value.is_empty() && !value.starts_with('-') && !value.contains('\n'),
            "invalid remote URL"
        );
        // Reject an active rewrite before credentials can reach another destination.
        // NUL-delimited configuration keeps values with spaces/newlines unambiguous.
        let configuration = self.git(&["config", "--includes", "--null", "--list"], None)?;
        for entry in configuration.split(|byte| *byte == 0) {
            let Some(split) = entry.iter().position(|byte| *byte == b'\n') else {
                continue;
            };
            let key = std::str::from_utf8(&entry[..split])?;
            if key.starts_with("url.")
                && (key.ends_with(".insteadof") || key.ends_with(".pushinsteadof"))
            {
                let prefix = std::str::from_utf8(&entry[split + 1..])?;
                ensure!(
                    !value.starts_with(prefix),
                    "remote URL rewrite is not permitted"
                );
            }
        }
        // ext/helper transports execute checkout-supplied commands and are not permitted.
        let helper_transport = value.split_once("::").is_some_and(|(prefix, _)| {
            !prefix.is_empty()
                && prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b))
        });
        ensure!(
            !helper_transport,
            "remote helper transports are not supported"
        );
        if value.contains("://") {
            let url = reqwest::Url::parse(&value)?;
            ensure!(
                matches!(url.scheme(), "https" | "http" | "ssh" | "file")
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "unsupported or credential-bearing remote URL"
            );
            ensure!(
                url.scheme() == "ssh" || url.username().is_empty(),
                "credential-bearing remote URL"
            );
        } else {
            ensure!(
                Path::new(&value).is_absolute() || value.contains(':'),
                "expected absolute local path or SSH remote"
            );
        }
        Ok(value)
    }

    /// Enumerate only the isolated, URL-keyed tracking namespace with the same
    /// environment isolation and output bounds as other local Git reads.
    pub fn tracking_inventory(&self, remote_url: &str) -> Result<String> {
        let namespace = format!(
            "refs/st/tracking/{}/",
            hex::encode(sha2::Sha256::digest(remote_url.as_bytes()))
        );
        self.text(
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                &namespace,
            ],
            None,
        )
    }

    pub fn source(&self, revision: &str) -> Result<(String, String)> {
        ensure!(
            !revision.is_empty() && !revision.starts_with('-'),
            "invalid source revision"
        );
        let source = self.text(
            &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
            None,
        )?;
        oid(&source)?;
        let tree = self.text(&["rev-parse", &format!("{source}^{{tree}}")], None)?;
        oid(&tree)?;
        Ok((source, tree))
    }

    pub fn trailer(&self, revision: &str) -> Result<Option<String>> {
        let (source, _) = self.source(revision)?;
        let message = self.git(&["show", "-s", "--format=%B", &source], None)?;
        let trailers = self.text(&["interpret-trailers", "--parse"], Some(&message))?;
        let mut found = None;
        for line in trailers.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if key.eq_ignore_ascii_case("Change-Id") {
                ensure!(
                    key == "Change-Id" && found.is_none(),
                    "duplicate or noncanonical Change-Id trailer"
                );
                change_id(value.trim())?;
                found = Some(value.trim().to_owned());
            }
        }
        Ok(found)
    }

    pub fn load(&self, object: &str) -> Result<Revision> {
        oid(object)?;
        let entries = self.text(&["ls-tree", object], None)?;
        let (entry, name) = entries
            .split_once('\t')
            .context("missing change metadata file")?;
        let parts: Vec<_> = entry.split_whitespace().collect();
        ensure!(
            parts.len() == 3 && parts[0] == "100644" && parts[1] == "blob" && name == "change.json",
            "metadata tree must contain exactly change.json"
        );
        let record: Record =
            serde_json::from_slice(&self.git(&["cat-file", "blob", parts[2]], None)?)?;
        self.validate_source(&record)?;
        let parents = self.text(&["show", "-s", "--format=%P", object], None)?;
        let parents: Vec<_> = parents.split_whitespace().collect();
        let expected: Vec<_> = record
            .previous_record
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(record.source_commit.as_str()))
            .collect();
        ensure!(
            parents == expected,
            "metadata parents do not retain previous/source commits"
        );
        if let Some(previous) = &record.previous_record {
            let prior: Record = serde_json::from_slice(
                &self.git(&["show", &format!("{previous}:change.json")], None)?,
            )?;
            prior.validate()?;
            ensure!(
                prior.change_id == record.change_id
                    && prior.generation.checked_add(1) == Some(record.generation),
                "metadata identity/generation history conflict"
            );
            ensure!(
                prior.created_at == record.created_at,
                "creation time rewritten"
            );
            ensure!(
                prior.base_commit == record.base_commit,
                "accepted base rewritten"
            );
        }
        Ok(Revision {
            oid: object.into(),
            record,
        })
    }

    pub fn validate_source(&self, record: &Record) -> Result<()> {
        record.validate()?;
        ensure!(
            self.source(&record.source_commit)?.1 == record.source_tree,
            "record source/tree mismatch"
        );
        self.git(
            &[
                "merge-base",
                "--is-ancestor",
                &record.base_commit,
                &record.source_commit,
            ],
            None,
        )?;
        if let Some(mirror) = &record.mirror {
            self.git(
                &["check-ref-format", &format!("refs/heads/{}", mirror.branch)],
                None,
            )?;
        }
        match (
            record.trailer_status.clone(),
            self.trailer(&record.source_commit)?,
        ) {
            (TrailerStatus::Present, Some(id)) if id == record.change_id => {}
            (TrailerStatus::ExternalMissing, None) => {}
            _ => bail!("source Change-Id missing or conflicting; use explicit recovery/import"),
        }
        Ok(())
    }

    /// Construct a distinct new metadata commit; this does not publish it.
    pub fn prepare(&self, record: Record) -> Result<Revision> {
        self.validate_source(&record)?;
        let body = serde_json::to_vec(&record)?;
        let blob = self.text(&["hash-object", "-w", "--stdin"], Some(&body))?;
        let tree = self.text(
            &["mktree"],
            Some(format!("100644 blob {blob}\tchange.json\n").as_bytes()),
        )?;
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(previous) = &record.previous_record {
            args.extend(["-p", previous]);
        }
        args.extend(["-p", record.source_commit.as_str()]);
        let object = self.text(&args, Some(b"st change record v1\n"))?;
        let candidate = self.load(&object)?;
        ensure!(
            candidate.record == record,
            "constructed record differs from request"
        );
        Ok(candidate)
    }

    /// Every actual ref update/deletion has its exact old-OID lease. The current ref
    /// ALWAYS changes, so Git cannot skip the only transaction guard as a no-op.
    pub fn publish(
        &self,
        remote: &str,
        old: Option<&Revision>,
        candidate: &Revision,
    ) -> Result<()> {
        self.publish_with_release(remote, old, candidate, None)
    }

    /// Claim release has an explicit worker/nonce proof. The caller still validates
    /// its runtime authority; actor labels and a publicly readable nonce are not identity.
    pub fn publish_release(
        &self,
        remote: &str,
        old: &Revision,
        candidate: &Revision,
        worker: &str,
        nonce: &str,
    ) -> Result<()> {
        self.publish_with_release(remote, Some(old), candidate, Some((worker, nonce)))
    }

    fn publish_with_release(
        &self,
        remote: &str,
        old: Option<&Revision>,
        candidate: &Revision,
        release: Option<(&str, &str)>,
    ) -> Result<()> {
        let loaded = self.load(&candidate.oid)?;
        ensure!(
            &loaded == candidate,
            "candidate does not match its Git object"
        );
        if let Some(old) = old {
            ensure!(
                self.load(&old.oid)? == *old,
                "old metadata does not match object"
            );
            ensure!(
                candidate.record.change_id == old.record.change_id
                    && candidate.record.previous_record.as_deref() == Some(old.oid.as_str())
                    && candidate.oid != old.oid,
                "transition needs a real current-record update"
            );
            ensure!(
                candidate.record.base_commit == old.record.base_commit,
                "accepted base cannot silently change"
            );
        } else {
            ensure!(
                candidate.record.previous_record.is_none(),
                "creation has previous metadata"
            );
        }
        let edge = transition(old, candidate)?;
        if edge == Edge::Release {
            let (worker, nonce) =
                release.context("claim release requires publish_release worker/nonce")?;
            let claim = old
                .and_then(|old| old.record.claim.as_ref())
                .context("missing old claim")?;
            ensure!(
                claim.worker == worker && claim.nonce == nonce,
                "claim release worker/nonce mismatch"
            );
        } else {
            ensure!(
                release.is_none(),
                "release proof supplied for a non-release transition"
            );
        }
        let before = old.map(markers).transpose()?.unwrap_or_default();
        let after = markers(candidate)?;
        let names: BTreeSet<_> = before.keys().chain(after.keys()).collect();
        let mut args = vec!["push".to_owned(), "--atomic".into(), "--porcelain".into()];
        let mut updates = Vec::new();
        for name in names {
            if before.get(name) == after.get(name) {
                continue;
            }
            let expected = before.get(name).map(String::as_str).unwrap_or("");
            args.push(format!("--force-with-lease={name}:{expected}"));
            updates.push(format!(
                "{}:{name}",
                after.get(name).map(String::as_str).unwrap_or("")
            ));
        }
        args.push(self.remote(remote)?);
        args.extend(updates);
        self.git(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
        Ok(())
    }

    /// The caller validates live runtime authority and dependencies/evidence before
    /// invoking this primitive. It never steals an extant claim or advances main.
    pub fn prepare_claim(
        &self,
        current: &Revision,
        worker: &str,
        work: WorkBinding,
    ) -> Result<Revision> {
        ensure!(
            current.record.state == State::Ready && current.record.claim.is_none(),
            "change is not eligible unclaimed ready work"
        );
        let selected_lane = current
            .record
            .next
            .clone()
            .context("change has no next lane")?;
        let mut record = successor(current, worker)?;
        record.next = None;
        record.claim = Some(Claim {
            lane: selected_lane,
            worker: worker.into(),
            work,
            nonce: random_hex::<16>()?,
            expected_source: record.source_commit.clone(),
            expected_tree: record.source_tree.clone(),
            acquired_at: record.updated_at.clone(),
        });
        let candidate = self.prepare(record)?;
        transition(Some(current), &candidate)?;
        Ok(candidate)
    }

    pub fn prepare_release(
        &self,
        current: &Revision,
        worker: &str,
        nonce: &str,
        next: Option<String>,
    ) -> Result<Revision> {
        let claim = current
            .record
            .claim
            .as_ref()
            .context("change is not claimed")?;
        ensure!(
            claim.worker == worker && claim.nonce == nonce,
            "claim worker/nonce mismatch"
        );
        let mut record = successor(current, worker)?;
        record.claim = None;
        record.next = next;
        let candidate = self.prepare(record)?;
        transition(Some(current), &candidate)?;
        Ok(candidate)
    }

    pub fn prepare_state(
        &self,
        current: &Revision,
        actor: &str,
        state: State,
        next: Option<String>,
    ) -> Result<Revision> {
        ensure!(
            current.record.claim.is_none(),
            "release the claim before revising state"
        );
        ensure!(
            matches!(state, State::Draft | State::Ready | State::Abandoned),
            "landing requires an independently verified observer"
        );
        ensure!(
            matches!(current.record.state, State::Draft | State::Ready),
            "terminal change cannot be reopened implicitly"
        );
        let mut record = successor(current, actor)?;
        record.state = state;
        record.next = next;
        let candidate = self.prepare(record)?;
        transition(Some(current), &candidate)?;
        Ok(candidate)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Edge {
    Create,
    Update,
    Claim,
    Release,
}

/// One publication transition table. A generic prepared object carries no permission
/// to reopen terminal work, land, retarget a claim, or release another worker's claim.
fn transition(old: Option<&Revision>, candidate: &Revision) -> Result<Edge> {
    let new = &candidate.record;
    new.validate()?;
    let Some(old) = old else {
        ensure!(
            new.generation == 1
                && new.previous_record.is_none()
                && new.claim.is_none()
                && matches!(new.state, State::Draft | State::Ready),
            "creation requires unclaimed draft or ready work"
        );
        return Ok(Edge::Create);
    };
    let prior = &old.record;
    ensure!(
        matches!(prior.state, State::Draft | State::Ready),
        "terminal change has no ordinary successor"
    );
    ensure!(
        prior.change_id == new.change_id
            && prior.base_commit == new.base_commit
            && prior.created_at == new.created_at
            && prior.generation.checked_add(1) == Some(new.generation)
            && new.previous_record.as_deref() == Some(old.oid.as_str())
            && candidate.oid != old.oid,
        "invalid change identity/base/history transition"
    );
    let same_payload = prior.source_commit == new.source_commit
        && prior.source_tree == new.source_tree
        && prior.trailer_status == new.trailer_status
        && prior.depends_on == new.depends_on
        && prior.mirror == new.mirror;
    if let Some(claim) = &prior.claim {
        ensure!(
            new.claim.is_none()
                && new.state == prior.state
                && same_payload
                && new.actor == claim.worker,
            "claimed work permits only owner release with unchanged state/content"
        );
        return Ok(Edge::Release);
    }
    if let Some(claim) = &new.claim {
        ensure!(
            prior.state == State::Ready
                && prior.next.as_deref() == Some(claim.lane.as_str())
                && new.state == State::Ready
                && new.next.is_none()
                && same_payload
                && new.actor == claim.worker,
            "claim requires old ready/next lane and unchanged content"
        );
        return Ok(Edge::Claim);
    }
    ensure!(
        matches!(new.state, State::Draft | State::Ready | State::Abandoned),
        "landing has no ordinary publication path; an independently verified observer is required"
    );
    Ok(Edge::Update)
}

pub fn successor(current: &Revision, actor: &str) -> Result<Record> {
    let mut record = current.record.clone();
    record.generation = record
        .generation
        .checked_add(1)
        .context("change generation overflow")?;
    record.previous_record = Some(current.oid.clone());
    record.actor = actor.into();
    record.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(record)
}

pub fn markers(revision: &Revision) -> Result<BTreeMap<String, String>> {
    revision.record.validate()?;
    oid(&revision.oid)?;
    let prefix = format!("{PREFIX}{}", revision.record.change_id);
    let mut map = BTreeMap::from([(format!("{prefix}/current"), revision.oid.clone())]);
    let state = match revision.record.state {
        State::Draft => "draft",
        State::Ready => "ready",
        _ => return Ok(map),
    };
    map.insert(
        format!("{prefix}/head"),
        revision.record.source_commit.clone(),
    );
    map.insert(format!("{prefix}/state/{state}"), revision.oid.clone());
    if let Some(next) = &revision.record.next {
        map.insert(format!("{prefix}/next/{next}"), revision.oid.clone());
    }
    if let Some(claim) = &revision.record.claim {
        map.insert(
            format!("{prefix}/claims/{}", claim.lane),
            revision.oid.clone(),
        );
    }
    Ok(map)
}

pub fn validate_advertisement(advertisement: &Advertisement, revision: &Revision) -> Result<()> {
    let expected = markers(revision)?;
    let prefix = format!("{PREFIX}{}/", revision.record.change_id);
    let actual: BTreeMap<_, _> = advertisement
        .refs
        .range(prefix.clone()..)
        .take_while(|(name, _)| name.starts_with(&prefix))
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect();
    ensure!(expected == actual, "remote change marker conflict");
    Ok(())
}

use sha2::Digest;

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn included_credential_helpers_are_reset_before_the_operator_helper() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "-q"]);
        git(&["config", "credential.useHttpPath", "true"]);
        let unexpected = root.join("unexpected-helper");
        let expected = root.join("operator-helper");
        let included = root.join("included.config");
        std::fs::write(
            &included,
            format!(
                "[credential \"https://example.invalid\"]\nhelper = !touch {}\n[http \"https://example.invalid\"]\nfollowRedirects = true\nextraHeader = Authorization: synthetic-checkout\n",
                unexpected.display()
            ),
        )
        .unwrap();
        git(&["config", "include.path", included.to_str().unwrap()]);
        let repository = Repository::open(
            root,
            Authentication {
                helper: Some(format!(
                    "!f() {{ touch {}; printf 'username=fixture\\npassword=synthetic\\n'; }}; f",
                    expected.display()
                )),
                ssh_command: None,
            },
        )
        .unwrap();
        // Configuration can change after construction: a newly added contextual
        // helper must also be disabled on subsequent operations.
        git(&[
            "config",
            "credential.https://example.invalid/new.helper",
            &format!("!touch {}", unexpected.display()),
        ]);
        let credential = repository
            .text(
                &["credential", "fill"],
                Some(b"protocol=https\nhost=example.invalid\npath=new\n\n"),
            )
            .unwrap();
        assert!(credential.contains("username=fixture"));
        assert!(credential.contains("password=synthetic"));
        assert!(expected.exists());
        assert!(!unexpected.exists());
        assert_eq!(
            repository
                .text(
                    &[
                        "config",
                        "--get-urlmatch",
                        "http.followRedirects",
                        "https://example.invalid/"
                    ],
                    None
                )
                .unwrap(),
            "false"
        );
        assert_eq!(
            repository
                .text(
                    &[
                        "config",
                        "--get-urlmatch",
                        "http.extraHeader",
                        "https://example.invalid/"
                    ],
                    None
                )
                .unwrap(),
            ""
        );
        git(&[
            "config",
            "core.askPass",
            &format!("sh -c 'touch {}'", unexpected.display()),
        ]);
        let unauthenticated = Repository::open(root, Authentication::default()).unwrap();
        assert!(
            unauthenticated
                .text(
                    &["credential", "fill"],
                    Some(b"protocol=https\nhost=example.invalid\npath=new\n\n")
                )
                .is_err()
        );
        assert!(!unexpected.exists());
    }
}
