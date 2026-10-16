//! The argv contract is derived from the same clap graph used to parse commands.
use std::collections::BTreeMap;

use clap::CommandFactory as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

fn actor_argument(command: &clap::Command) -> Option<&clap::Arg> {
    command.get_arguments().find(|argument| {
        argument.get_long() == Some("as")
            || argument
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&"as"))
    })
}

// Probe the real value parser, without other arguments, environment defaults, or conflicts.
// This describes CLI syntax, not the authority a daemon will grant to a caller.
fn accepts_person(argument: &clap::Arg) -> bool {
    clap::Command::new("actor-probe")
        .arg(
            clap::Arg::new("actor")
                .long("as")
                .value_parser(argument.get_value_parser().clone()),
        )
        .try_get_matches_from(["actor-probe", "--as", "person/probe"])
        .is_ok()
}

// These paths carry a configured person without an --as actor argument. Keep this
// small semantic supplement next to the generated syntax, not in an operator wrapper.
// Call-site map (review these handlers when their identity handling changes):
// - st setup: setup::run reads config.person to select default_person, then records
//   the selected person in config.person (crates/st3/src/setup.rs).
// - st ui: main::run_terminal_ui calls stui::run, which falls back to
//   configured_person() and passes that actor to Client::unix_as (crates/stui/src/lib.rs).
// - st ui open: stui::open_in_terminal_ui resolves configured_person() before
//   queueing a request in that person's terminal UI request directory.
// - st note and its subcommands: main's Command::Note passes config.person to
//   directive_note::run, which resolves it with configured_actor before dispatch.
// - st changes except list/show: main's Command::Changes resolves config.person
//   with parse_person_subject before constructing Client::unix_as.
// A parser-only test cannot prove these runtime fallbacks; keep their source
// anchors explicit rather than asserting source text or requiring live setup/UI.
fn implicit_person(path: &str) -> bool {
    path == "st setup"
        || path == "st ui"
        || path == "st ui open"
        || path == "st note"
        || path.starts_with("st note ")
        || (path.starts_with("st changes ")
            && !matches!(path, "st changes list" | "st changes show"))
}

fn walk(command: &clap::Command, path: &str, commands: &mut BTreeMap<String, Value>) {
    let has_children = command
        .get_subcommands()
        .any(|child| child.get_name() != "help");
    if path != "st" && (!has_children || !command.is_subcommand_required_set()) {
        let argument = actor_argument(command);
        let requirement = match argument {
            None => "none",
            Some(argument) if argument.is_required_set() => "required",
            Some(_) => "optional",
        };
        let ignored = path == "st devices complete"
            || argument.is_some_and(|argument| argument.get_id() == "ignored_actor");
        let document_name = path == "st documents put";
        let person = argument.is_some_and(accepts_person);
        let alternate_actor = command
            .get_arguments()
            .find(|argument| argument.get_long() == Some("actor"));
        commands.insert(path.to_owned(), json!({
            "actor": requirement,
            "accepts_person": person,
            "can_carry_person": !path.starts_with("st agents handoff ")
                && (implicit_person(path)
                    || (!ignored && !document_name && person)
                    || alternate_actor.is_some_and(accepts_person)),
            "as_role": if document_name { "document_name" } else if ignored { "ignored" } else if argument.is_some() { "actor" } else { "none" },
            "alternate_actor_option": alternate_actor.and_then(clap::Arg::get_long),
            "actor_environment": argument.and_then(|argument| argument.get_env()).map(|name| name.to_string_lossy()),
            "ignores_actor": ignored,
        }));
    }
    for child in command
        .get_subcommands()
        .filter(|child| child.get_name() != "help")
    {
        walk(child, &format!("{path} {}", child.get_name()), commands);
    }
}

pub(super) fn generate(mut command: clap::Command) -> Value {
    command.build();
    let mut commands = BTreeMap::new();
    walk(&command, "st", &mut commands);
    let inventory = super::read_contract_inventory_for_command(command);
    let contract = json!({
        "kind": "st3.actor-contract.v1",
        "version": 1,
        "semantics": {
            "actor": "Whether this command accepts --as in argv; required can also be supplied by actor_environment. Optional does not mean that the operation needs no identity.",
            "accepts_person": "Whether the --as value parser accepts person/NAME. This is not a grant of person authority; runtime and daemon checks still apply.",
            "can_carry_person": "Whether this command can carry a person actor, through an actor option or a configured-person fallback. It is a capability, not a grant: ST_AGENT identity guards and daemon authorization still apply.",
            "as_role": "The --as spelling is also used for a document name and for ignored read compatibility flags; these do not carry an actor.",
            "identity_resolution": "configured_actor resolves explicit actor, then ST_AGENT when it starts with agent/, then configured person. configured_human resolves explicit or configured person. Command-specific defaults and guards remain unchanged.",
            "scope": "Canonical clap command paths, including hidden commands and callable parents; aliases resolve to their canonical path.",
        },
        "commands": commands,
        "aliases": inventory["aliases"],
    });
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&contract).expect("contract JSON"))
    );
    json!({
        "actor_semantics": "actor: none means no --as syntax",
        "contract": contract,
        "contract_sha256": digest,
    })
}

