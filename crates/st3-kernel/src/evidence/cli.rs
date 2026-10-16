//! Portable offline evidence commands and a protected selector-only CI observation command.
use crate::cli_println as println;

use super::*;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct EvidenceArgs {
    #[command(subcommand)]
    pub command: EvidenceCommand,
}

#[derive(Subcommand)]
pub enum EvidenceCommand {
    /// Installed observer for a protected profile; accepts authenticated CI selectors only.
    #[command(hide = true)]
    ObserveCi {
        #[arg(long)]
        run: u64,
        #[arg(long)]
        attempt: u64,
        #[arg(long)]
        artifact: u64,
        /// Immutable degraded check records; the observer authenticates their jobs itself.
        #[arg(long = "re-attest")]
        predecessors: Vec<String>,
    },
    /// Import an observation: signed full/test, or explicitly unsigned degraded.
    Put {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Full notes commit ID (fetch its objects first).
        #[arg(long)]
        record: String,
        /// Operator-supplied OpenSSH allowed-signers file, outside repository policy.
        #[arg(long)]
        trusted_signers: PathBuf,
    },
    /// Print a record's in-toto statement. This does not assert signature trust.
    Show {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        record: String,
    },
    /// Verify a record's signature, or require all gate evidence for a target revision.
    Verify {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Verify only this signed notes commit, without evaluating stage policy.
        #[arg(long, conflicts_with_all = ["revision", "base", "change_id"])]
        record: Option<String>,
        #[arg(long, required_unless_present = "record", requires_all = ["base", "change_id"])]
        revision: Option<String>,
        /// Current review base, including when the target has been rebased.
        #[arg(long, requires = "revision")]
        base: Option<String>,
        /// Placeholder stable identity until the changes-in-git format is available.
        #[arg(long, requires = "revision")]
        change_id: Option<String>,
        #[arg(long)]
        trusted_signers: PathBuf,
    },
}

pub fn run(args: EvidenceArgs) -> Result<()> {
    let value = match args.command {
        EvidenceCommand::ObserveCi {
            run,
            attempt,
            artifact,
            predecessors,
        } => ci_transport::observe_ci(run, attempt, artifact, &predecessors)?,
        EvidenceCommand::Put {
            repo,
            record,
            trusted_signers,
        } => {
            serde_json::json!({"ref": put(&repo, &record, &trusted_signers)?, "tier": show(&repo, &record)?.predicate.tier})
        }
        EvidenceCommand::Show { repo, record } => serde_json::to_value(show(&repo, &record)?)?,
        EvidenceCommand::Verify {
            repo,
            record: Some(record),
            trusted_signers,
            ..
        } => {
            let statement = verify_record(&repo, &record, &trusted_signers)?;
            serde_json::json!({"record":record, "signer":statement.predicate.signer, "tier":statement.predicate.tier, "signature_verified":statement.predicate.tier != TrustTier::Degraded})
        }
        EvidenceCommand::Verify {
            repo,
            record: None,
            revision,
            base,
            change_id,
            trusted_signers,
        } => {
            let revision = revision.context("--revision required for stage verification")?;
            let base = base.context("--base required for stage verification")?;
            let base = commit(&repo, &base)?;
            let change_id = change_id.context("--change-id required for stage verification")?;
            // Pin aliases such as HEAD once: each source must stay fixed throughout verification.
            let revision = commit(&repo, &revision)?;
            let policy = load_policy(&repo, &base)?;
            serde_json::to_value(verify(
                &repo,
                &revision,
                &base,
                &change_id,
                &trusted_signers,
                &policy,
            )?)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
