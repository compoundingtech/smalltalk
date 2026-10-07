//! Human entry points and the actions to take after creating durable work.
use std::fmt::Write as _;

use clap::CommandFactory as _;

use super::{Cli, presentation::shell_argument};

const GROUPS: &[(&str, &[&str])] = &[
    (
        "Everyday use",
        &[
            "ui",
            "now",
            "agents",
            "conversations",
            "missions",
            "attention",
            "usage",
            "terminals",
            "machines",
            "launch",
            "lanes",
            "devices",
            "clients",
            "sekrets",
        ],
    ),
    (
        "Inside an agent seat",
        &["work", "gh", "claim", "diagnostic", "trace", "skill"],
    ),
    (
        "Running st on a machine or fleet",
        &[
            "up",
            "service",
            "fleet",
            "replication",
            "apply",
            "sets",
            "backup",
            "doctor",
            "admission",
            "rules",
            "repair",
            "uninstall",
        ],
    ),
    (
        "Plumbing",
        &[
            "schema",
            "subject",
            "documents",
            "blobs",
            "recorder",
            "activity",
            "import",
            "completions",
            "gate",
        ],
    ),
];

/// Only intercept root `help --all`. All other help paths remain clap's native help tree.
pub(super) fn all_help_requested(arguments: &[std::ffi::OsString]) -> bool {
    if !arguments.iter().any(|argument| argument == "--all") {
        return false;
    }
    // Use the real global options to find the root command, so option values such as a
    // device name or endpoint containing "help" cannot accidentally trigger this path.
    let parser = Cli::command().disable_help_subcommand(true).subcommand(
        clap::Command::new("help").arg(
            clap::Arg::new("all")
                .long("all")
                .action(clap::ArgAction::SetTrue),
        ),
    );
    parser
        .try_get_matches_from(arguments)
        .ok()
        .is_some_and(|matches| {
            matches
                .subcommand_matches("help")
                .is_some_and(|help| help.get_flag("all"))
        })
}

pub(super) fn root_help(all: bool) -> String {
    let command = Cli::command();
    let mut output = String::from(
        "See what needs you:\n  st now\n\nRun st in a terminal to open the interface:\n  st\n  st ui --space NAME\n\nStart an agent and talk to it:\n  st agents new NAME --harness claude --attach\n\nSee what is running:\n  st missions ls\n  st agents ls\n\nFrom an agent seat:\n  st skill\n  st work ls --as \"$ST_AGENT\"\n\nRecord an authorized job:\n  Claim an existing step first, or st work start TITLE --as \"$ST_AGENT\".\n  Use a finite mission for dependencies, verification gates or review.\n\nUsage: st [OPTIONS] <COMMAND>\n",
    );
    for (heading, names) in GROUPS {
        if *heading == "Plumbing" && !all {
            continue;
        }
        let _ = writeln!(output, "\n{heading}:");
        for name in *names {
            let subcommand = command
                .find_subcommand(name)
                .expect("help names a CLI command");
            let about = subcommand.get_about().expect("command has a purpose");
            let _ = writeln!(output, "  {name:14} {about}");
        }
    }
    let _ = writeln!(output, "\n{}", st3::skill::message_sender_guidance());
    // Let clap keep the options, defaults and environment variables accurate.
    let mut options = command.clone().help_template("\nOptions:\n{options}");
    output.push('\n');
    output.push_str(&options.render_help().to_string());
    output.push_str("\nUse st help COMMAND for command help; st help --all also lists plumbing.\n");
    output.push_str(
        "\nIssues and suggestions are welcome. Please file an upstream issue:\n  gh issue create --repo compoundingtech/smalltalk\nPull requests are welcome too: https://github.com/compoundingtech/smalltalk\n",
    );
    output
}

pub(super) fn next_steps(state: &str, actions: &[(&str, String)]) -> String {
    let mut output = format!("{state}\n\nNext steps:\n");
    for (label, command) in actions {
        let _ = writeln!(output, "  {label}: {command}");
    }
    output
}

