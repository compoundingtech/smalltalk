//! Explicit operator configuration. Requests select a configured identity; they
//! never supply a checkout, credential command, remote destination, or signer file.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub repositories: Vec<Repository>,
}

#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Repository {
    pub name: String,
    pub reader_node: String,
    pub transport_directory: PathBuf,
    pub remote: String,
    /// Exact operator-authorized destination; checkout configuration cannot select it.
    pub remote_url: String,
    pub credential_helper: Option<String>,
    pub ssh_command: Option<String>,
    /// Safe operator label for diagnostics, never a token or command body.
    pub credential_source_name: Option<String>,
    /// The approved pilot's start time, never derived from an unsigned metadata label.
    pub pilot_started_at: String,
    /// Operator-managed verification trust, outside the transport checkout.
    pub trusted_signers: Option<PathBuf>,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChangeRepository")
            .field("name", &self.name)
            .field("reader_node", &self.reader_node)
            .field("transport_directory", &self.transport_directory)
            .field("remote", &self.remote)
            .field(
                "credential_helper_configured",
                &self.credential_helper.is_some(),
            )
            .field("ssh_command_configured", &self.ssh_command.is_some())
            .field("pilot_started_at", &self.pilot_started_at)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        ensure!(
            !self.repositories.is_empty() && self.repositories.len() <= 8,
            "changes requires 1 through 8 explicitly configured repositories"
        );
        let mut names = BTreeSet::new();
        for repository in &self.repositories {
            ensure!(
                !repository.name.is_empty()
                    && repository.name.len() <= 128
                    && repository.name.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || b"/-_.".contains(&byte))
                    && names.insert(repository.name.clone()),
                "invalid or duplicate configured change repository"
            );
            ensure!(
                !repository.reader_node.is_empty()
                    && repository.reader_node.len() <= 128
                    && repository
                        .reader_node
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
                "changes reader_node must name one concrete node"
            );
            ensure!(
                repository.transport_directory.is_absolute(),
                "changes transport_directory must be absolute"
            );
            ensure!(
                !repository.remote.is_empty()
                    && repository.remote.len() <= 64
                    && repository
                        .remote
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
                "invalid configured Git remote name"
            );
            ensure!(
                !repository.remote_url.is_empty()
                    && repository.remote_url.len() <= 2048
                    && !repository.remote_url.chars().any(char::is_control),
                "missing or invalid authorized Git destination"
            );
            ensure!(
                repository.pilot_started_at.len() <= 64,
                "invalid pilot start time"
            );
            chrono::DateTime::parse_from_rfc3339(&repository.pilot_started_at)?;
            for command in [&repository.credential_helper, &repository.ssh_command]
                .into_iter()
                .flatten()
            {
                ensure!(
                    !command.trim().is_empty()
                        && command.len() <= 2048
                        && !command.chars().any(char::is_control),
                    "invalid operator Git authentication command"
                );
            }
            if repository.credential_helper.is_some() || repository.ssh_command.is_some() {
                ensure!(
                    repository
                        .credential_source_name
                        .as_ref()
                        .is_some_and(|name| !name.is_empty()
                            && name.len() <= 128
                            && name.bytes().all(
                                |byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte)
                            )),
                    "changes requires a safe credential_source_name when authentication is configured"
                );
            }
            if let Some(signers) = &repository.trusted_signers {
                ensure!(
                    signers.is_absolute() && !signers.starts_with(&repository.transport_directory),
                    "changes trusted_signers must be outside the transport checkout"
                );
            }
        }
        Ok(())
    }
}
