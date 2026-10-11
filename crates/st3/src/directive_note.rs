use anyhow::{Context as _, Result};
use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::json;
use st3::client::{Client, Endpoint};
use st3::store::directive_notes::DirectiveNote;

#[derive(Args)]
#[command(after_help = "Notes are explicit reads only: no launch delivery, notifications or UI. Only a person can set or clear their own note. Notes never assign work, approve a gate or grant authority.")]
pub(super) struct NoteArgs {
    /// Read as this person or agent; defaults to the harness identity, else configured person.
    #[arg(long, global = true, value_parser = super::parse_actor_subject)]
    actor: Option<String>,
    #[command(subcommand)]
    command: Option<NoteCommand>,
}

#[derive(Subcommand)]
enum NoteCommand {
    /// Replace your current note (nonblank, at most 4096 UTF-8 bytes).
    Set {
        text: String,
        /// Optional UTC RFC3339 expiry; expired notes disappear from reads.
        #[arg(long, value_name = "RFC3339")]
        expires_at: Option<String>,
    },
    /// Clear your current note. Only persons may clear their own note.
    Clear,
}

#[derive(Deserialize, serde::Serialize)]
struct NotesResponse {
    notes: Vec<DirectiveNote>,
}

#[derive(Deserialize, serde::Serialize)]
struct NoteResponse {
    note: Option<DirectiveNote>,
}

pub(super) async fn run(
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    args: NoteArgs,
    json_output: bool,
) -> Result<()> {
    let actor = super::configured_actor(args.actor.as_deref(), configured_person, "note")?;
    if let Some(own) = std::env::var("ST_AGENT").ok().filter(|own| !own.is_empty()) {
        anyhow::ensure!(actor == own, "an agent must read notes as its own harness identity");
        anyhow::ensure!(args.command.is_none(), "only a person outside an agent seat can set or clear a note");
    }
    if args.command.is_some() {
        super::parse_person_subject(&actor).map_err(anyhow::Error::msg)?;
    }
    let Endpoint::Unix(socket) = endpoint else {
        anyhow::bail!("st note requires the authenticated local Unix socket");
    };
    let client = if actor.starts_with("agent/") {
        Client::unix_agent(socket, &actor)?
    } else {
        Client::unix_as(socket, &actor)?
    };
    match args.command {
        None => {
            let response: NotesResponse = client
                .get(&format!("/v1/client/notes?actor={}", urlencoding::encode(&actor)))
                .await?;
            if json_output {
                return super::print_value(&response, true);
            }
            if response.notes.is_empty() {
                println!("No current notes.");
            }
            for note in response.notes {
                println!("{} — {} at {}", note.person, note.author, note.time);
                if let Some(expiry) = note.expires_at {
                    println!("Expires: {expiry}");
                }
                println!("Revision: {}\n{}", note.revision, note.text);
            }
        }
        Some(command) => {
            let (text, expires_at) = match command {
                NoteCommand::Set { text, expires_at } => (Some(text), expires_at),
                NoteCommand::Clear => (None, None),
            };
            let response: NoteResponse = client
                .request("PUT", "/v1/notes", Some(&json!({
                    "person": actor,
                    "actor": actor,
                    "text": text,
                    "expires_at": expires_at,
                })))
                .await
                .context("could not update your directive note")?;
            if json_output {
                return super::print_value(&response, true);
            }
            println!("{}", if text.is_some() { "Note set." } else { "Note cleared." });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use super::*;

    #[test]
    fn note_parser_reads_and_accepts_actor_after_subcommand() {
        let cli = super::super::Cli::try_parse_from(["st", "note", "--actor", "agent/helper"]).unwrap();
        let super::super::Command::Note(args) = cli.command else { panic!("expected note") };
        assert_eq!(args.actor.as_deref(), Some("agent/helper"));
        assert!(args.command.is_none());
        let cli = super::super::Cli::try_parse_from(["st", "note", "clear", "--actor", "person/avery"]).unwrap();
        let super::super::Command::Note(args) = cli.command else { panic!("expected note") };
        assert!(matches!(args.command, Some(NoteCommand::Clear)));
    }

    #[test]
    fn note_parser_preserves_text_and_expiry_without_person_override() {
        let cli = super::super::Cli::try_parse_from([
            "st", "note", "set", "Review before merging", "--expires-at", "2099-01-01T00:00:00Z",
        ]).unwrap();
        let super::super::Command::Note(args) = cli.command else { panic!("expected note") };
        let Some(NoteCommand::Set { text, expires_at }) = args.command else { panic!("expected set") };
        assert_eq!(text, "Review before merging");
        assert_eq!(expires_at.as_deref(), Some("2099-01-01T00:00:00Z"));
        assert!(super::super::Cli::try_parse_from(["st", "note", "clear", "--person", "person/other"]).is_err());
        assert!(super::super::Cli::try_parse_from(["st", "note", "set"]).is_err());
    }
}