pub(super) fn agent_state(
    state: &str,
    harness: Option<&str>,
    fault: Option<&str>,
    reachability: &str,
) -> String {
    match state {
        "running" => "Ready — the agent is running.".into(),
        "failed" => format!(
            "Could not start — {}.",
            fault.unwrap_or("the agent process failed")
        ),
        "stopped" => "Stopped — the agent is no longer running.".into(),
        "suspended" => {
            "Suspended — the agent stopped at a quiet moment; `st agents resume` brings back its session.".into()
        }
        "waiting" if reachability != "reachable" => {
            "Still starting — the agent process cannot currently be reached.".into()
        }
        "waiting" => match harness {
            Some("unauthenticated" | "needs-login") => {
                "Waiting for you — attach to sign in to the agent's provider.".into()
            }
            _ => "Waiting for you — attach to inspect what the agent needs.".into(),
        },
        _ => match harness {
            None | Some("unobserved") => {
                "Still starting — waiting for the agent process to appear.".into()
            }
            Some("starting") => "Still starting — the agent process is initializing.".into(),
            _ => "Still starting — waiting for the agent to become ready.".into(),
        },
    }
}

pub(super) fn agent_next_steps(subject: &str, actor: &str, state: &str) -> String {
    let subject = shell_argument(subject);
    let attach_actor = if actor.starts_with("person/") {
        format!(" --as {}", shell_argument(actor))
    } else {
        String::new()
    };
    let actor = shell_argument(actor);
    let mut output = next_steps(
        state,
        &[
            (
                "Attach",
                format!("st terminals attach {subject}{attach_actor}"),
            ),
            (
                "Send a message",
                format!("st conversations send {subject} --from {actor} --body 'Hello'"),
            ),
            ("Show", format!("st agents show {subject}")),
            ("Stop", format!("st agents stop {subject} --as {actor}")),
        ],
    );
    output.push_str("\nUse st agents new NAME --attach to attach as soon as the agent starts. Detach with Ctrl+\\.\n");
    output
}

pub(super) fn mission_next_steps(run: &super::MissionRunView) -> String {
    let subject = shell_argument(&run.subject);
    let actor = shell_argument(if run.requester.starts_with("person/") {
        &run.requester
    } else {
        "person/NAME"
    });
    let state = match run.status.as_str() {
        "running" => "Started — the mission is running.".to_owned(),
        "desired" | "starting" => "Still starting — waiting for the mission's agents.".to_owned(),
        "waiting" | "blocked" => run.after.as_ref().map_or_else(
            || "Waiting — inspect the run to see what it needs.".to_owned(),
            |after| format!("Waiting — work starts after {after} completes."),
        ),
        "standing" => "Ready — the mission's agents are standing by.".to_owned(),
        "failed" => "Failed — inspect the run for the reason.".to_owned(),
        "completed" => "Completed — the mission's work has finished.".to_owned(),
        "cancelled" => "Cancelled — the mission has stopped.".to_owned(),
        _ => format!("Mission state: {}.", run.status),
    };
    next_steps(
        &state,
        &[
            ("Follow", format!("st missions show {subject} --follow")),
            ("Show", format!("st missions show {subject}")),
            (
                "Stop",
                format!("st missions cancel {subject} --as {actor} --reason 'No longer needed'"),
            ),
        ],
    )
}

pub(super) fn launch_next_steps(launch: &super::PlanningSessionView) -> String {
    let subject = shell_argument(&launch.subject);
    let requester = shell_argument(&launch.requester);
    let planner = shell_argument(&launch.planner);
    next_steps(
        "Started — the planner is preparing a mission for your review.",
        &[
            ("Show", format!("st launch show {subject}")),
            (
                "Send a message",
                format!("st conversations send {planner} --from {requester} --body 'Hello'"),
            ),
            ("Preview", format!("st launch preview {subject}")),
            (
                "Stop",
                format!("st launch cancel {subject} --as {requester}"),
            ),
        ],
    )
}

pub(super) fn pairing_next_steps(person: &str) -> String {
    let person = shell_argument(person);
    next_steps(
        "Waiting for your device — enter the pairing code in its st client before it expires.",
        &[
            (
                "Complete on the device",
                "st devices complete MEMBER_URL PAIRING_ID --fingerprint SHA256".to_owned(),
            ),
            ("Show paired devices", format!("st devices --as {person}")),
            (
                "Pair again",
                format!("st devices pair DEVICE_NAME --as {person}"),
            ),
        ],
    )
}

pub(super) fn fleet_next_steps(services: bool, sync_state: Option<&str>) -> String {
    let state = if sync_state == Some("verified") {
        "Ready — this machine joined the fleet and completed its first sync."
    } else if sync_state.is_some_and(|state| matches!(state, "failed" | "mismatch")) {
        "Sync failed — this machine joined the fleet; inspect its sync status for the reason."
    } else if services {
        "Still starting — this machine joined the fleet; check its sync status."
    } else {
        "Still starting — this machine joined the fleet; start its services to begin syncing."
    };
    let mut actions = vec![
        ("Show", "st fleet status".into()),
        ("Wait for sync", "st fleet wait".into()),
        ("Check machines", "st machines".into()),
    ];
    if !services {
        actions.push(("Start services", "st service install".into()));
    }
    next_steps(state, &actions)
}

