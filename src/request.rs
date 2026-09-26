//! Idempotent service-principal request/reply transport over the native message bus.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use kdl::KdlDocument;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::message;

const REQUEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePrincipal {
    pub identity: String,
    pub host: String,
    pub path: PathBuf,
}

impl ServicePrincipal {
    pub fn bus_id(&self) -> String {
        format!("{}.{}", self.host, self.identity)
    }

    pub fn inbox(&self) -> PathBuf {
        message::inbox_dir(&self.path)
    }

    pub fn archive(&self) -> PathBuf {
        message::archive_dir(&self.path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestEnvelope {
    version: u32,
    idempotency_key: String,
    from: String,
    to: String,
    reply_to: String,
    tags: BTreeMap<String, String>,
    body: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplyEnvelope {
    version: u32,
    idempotency_key: String,
    request_filename: String,
    from: String,
    tags: BTreeMap<String, String>,
    body: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicationRecord {
    version: u32,
    idempotency_key: String,
    from: String,
    to: String,
    filename: String,
    envelope: String,
    rendered_message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishReceipt {
    pub status: &'static str,
    pub idempotency_key: String,
    pub filename: String,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IncomingRequest {
    pub status: &'static str,
    pub idempotency_key: String,
    pub request_filename: String,
    pub from: String,
    pub tags: BTreeMap<String, String>,
    pub body: Value,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum RequestStatus {
    Pending {
        idempotency_key: String,
        request_filename: String,
    },
    Replied {
        idempotency_key: String,
        request_filename: String,
        from: String,
        tags: BTreeMap<String, String>,
        body: Value,
    },
}

pub fn discover_principals(root: &Path) -> anyhow::Result<Vec<ServicePrincipal>> {
    let principals_root = root.join("principals");
    let mut declarations = Vec::new();
    let hosts = match fs::read_dir(&principals_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    for host_entry in hosts.flatten().filter(|entry| entry.path().is_dir()) {
        let identities = fs::read_dir(host_entry.path())?;
        for identity_entry in identities.flatten().filter(|entry| entry.path().is_dir()) {
            let declaration = identity_entry.path().join("principal.kdl");
            if declaration.is_file() {
                declarations.push(declaration);
            }
        }
    }
    declarations.sort();

    let mut principals = Vec::new();
    let mut bus_ids = HashSet::new();
    for declaration in declarations {
        let text = fs::read_to_string(&declaration)?;
        let document = KdlDocument::parse(&text).map_err(|error| {
            anyhow::anyhow!("{}: KDL parse error: {error}", declaration.display())
        })?;
        let nodes: Vec<_> = document
            .nodes()
            .iter()
            .filter(|node| node.name().value() == "principal")
            .collect();
        if nodes.len() != 1 {
            anyhow::bail!(
                "{}: expected exactly one `principal` declaration",
                declaration.display()
            );
        }
        let node = nodes[0];
        let identity = node
            .get(0)
            .and_then(|value| value.as_string())
            .context("principal identity must be the first string argument")?
            .to_string();
        let host = node
            .get("host")
            .and_then(|value| value.as_string())
            .context("principal requires string property `host`")?
            .to_string();
        let directory = declaration
            .parent()
            .context("principal declaration has no parent")?;
        let path_identity = directory.file_name().and_then(|value| value.to_str());
        let path_host = directory
            .parent()
            .and_then(|value| value.file_name())
            .and_then(|value| value.to_str());
        if path_identity != Some(identity.as_str()) || path_host != Some(host.as_str()) {
            anyhow::bail!(
                "{}: principal content `{host}.{identity}` must match `principals/<host>/<identity>/principal.kdl`",
                declaration.display()
            );
        }
        let principal = ServicePrincipal {
            identity,
            host,
            path: directory.to_path_buf(),
        };
        if !bus_ids.insert(principal.bus_id()) {
            anyhow::bail!("duplicate service principal `{}`", principal.bus_id());
        }
        principals.push(principal);
    }
    Ok(principals)
}

pub fn resolve_principal(
    root: &Path,
    identity: &str,
    this_host: &str,
) -> anyhow::Result<ServicePrincipal> {
    let principal = discover_principals(root)?
        .into_iter()
        .find(|principal| {
            principal.bus_id() == identity
                || (principal.host == this_host && principal.identity == identity)
        })
        .with_context(|| {
            format!(
                "'{identity}' is not a declared service principal in catalog {}",
                root.display()
            )
        })?;
    if crate::discover(root)
        .specs
        .into_iter()
        .any(|spec| spec.bus_id(this_host) == principal.bus_id())
    {
        anyhow::bail!(
            "service principal `{}` collides with an Agent Spec identity",
            principal.bus_id()
        );
    }
    Ok(principal)
}

fn resolve_agent(
    root: &Path,
    identity: &str,
    this_host: &str,
) -> anyhow::Result<(String, PathBuf)> {
    let discovered = crate::discover(root);
    let spec = discovered
        .specs
        .into_iter()
        .find(|spec| {
            spec.bus_id(this_host) == identity
                || (spec.resolved_host(this_host) == this_host && spec.identity == identity)
        })
        .with_context(|| format!("no agent '{identity}' found in catalog {}", root.display()))?;
    let bus_id = spec.bus_id(this_host);
    let path = spec
        .path
        .parent()
        .context("agent declaration has no parent")?
        .to_path_buf();
    Ok((bus_id, path))
}

pub fn publish(
    root: &Path,
    this_host: &str,
    principal_identity: &str,
    recipient: &str,
    idempotency_key: &str,
    tags: BTreeMap<String, String>,
    body: Value,
) -> anyhow::Result<PublishReceipt> {
    require_key(idempotency_key)?;
    let principal = resolve_principal(root, principal_identity, this_host)?;
    let (to, recipient_dir) = resolve_agent(root, recipient, this_host)?;
    let from = principal.bus_id();
    let envelope = RequestEnvelope {
        version: REQUEST_VERSION,
        idempotency_key: idempotency_key.to_string(),
        from: from.clone(),
        to: to.clone(),
        reply_to: from.clone(),
        tags,
        body,
    };
    let envelope = serde_json::to_string(&envelope)?;
    let record_dir = principal.path.join("resources/request-state/outgoing");
    publish_once(
        &record_dir,
        idempotency_key,
        idempotency_key,
        &from,
        &to,
        &envelope,
        &message::inbox_dir(&recipient_dir),
        Some(&format!("request {idempotency_key}")),
        None,
        &["st2-request".to_string()],
        "request",
    )
}

pub fn reply(
    root: &Path,
    this_host: &str,
    agent_identity: &str,
    request_filename: &str,
    tags: BTreeMap<String, String>,
    body: Value,
) -> anyhow::Result<PublishReceipt> {
    let (from, agent_dir) = resolve_agent(root, agent_identity, this_host)?;
    let request_message = read_inbox_or_archive(&agent_dir, request_filename)?;
    let request = parse_request_message(&request_message)?;
    if request.version != REQUEST_VERSION || request.to != from {
        anyhow::bail!("request is not addressed to agent `{from}`");
    }
    let principal = resolve_principal(root, &request.reply_to, this_host)?;
    let envelope = ReplyEnvelope {
        version: REQUEST_VERSION,
        idempotency_key: request.idempotency_key.clone(),
        request_filename: request_filename.to_string(),
        from: from.clone(),
        tags,
        body,
    };
    let envelope = serde_json::to_string(&envelope)?;
    let state_key = format!("{}\0{}", request.reply_to, request.idempotency_key);
    let record_dir = agent_dir.join("resources/request-state/replies");
    publish_once(
        &record_dir,
        &state_key,
        &request.idempotency_key,
        &from,
        &request.reply_to,
        &envelope,
        &principal.inbox(),
        Some(&format!("re: request {}", request.idempotency_key)),
        Some(request_filename),
        &["st2-request-reply".to_string()],
        "reply",
    )
}

pub fn read(
    root: &Path,
    this_host: &str,
    agent_identity: &str,
    request_filename: &str,
) -> anyhow::Result<IncomingRequest> {
    let (agent, agent_dir) = resolve_agent(root, agent_identity, this_host)?;
    let message = read_inbox_or_archive(&agent_dir, request_filename)?;
    let request = parse_request_message(&message)?;
    if request.version != REQUEST_VERSION || request.to != agent {
        anyhow::bail!("request is not addressed to agent `{agent}`");
    }
    resolve_principal(root, &request.reply_to, this_host)?;
    Ok(IncomingRequest {
        status: "request",
        idempotency_key: request.idempotency_key,
        request_filename: request_filename.to_string(),
        from: request.from,
        tags: request.tags,
        body: request.body,
    })
}

pub fn status(
    root: &Path,
    this_host: &str,
    principal_identity: &str,
    idempotency_key: &str,
) -> anyhow::Result<RequestStatus> {
    let principal = resolve_principal(root, principal_identity, this_host)?;
    let record_path = record_path(
        &principal.path.join("resources/request-state/outgoing"),
        idempotency_key,
    );
    let record: PublicationRecord =
        serde_json::from_slice(&fs::read(&record_path).with_context(|| {
            format!("no published request for idempotency key `{idempotency_key}`")
        })?)?;

    let mut replies = Vec::new();
    for directory in [principal.inbox(), principal.archive()] {
        for candidate in message::list_dir(&directory)? {
            if candidate.in_reply_to.as_deref() != Some(record.filename.as_str()) {
                continue;
            }
            if let Ok(reply) = serde_json::from_str::<ReplyEnvelope>(candidate.body.trim())
                && reply.version == REQUEST_VERSION
                && reply.idempotency_key == idempotency_key
                && reply.request_filename == record.filename
                && reply.from == record.to
                && candidate.from.as_deref() == Some(reply.from.as_str())
                && candidate.tags.iter().any(|tag| tag == "st2-request-reply")
            {
                replies.push(reply);
            }
        }
    }
    replies.dedup_by(|left, right| left == right);
    match replies.as_slice() {
        [] => Ok(RequestStatus::Pending {
            idempotency_key: idempotency_key.to_string(),
            request_filename: record.filename,
        }),
        [reply] => Ok(RequestStatus::Replied {
            idempotency_key: idempotency_key.to_string(),
            request_filename: record.filename,
            from: reply.from.clone(),
            tags: reply.tags.clone(),
            body: reply.body.clone(),
        }),
        _ => anyhow::bail!("conflicting replies for idempotency key `{idempotency_key}`"),
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_once(
    state_dir: &Path,
    state_key: &str,
    receipt_key: &str,
    from: &str,
    to: &str,
    envelope: &str,
    inbox: &Path,
    subject: Option<&str>,
    in_reply_to: Option<&str>,
    message_tags: &[String],
    kind: &str,
) -> anyhow::Result<PublishReceipt> {
    fs::create_dir_all(state_dir)?;
    let path = record_path(state_dir, state_key);
    let candidate = PublicationRecord {
        version: REQUEST_VERSION,
        idempotency_key: state_key.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        filename: message::new_filename(),
        envelope: envelope.to_string(),
        rendered_message: String::new(),
    };
    let mut candidate = candidate;
    candidate.rendered_message = message::render_message(
        from,
        subject,
        in_reply_to,
        message_tags,
        &candidate.envelope,
    );
    let serialized = serde_json::to_vec(&candidate)?;
    let created = atomic_create(&path, &serialized)?;
    let record: PublicationRecord = if created {
        candidate
    } else {
        serde_json::from_slice(&fs::read(&path)?)?
    };
    if record.version != REQUEST_VERSION
        || record.idempotency_key != state_key
        || record.from != from
        || record.to != to
        || record.envelope != envelope
    {
        anyhow::bail!("idempotency key reused with different {kind}");
    }
    let message_created =
        message::materialize_message_once(inbox, &record.filename, &record.rendered_message)?;
    Ok(PublishReceipt {
        status: "published",
        idempotency_key: receipt_key.to_string(),
        filename: record.filename,
        deduplicated: !created || !message_created,
    })
}

/// Reserve the idempotency record, reporting whether this call is the one that created it.
/// Hardlinked rather than renamed for that boolean: the caller replays an interrupted send when
/// the name was already taken, and replaces nothing.
fn atomic_create(path: &Path, bytes: &[u8]) -> anyhow::Result<bool> {
    Ok(crate::fsatomic::create_once(
        path,
        bytes,
        crate::fsatomic::Staging::new(".request-state"),
    )?)
}

fn record_path(directory: &Path, key: &str) -> PathBuf {
    let hash = Sha256::digest(key.as_bytes());
    directory.join(format!("{hash:x}.json"))
}

fn require_key(key: &str) -> anyhow::Result<()> {
    if key.is_empty() {
        anyhow::bail!("idempotency key must not be empty");
    }
    Ok(())
}

fn parse_request_message(message: &message::Message) -> anyhow::Result<RequestEnvelope> {
    let request: RequestEnvelope =
        serde_json::from_str(message.body.trim()).context("message is not a typed st2 request")?;
    if request.from != request.reply_to {
        anyhow::bail!("request sender and reply target differ");
    }
    if message.from.as_deref() != Some(request.from.as_str()) {
        anyhow::bail!("request frontmatter sender does not match its JSON envelope");
    }
    if !message.tags.iter().any(|tag| tag == "st2-request") {
        anyhow::bail!("request is missing the native `st2-request` transport tag");
    }
    if message.in_reply_to.is_some() {
        anyhow::bail!("request must not be a native reply");
    }
    Ok(request)
}

fn read_inbox_or_archive(agent_dir: &Path, filename: &str) -> anyhow::Result<message::Message> {
    message::read_msg(&message::inbox_dir(agent_dir), filename)
        .or_else(|_| message::read_msg(&message::archive_dir(agent_dir), filename))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`atomic_create`]'s create-once contract. The idempotency record IS the deduplication: a
    /// second publication of the same key must report `false` rather than replace the record the
    /// first request registered, because the caller reads that boolean to decide whether it is
    /// replaying or racing.
    ///
    /// The mode is the deliberate change of the fold onto `fsatomic` — these records used to be
    /// published at whatever an ordinary write produces (`0644` under the fleet's umask).
    #[test]
    fn a_create_once_state_record_keeps_the_first_bytes_and_reports_the_duplicate() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let path = record_path(&tmp.path().join("state"), "key");
        assert!(atomic_create(&path, b"first").unwrap());
        assert!(!atomic_create(&path, b"second").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the state record is published owner-only"
        );

        let residue = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".request-state.tmp-"))
            .collect::<Vec<_>>();
        assert!(
            residue.is_empty(),
            "staging residue left behind: {residue:?}"
        );
    }
}