pub(super) fn print(effective: bool) -> anyhow::Result<()> {
    let mut value = generate(super::Cli::command());
    if effective {
        let (config, _) = st3::config::Config::load_client_unvalidated(None)?;
        value["effective"] = json!({
            "instance": st3::instance::current().map(|instance| instance.name().to_owned()),
            "config_path": st3::config::Config::default_path(),
            "state_dir": config.state_dir,
        });
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_contract_records_runtime_person_exceptions() {
        let value = generate(super::super::Cli::command());
        let commands = &value["contract"]["commands"];
        for path in ["st conversations send", "st conversations reply"] {
            assert_eq!(commands[path]["actor"], "required", "{path}");
            assert_eq!(commands[path]["can_carry_person"], true, "{path}");
        }
        assert_eq!(commands["st documents put"]["as_role"], "document_name");
        assert_eq!(commands["st documents put"]["can_carry_person"], false);
        assert_eq!(commands["st missions show"]["can_carry_person"], false);
        assert_eq!(commands["st work show"]["can_carry_person"], false);
        assert_eq!(commands["st note"]["alternate_actor_option"], "actor");
        for path in [
            "st note",
            "st note set",
            "st note clear",
            "st setup",
            "st ui",
            "st ui open",
            "st changes sync",
        ] {
            assert_eq!(commands[path]["can_carry_person"], true, "{path}");
        }
        for path in [
            "st changes list",
            "st changes show",
            "st agents handoff write",
            "st agents handoff ready",
            "st agents handoff show",
        ] {
            assert_eq!(commands[path]["can_carry_person"], false, "{path}");
        }
        assert!(st3::handoff::document_name("person/probe").is_none());
        assert!(super::super::configured_human(Some("person/probe"), None, "contract").is_ok());
        assert!(super::super::configured_actor(Some("person/probe"), None, "contract").is_ok());
    }

    #[test]
    fn actor_contract_records_real_command_aliases() {
        let value = generate(super::super::Cli::command());
        assert_eq!(value["contract"]["aliases"]["st alerts"], json!(["attention"]));
        assert_eq!(value["contract"]["aliases"]["st changes list"], json!(["ls"]));
        for (alias, canonical) in [
            ("attention ls", "alerts ls"),
            ("changes ls", "changes list"),
        ] {
            // `changes list` needs a repository, so supply it with the alias.
            let mut argv = vec!["st"];
            argv.extend(alias.split_whitespace());
            if canonical == "changes list" {
                argv.extend(["--repository", "probe"]);
            }
            let matches = super::super::Cli::command()
                .try_get_matches_from(argv)
                .unwrap();
            let mut matches = &matches;
            for name in canonical.split_whitespace() {
                assert_eq!(matches.subcommand_name(), Some(name));
                matches = matches.subcommand_matches(name).unwrap();
            }
        }
    }

    fn assert_reviewed_configured_person_families(command: clap::Command) {
        let inventory = super::super::read_contract_inventory_for_command(command);
        let paths = inventory["commands"].as_array().unwrap();
        let reviewed = [
            "st changes list",
            "st changes show",
            "st changes sync",
            "st note",
            "st note clear",
            "st note set",
            "st setup",
            "st ui",
            "st ui open",
        ];
        let actual = paths
            .iter()
            .map(|path| path.as_str().unwrap())
            .filter(|path| {
                ["st setup", "st ui", "st note", "st changes"]
                    .into_iter()
                    .any(|family| *path == family || path.starts_with(&format!("{family} ")))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual, reviewed,
            "configured-person command family changed; review the runtime handlers in the call-site map and update this coverage"
        );
        let supplemented = paths
            .iter()
            .map(|path| path.as_str().unwrap())
            .filter(|path| implicit_person(path))
            .collect::<Vec<_>>();
        assert_eq!(
            supplemented,
            reviewed
                .into_iter()
                .filter(|path| !matches!(*path, "st changes list" | "st changes show"))
                .collect::<Vec<_>>(),
            "configured-person supplement has missing or unreviewed paths"
        );
    }

    #[test]
    fn configured_person_supplement_covers_the_reviewed_clap_families() {
        assert_reviewed_configured_person_families(super::super::Cli::command());
    }

    #[test]
    #[should_panic(expected = "configured-person command family changed")]
    fn configured_person_guard_rejects_new_ui_descendants() {
        assert_reviewed_configured_person_families(
            super::super::Cli::command().mut_subcommand("ui", |command| {
                command.subcommand(clap::Command::new("unreviewed-probe"))
            }),
        );
    }

    #[test]
    #[should_panic(expected = "configured-person command family changed")]
    fn configured_person_guard_rejects_new_setup_descendants() {
        assert_reviewed_configured_person_families(
            super::super::Cli::command().mut_subcommand("setup", |command| {
                command.subcommand(clap::Command::new("unreviewed-probe"))
            }),
        );
    }

    #[test]
    #[should_panic(expected = "configured-person command family changed")]
    fn configured_person_guard_rejects_removed_ui_descendants() {
        assert_reviewed_configured_person_families(
            super::super::Cli::command().mut_subcommand("ui", |_| clap::Command::new("ui")),
        );
    }

    fn assert_published_classification(value: &Value) {
        let path = std::path::Path::new(test_env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/st3/actor-contract.json");
        let published: Value = serde_json::from_slice(
            &std::fs::read(path).expect("regenerate with: st actor-contract --json | jq 'del(.contract_sha256)' > docs/st3/actor-contract.json"),
        )
        .unwrap();
        assert!(published.get("contract_sha256").is_none());
        assert_eq!(
            value["contract"],
            published["contract"],
            "the CLI actor surface changed; review every changed command and regenerate with: st actor-contract --json | jq 'del(.contract_sha256)' > docs/st3/actor-contract.json; resolve rebase conflicts by regenerating, never by hand-merging"
        );
    }

    #[test]
    fn actor_contract_has_no_unclassified_commands() {
        assert_published_classification(&generate(super::super::Cli::command()));
    }

    #[test]
    #[should_panic(expected = "the CLI actor surface changed")]
    fn actor_contract_new_command_fails_the_published_classification_guard() {
        assert_published_classification(&generate(
            super::super::Cli::command().subcommand(clap::Command::new("unclassified-probe")),
        ));
    }

    #[test]
    fn actor_contract_matches_actual_as_acceptance() {
        fn check(command: &clap::Command, path: &str, contract: &Value) {
            let callable = path != "st"
                && (!command.get_subcommands().any(|child| child.get_name() != "help")
                    || !command.is_subcommand_required_set());
            if callable {
                let entry = contract["contract"]["commands"]
                    .get(path)
                    .unwrap_or_else(|| panic!("callable command missing from actor contract: {path}"));
                let mut argv = path.split_whitespace().collect::<Vec<_>>();
                argv.extend(["--as", "person/probe"]);
                let actual = super::super::Cli::command().try_get_matches_from(argv);
                if entry["actor"] == "none" {
                    assert_eq!(
                        actual.unwrap_err().kind(),
                        clap::error::ErrorKind::UnknownArgument,
                        "{path}"
                    );
                } else if let Err(error) = actual {
                    assert_ne!(
                        error.kind(),
                        clap::error::ErrorKind::UnknownArgument,
                        "{path}: {error}"
                    );
                }
                // Keep the real argument's required bit and value parser. Remove unrelated
                // arguments so a missing positional or conflicting option cannot hide --as.
                let mut parser = clap::Command::new("probe");
                if let Some(argument) = actor_argument(command) {
                    parser = parser.arg(
                        clap::Arg::new("actor")
                            .long("as")
                            .required(argument.is_required_set())
                            .value_parser(argument.get_value_parser().clone()),
                    );
                }
                let supplied =
                    parser
                        .clone()
                        .try_get_matches_from(["probe", "--as", "person/probe"]);
                if entry["actor"] == "none" {
                    assert_eq!(
                        supplied.unwrap_err().kind(),
                        clap::error::ErrorKind::UnknownArgument,
                        "{path}"
                    );
                } else {
                    assert_eq!(
                        supplied.is_ok(),
                        entry["accepts_person"].as_bool().unwrap(),
                        "{path}"
                    );
                    let absent = parser.try_get_matches_from(["probe"]);
                    if entry["actor"] == "required" {
                        assert_eq!(
                            absent.unwrap_err().kind(),
                            clap::error::ErrorKind::MissingRequiredArgument,
                            "{path}"
                        );
                    } else {
                        assert!(absent.is_ok(), "{path}");
                    }
                }
            } else {
                assert!(
                    contract["contract"]["commands"].get(path).is_none(),
                    "non-callable command classified: {path}"
                );
            }
            for child in command
                .get_subcommands()
                .filter(|child| child.get_name() != "help")
            {
                check(child, &format!("{path} {}", child.get_name()), contract);
            }
        }
        let mut command = super::super::Cli::command();
        command.build();
        check(&command, "st", &generate(command.clone()));
    }
}