pub(super) fn fleet_created_next_steps(services: bool) -> String {
    let state = if services {
        "Ready — this machine is the fleet's first member."
    } else {
        "Still starting — the fleet was created; start this machine's services."
    };
    let mut actions = vec![
        ("Invite a machine", "st fleet invite NAME".into()),
        ("Show", "st fleet status".into()),
    ];
    if !services {
        actions.push(("Start services", "st service install".into()));
    }
    next_steps(state, &actions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_help_groups_every_public_command_once_and_opens_with_examples() {
        let default = root_help(false);
        let all = root_help(true);
        assert!(default.starts_with("See what needs you:\n  st now\n"));
        assert!(default.contains("st agents new NAME --harness claude --attach"));
        assert!(default.contains("st missions ls\n  st agents ls"));
        assert!(default.find("Everyday use:") < default.find("Inside an agent seat:"));
        assert!(
            default.find("Inside an agent seat:")
                < default.find("Running st on a machine or fleet:")
        );
        assert!(!default.contains("Plumbing:"));
        assert!(all.contains("Plumbing:"));
        let sender_guidance = st3::skill::message_sender_guidance();
        assert_eq!(default.matches(sender_guidance).count(), 1);
        assert_eq!(all.matches(sender_guidance).count(), 1);
        for command in Cli::command()
            .get_subcommands()
            .filter(|command| !command.is_hide_set())
        {
            let row = format!("  {:14} ", command.get_name());
            assert_eq!(all.matches(&row).count(), 1, "{}", command.get_name());
            let plumbing = GROUPS.last().unwrap().1.contains(&command.get_name());
            assert_eq!(default.contains(&row), !plumbing, "{}", command.get_name());
        }
        assert!(default.contains("--endpoint <ENDPOINT>"));
        assert!(default.contains("--daemon-wait <SECONDS>"));
    }

    #[test]
    fn agent_endings_explain_state_and_name_exact_actions() {
        for (state, harness, expected) in [
            ("running", Some("running"), "Ready"),
            ("desired", None, "waiting for the agent process to appear"),
            (
                "starting",
                Some("unobserved"),
                "waiting for the agent process to appear",
            ),
            ("starting", Some("starting"), "initializing"),
            ("waiting", Some("needs-login"), "Waiting for you"),
            ("failed", None, "Could not start"),
        ] {
            let state = agent_state(state, harness, None, "reachable");
            assert!(state.contains(expected), "{state}");
            let output = agent_next_steps("agent/builder.demo", "person/avery", &state);
            for command in [
                "st terminals attach agent/builder.demo",
                "st conversations send agent/builder.demo --from person/avery --body 'Hello'",
                "st agents show agent/builder.demo",
                "st agents stop agent/builder.demo --as person/avery",
                "--attach",
            ] {
                assert!(output.contains(command), "{output}");
            }
            assert!(!output.contains("unobserved"));
        }
    }

    #[test]
    fn fleet_endings_distinguish_sync_and_service_states() {
        let ready = fleet_next_steps(true, Some("verified"));
        assert!(ready.starts_with("Ready"));
        let syncing = fleet_next_steps(true, None);
        assert!(syncing.starts_with("Still starting"));
        assert!(syncing.contains("st fleet wait"));
        assert!(!syncing.contains("st service install"));
        let stopped = fleet_next_steps(false, None);
        assert!(stopped.contains("start its services"));
        assert!(stopped.contains("st service install"));
        assert!(fleet_next_steps(true, Some("failed")).starts_with("Sync failed"));
        assert!(fleet_created_next_steps(true).starts_with("Ready"));
        assert!(fleet_created_next_steps(false).contains("st service install"));
        assert!(fleet_created_next_steps(true).contains("st fleet invite NAME"));
    }

    #[test]
    fn an_unreachable_agent_does_not_imply_a_login_prompt() {
        let state = agent_state("waiting", Some("ready"), None, "unreachable");
        assert!(state.contains("cannot currently be reached"));
    }

    #[test]
    fn commands_quote_subjects_and_actors() {
        let output = agent_next_steps("agent/demo's seat", "person/avery", "Ready");
        assert!(output.contains("'agent/demo'\"'\"'s seat'"));
    }
}
