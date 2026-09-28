//! An invented fleet for `stui --demo`. Nothing here reads st3.
//!
//! Every name is made up. The world moves: one agent streams a reply, a tool runs and
//! finishes, a message arrives, so the scrolling and "new below" behaviour can be felt.

use super::view::*;
use std::collections::BTreeMap;

fn s(text: &str) -> String {
    text.to_owned()
}

pub fn world() -> World {
    let mut conversations = BTreeMap::new();
    conversations.insert(
        s("agent/fleet/atlas/builder"),
        Load::Ready(atlas_conversation()),
    );
    conversations.insert(s("agent/fleet/cos"), Load::Ready(cos_conversation()));
    conversations.insert(
        s("agent/fleet/docs/writer"),
        Load::Ready(docs_conversation()),
    );
    conversations.insert(
        s("agent/fleet/harbor/reviewer"),
        Load::Ready(vec![
            Entry {
                id: s("h1"),
                at: s("09:12"),
                body: Body::Event(s("step review ready · harbor/pull-request-review")),
            },
            Entry {
                id: s("h2"),
                at: s("09:13"),
                body: Body::Assistant(s(
                    "Reviewed **#218**. Two findings, both small:\n\n1. `retry_after` is read as seconds but the server sends milliseconds.\n2. The new test sleeps for a real second; use the paused clock instead.\n\nI asked for changes on the pull request.",
                )),
            },
        ]),
    );
    for (id, text) in [
        ("agent/fleet/release/captain", "Tagging the weekly release."),
        (
            "agent/fleet/rekey/worker",
            "Inventory done: 5 signing keys, 3 services read them.",
        ),
        (
            "agent/fleet/pi/sketcher",
            "Sketched three layouts for the settings screen; they are in ~/src/sketches.",
        ),
    ] {
        conversations.insert(
            s(id),
            Load::Ready(vec![Entry {
                id: s("only"),
                at: s("08:00"),
                body: Body::Assistant(s(text)),
            }]),
        );
    }
    conversations.insert(s("agent/fleet/atlas/indexer"), Load::Ready(vec![]));
    conversations.insert(s("session/unmanaged-1"), Load::Failed(s(
        "This codex process was found running on lark, but st cannot tell which session file it writes, so there is no conversation to show. Start it through st to see it here.",
    )));
    World {
        person: s("person/robin"),
        host: s("lark"),
        link: Link::Live,
        attention: Load::Ready(attention()),
        agents: Load::Ready(agents()),
        missions: Load::Ready(missions()),
        machines: Load::Ready(machines()),
        worktrees: Load::Ready(worktrees()),
        devices: Load::Ready(vec![
            Device {
                id: s("device/phone"),
                name: s("Robin's phone"),
                state: s("active"),
                scopes: vec![s("full control")],
                expires: s("in 83 days"),
            },
            Device {
                id: s("device/tablet"),
                name: s("Old tablet"),
                state: s("active"),
                scopes: vec![s("read"), s("attention")],
                expires: s("in 4 days"),
            },
        ]),
        conversations,
        quiet_missions: 6,
    }
}

/// The same world before anything has loaded, so the first second is honest.
pub fn loading() -> World {
    World {
        person: s("person/robin"),
        host: s("lark"),
        link: Link::Connecting,
        attention: Load::Loading,
        agents: Load::Loading,
        missions: Load::Loading,
        machines: Load::Loading,
        worktrees: Load::Loading,
        devices: Load::Loading,
        conversations: BTreeMap::new(),
        quiet_missions: 0,
    }
}

fn attention() -> Vec<Attention> {
    vec![
        Attention {
            id: s("attention/1"),
            tier: Tier::Stopped,
            title: s("Approve the atlas store cut-over"),
            waiting: Some(s("Atlas Builder on harbor")),
            age: s("33m"),
            mission: Some(s("mission/fleet/atlas/store-move")),
            agent: Some(s("agent/fleet/atlas/builder")),
            actions: vec![],
            related: vec![],
            raised_by: None,
            kind: AttentionKind::Review {
                question: s(
                    "The row counts agree and the contract check passed. Cut over to the new store?",
                ),
                because: s(
                    "cut-over cannot start until you answer; the old store stays read-only meanwhile",
                ),
                look_at: vec![
                    (s("report"), s("1,204,881 rows on both sides, 0 mismatches")),
                    (
                        s("contract"),
                        s("42 of 42 public queries return identical results"),
                    ),
                    (
                        s("rollback"),
                        s("the old store is frozen and kept for 7 days"),
                    ),
                ],
                step: s("cut-over"),
            },
        },
        Attention {
            id: s("attention/2"),
            tier: Tier::Stopped,
            title: s("Feedback on the new pricing page"),
            waiting: Some(s("Docs Writer on harbor")),
            age: s("12m"),
            mission: Some(s("mission/fleet/site/pricing-page")),
            agent: Some(s("agent/fleet/docs/writer")),
            actions: vec![],
            related: vec![],
            raised_by: None,
            kind: AttentionKind::Feedback {
                question: s(
                    "Here is the draft of the pricing page. What should change before it goes live?",
                ),
                subject: s("site/pricing · draft 2"),
                excerpt: vec![
                    s("# Simple pricing"),
                    s(""),
                    s("Start free. Pay when your team grows."),
                    s(""),
                    s("- **Solo** — free forever, one workspace"),
                    s("- **Team** — $12 per person per month"),
                    s("- **Company** — talk to us"),
                ],
                link: Some(s("https://preview.example.com/pricing")),
            },
        },
        Attention {
            id: s("attention/3"),
            tier: Tier::Today,
            title: s("Launch harbor/nightly-audit"),
            waiting: Some(s("Planner on lark")),
            age: s("1h"),
            mission: None,
            agent: Some(s("agent/fleet/planner")),
            actions: vec![],
            related: vec![],
            raised_by: None,
            kind: AttentionKind::Launch {
                planner: s("Planner"),
                name: s("harbor/nightly-audit"),
                preview: Load::Ready(MissionPreview {
                    name: s("harbor/nightly-audit"),
                    goals: vec![
                        s("Every night, audit harbor's dependencies for known advisories."),
                        s("Open one pull request per fix; never merge without review."),
                    ],
                    steps: vec![
                        PreviewStep {
                            name: s("scan"),
                            assignee: s("Auditor"),
                            after: vec![],
                            asks_you: false,
                        },
                        PreviewStep {
                            name: s("fix"),
                            assignee: s("Auditor"),
                            after: vec![s("scan")],
                            asks_you: false,
                        },
                        PreviewStep {
                            name: s("review"),
                            assignee: s("Harbor Reviewer"),
                            after: vec![s("fix")],
                            asks_you: false,
                        },
                        PreviewStep {
                            name: s("merge"),
                            assignee: s("you"),
                            after: vec![s("review")],
                            asks_you: true,
                        },
                    ],
                    agents: vec![
                        PreviewAgent {
                            name: s("Auditor"),
                            harness: Harness::Codex,
                            host: s("harbor"),
                        },
                        PreviewAgent {
                            name: s("Harbor Reviewer"),
                            harness: Harness::Claude,
                            host: s("harbor"),
                        },
                    ],
                    workspace: s("~/src/harbor"),
                }),
            },
        },
        Attention {
            id: s("attention/4"),
            tier: Tier::Alert,
            title: s("Release Captain keeps restarting"),
            waiting: None,
            age: s("8m"),
            mission: Some(s("mission/fleet/release/weekly")),
            agent: Some(s("agent/fleet/release/captain")),
            actions: vec![],
            related: vec![],
            raised_by: None,
            kind: AttentionKind::Fault {
                what: s(
                    "Release Captain exited 4 times in 10 minutes and st stopped restarting it.",
                ),
                because: s("the weekly release cannot tag until this seat is back"),
                fix: Some(s(
                    "Its API key expired at 08:40. Renew the key, then restart the seat.",
                )),
                source: s("seat release-captain on harbor"),
            },
        },
        Attention {
            id: s("attention/5"),
            tier: Tier::Today,
            title: s("Add an audit step to the rekey mission"),
            waiting: Some(s("Rekey Worker on harbor")),
            age: s("19m"),
            mission: Some(s("mission/fleet/rekey")),
            agent: Some(s("agent/fleet/rekey/worker")),
            actions: vec![],
            related: vec![],
            raised_by: None,
            kind: AttentionKind::Revision {
                reason: s(
                    "A key rotated mid-run last week and nobody noticed. An audit step would catch it.",
                ),
                changes: vec![
                    (
                        '+',
                        s("step \"audit\" { assigned-to \"Rekey Worker\"; after \"rotate\" }"),
                    ),
                    (
                        '~',
                        s("step \"report\" { after \"audit\" }   (was: after \"rotate\")"),
                    ),
                ],
            },
        },
        Attention {
            id: s("attention/6"),
            tier: Tier::Later,
            title: s("Weekly usage is up 18%"),
            waiting: None,
            age: s("2h"),
            mission: None,
            agent: Some(s("agent/fleet/cos")),
            actions: vec![],
            related: vec![],
            raised_by: None,
            kind: AttentionKind::Message {
                from: s("Chief of Staff"),
                body: s(
                    "Usage this week is **up 18%**, almost all from the atlas store move (reruns of the compare step).\n\nNo action needed; it should fall back once the cut-over is done.",
                ),
            },
        },
    ]
}

#[allow(clippy::too_many_arguments)]
fn agent(
    id: &str,
    name: &str,
    harness: Harness,
    state: AgentState,
    host: &str,
    worktree: Option<&str>,
    mission: Option<(&str, &str)>,
    activity: &str,
) -> Agent {
    Agent {
        id: s(id),
        name: s(name),
        harness,
        state,
        host: s(host),
        worktree: worktree.map(s),
        mission: mission.map(|(mission, _)| s(mission)),
        step: mission.map(|(_, step)| s(step)),
        activity: s(activity),
        unmanaged: false,
        parent: None,
        details: AgentDetails::default(),
        terminal: !matches!(state, AgentState::Stopped),
    }
}

fn agents() -> Vec<Agent> {
    let mut list = vec![
        agent(
            "agent/fleet/atlas/builder",
            "Atlas Builder",
            Harness::Claude,
            AgentState::NeedsYou,
            "harbor",
            Some("~/src/atlas--store-move"),
            Some(("mission/fleet/atlas/store-move", "cut-over")),
            "33m",
        ),
        agent(
            "agent/fleet/docs/writer",
            "Docs Writer",
            Harness::Claude,
            AgentState::NeedsYou,
            "harbor",
            Some("~/src/site--pricing"),
            Some(("mission/fleet/site/pricing-page", "draft")),
            "12m",
        ),
        agent(
            "agent/fleet/release/captain",
            "Release Captain",
            Harness::Codex,
            AgentState::Fault,
            "harbor",
            Some("~/src/atlas"),
            Some(("mission/fleet/release/weekly", "tag")),
            "8m",
        ),
        agent(
            "agent/fleet/cos",
            "Chief of Staff",
            Harness::Claude,
            AgentState::Working,
            "lark",
            None,
            None,
            "now",
        ),
        agent(
            "agent/fleet/harbor/reviewer",
            "Harbor Reviewer",
            Harness::Claude,
            AgentState::Working,
            "harbor",
            Some("~/src/harbor--review-218"),
            Some(("mission/fleet/harbor/pull-request-review", "review")),
            "1m",
        ),
        agent(
            "agent/fleet/rekey/worker",
            "Rekey Worker",
            Harness::Omp,
            AgentState::Idle,
            "harbor",
            Some("~/src/harbor"),
            Some(("mission/fleet/rekey", "rotate")),
            "19m",
        ),
        agent(
            "agent/fleet/pi/sketcher",
            "Sketcher",
            Harness::Pi,
            AgentState::Idle,
            "lark",
            Some("~/src/sketches"),
            None,
            "3h",
        ),
        agent(
            "agent/fleet/atlas/indexer",
            "Atlas Indexer",
            Harness::Codex,
            AgentState::Stopped,
            "harbor",
            None,
            None,
            "2d",
        ),
    ];
    list.push(agent(
        "agent/fleet/planner",
        "Planner",
        Harness::Claude,
        AgentState::NeedsYou,
        "lark",
        None,
        None,
        "1h",
    ));
    list[4].parent = Some(s("agent/fleet/cos"));
    list.push(Agent {
        id: s("session/unmanaged-1"),
        name: s("codex in ~/src/scratch"),
        harness: Harness::Codex,
        state: AgentState::Unknown,
        host: s("lark"),
        worktree: Some(s("~/src/scratch")),
        mission: None,
        step: None,
        activity: s("5m"),
        unmanaged: true,
        parent: None,
        details: AgentDetails::default(),
        terminal: false,
    });
    let detail =
        |goal: &str, claimed: &str, next: Option<&str>, queue: &[&str], state: &str| AgentDetails {
            goal: Some(s(goal)),
            claimed: Some(s(claimed)),
            next: next.map(s),
            queue: queue.iter().map(|item| s(item)).collect(),
            queued: queue.len() as u64,
            harness_state: Some(s(state)),
            runtime: Some(s("running · incarnation 3")),
            fault: None,
            under: None,
        };
    list[0].details = detail(
        "Cut over to the new store once a person approves.",
        "33m ago",
        Some("Move the atlas store › cleanup"),
        &["Atlas nightly build › build (tonight)"],
        "idle, waiting for review",
    );
    list[1].details = detail(
        "Draft the pricing page and get feedback.",
        "12m ago",
        Some("A new pricing page › publish"),
        &[],
        "idle, waiting for feedback",
    );
    list[2].details = AgentDetails {
        goal: Some(s("Tag this week's release.")),
        claimed: Some(s("1h ago")),
        next: Some(s("Weekly release › publish")),
        queue: vec![],
        queued: 0,
        harness_state: Some(s("exited 4 times in 10 minutes")),
        runtime: Some(s("restarts paused")),
        fault: Some(s("401 Unauthorized: the API key expired at 08:40.")),
        under: None,
    };
    list[3].details = detail(
        "Keep Robin's fleet moving; answer questions.",
        "standing",
        None,
        &["Rotate harbor's signing keys › report"],
        "working",
    );
    list[4].details = detail(
        "Review pull request #218.",
        "4m ago",
        Some("Review harbor #218 › route"),
        &[],
        "working",
    );
    list[4].details.under = Some(s("Chief of Staff"));
    list
}

fn step(
    name: &str,
    state: StepState,
    owner: Option<&str>,
    after: &[&str],
    age: &str,
    note: Option<&str>,
) -> Step {
    Step {
        name: s(name),
        state,
        owner: owner.map(s),
        note: note.map(s),
        after: after.iter().map(|name| s(name)).collect(),
        age: s(age),
        goals: vec![format!(
            "Finish the {name} step and record what it produced."
        )],
        constraints: vec![s("Never write to the old store.")],
        gates: vec![],
        attempt: 1,
        blockers: vec![],
    }
}

const ATLAS_KDL: &str = r#"version 2

mission "fleet/atlas/store-move" state="ready" {
  goal "Move every row to the new store without losing a value."
  goal "Prove the two stores agree before anything cuts over."
  goal "Leave the old store readable until a person says otherwise."
  constraint "Never write to the old store."

  step "snapshot" { assigned-to "agent/fleet/atlas/builder" }
  step "convert" {
    assigned-to "agent/fleet/atlas/builder"
    depends-on { step "snapshot" completed }
    gate "conversion-check" { exec "atlas-cli check --converted" }
  }
  step "compare" {
    assigned-to "agent/fleet/atlas/builder"
    depends-on { step "convert" completed }
  }
  step "cut-over" {
    agentless
    depends-on { step "compare" completed }
    gate "cut-over-review" type="human" {
      reviewer "person/robin"
      question "The row counts agree and the contract check passed. Cut over?"
      review "doc/atlas/compare-report"
    }
  }
  step "cleanup" {
    assigned-to "agent/fleet/atlas/builder"
    depends-on { step "cut-over" completed }
  }
}
"#;

fn missions() -> Vec<Mission> {
    let mut missions = all_missions();
    // Gates and retries worth showing when a step is opened.
    if let Some(step) = missions[0]
        .steps
        .iter_mut()
        .find(|step| step.name == "cut-over")
    {
        step.gates = vec![s(
            "human review by robin: \"The row counts agree and the contract check passed. Cut over?\"",
        )];
        step.goals = vec![s(
            "Switch reads and writes to the new store once a person approves.",
        )];
    }
    if let Some(step) = missions[0]
        .steps
        .iter_mut()
        .find(|step| step.name == "convert")
    {
        step.gates = vec![s("conversion-check: atlas-cli check --converted (passed)")];
    }
    if let Some(step) = missions[2].steps.iter_mut().find(|step| step.name == "tag") {
        step.attempt = 4;
        step.blockers = vec![s("seat release-captain on harbor is not running")];
    }
    missions
}

fn all_missions() -> Vec<Mission> {
    vec![
        Mission {
            id: s("mission/fleet/atlas/store-move"),
            title: s("Move the atlas store"),
            word: Word::Decision,
            age: s("33m"),
            host: s("harbor"),
            goals: vec![
                s("Move every row to the new store without losing a value."),
                s("Prove the two stores agree before anything cuts over."),
                s("Leave the old store readable until a person says otherwise."),
            ],
            steps: vec![
                step(
                    "snapshot",
                    StepState::Done,
                    Some("Atlas Builder"),
                    &[],
                    "3h",
                    None,
                ),
                step(
                    "convert",
                    StepState::Done,
                    Some("Atlas Builder"),
                    &["snapshot"],
                    "2h",
                    Some("conversion check passed"),
                ),
                step(
                    "compare",
                    StepState::Done,
                    Some("Atlas Builder"),
                    &["convert"],
                    "41m",
                    Some("0 mismatches in 1,204,881 rows"),
                ),
                step(
                    "cut-over",
                    StepState::NeedsYou,
                    None,
                    &["compare"],
                    "33m",
                    Some("waiting for your review"),
                ),
                step(
                    "cleanup",
                    StepState::Pending,
                    Some("Atlas Builder"),
                    &["cut-over"],
                    "",
                    None,
                ),
            ],
            agents: vec![s("agent/fleet/atlas/builder")],
            decision: Some(s("attention/1")),
            worktree: Some(s("~/src/atlas--store-move")),
            parent: None,
            system: false,
            kdl: Some(s(ATLAS_KDL)),
        },
        Mission {
            id: s("mission/fleet/site/pricing-page"),
            title: s("A new pricing page"),
            word: Word::Decision,
            age: s("12m"),
            host: s("harbor"),
            goals: vec![s("Publish a pricing page the team agrees on.")],
            steps: vec![
                step(
                    "draft",
                    StepState::NeedsYou,
                    Some("Docs Writer"),
                    &[],
                    "12m",
                    Some("asked for your feedback"),
                ),
                step(
                    "publish",
                    StepState::Pending,
                    Some("Docs Writer"),
                    &["draft"],
                    "",
                    None,
                ),
            ],
            agents: vec![s("agent/fleet/docs/writer")],
            decision: Some(s("attention/2")),
            worktree: Some(s("~/src/site--pricing")),
            parent: None,
            system: false,
            kdl: None,
        },
        Mission {
            id: s("mission/fleet/release/weekly"),
            title: s("Weekly release"),
            word: Word::Stalled,
            age: s("8m"),
            host: s("harbor"),
            goals: vec![s("Tag, build and publish this week's release.")],
            steps: vec![
                step(
                    "changelog",
                    StepState::Done,
                    Some("Release Captain"),
                    &[],
                    "1h",
                    None,
                ),
                step(
                    "tag",
                    StepState::Failed,
                    Some("Release Captain"),
                    &["changelog"],
                    "8m",
                    Some("seat keeps restarting"),
                ),
                step(
                    "publish",
                    StepState::Pending,
                    Some("Release Captain"),
                    &["tag"],
                    "",
                    None,
                ),
            ],
            agents: vec![s("agent/fleet/release/captain")],
            decision: None,
            worktree: Some(s("~/src/atlas")),
            parent: None,
            system: false,
            kdl: None,
        },
        Mission {
            id: s("mission/fleet/harbor/pull-request-review"),
            title: s("Review harbor #218"),
            word: Word::Working,
            age: s("1m"),
            host: s("harbor"),
            goals: vec![s("Review pull request #218 and route the result.")],
            steps: vec![
                step(
                    "review",
                    StepState::Working,
                    Some("Harbor Reviewer"),
                    &[],
                    "4m",
                    None,
                ),
                step(
                    "route",
                    StepState::Pending,
                    Some("Harbor Reviewer"),
                    &["review"],
                    "",
                    None,
                ),
            ],
            agents: vec![s("agent/fleet/harbor/reviewer")],
            decision: None,
            worktree: Some(s("~/src/harbor--review-218")),
            parent: None,
            system: false,
            kdl: None,
        },
        Mission {
            id: s("mission/fleet/rekey"),
            title: s("Rotate harbor's signing keys"),
            word: Word::Queued,
            age: s("19m"),
            host: s("harbor"),
            goals: vec![s(
                "Rotate every signing key and prove nothing still uses the old ones.",
            )],
            steps: vec![
                step(
                    "inventory",
                    StepState::Done,
                    Some("Rekey Worker"),
                    &[],
                    "1h",
                    None,
                ),
                step(
                    "rotate",
                    StepState::Ready,
                    None,
                    &["inventory"],
                    "19m",
                    Some("queued for Rekey Worker, which finishes harbor/nightly first"),
                ),
                step(
                    "report",
                    StepState::Pending,
                    Some("Rekey Worker"),
                    &["rotate"],
                    "",
                    None,
                ),
            ],
            agents: vec![s("agent/fleet/rekey/worker")],
            decision: None,
            worktree: Some(s("~/src/harbor")),
            parent: None,
            system: false,
            kdl: None,
        },
        Mission {
            id: s("mission/fleet/docs/handbook"),
            title: s("Keep the handbook current"),
            word: Word::Idle,
            age: s("2h"),
            host: s("harbor"),
            goals: vec![s(
                "Update the handbook when a mission changes how the fleet works.",
            )],
            steps: vec![step(
                "watch",
                StepState::Waiting,
                Some("Docs Writer"),
                &[],
                "2h",
                Some("standing; wakes on merged changes"),
            )],
            agents: vec![s("agent/fleet/docs/writer")],
            decision: None,
            worktree: None,
            parent: None,
            system: false,
            kdl: None,
        },
        Mission {
            id: s("mission/fleet/atlas/nightly"),
            title: s("Atlas nightly build"),
            word: Word::Done,
            age: s("6h"),
            host: s("harbor"),
            goals: vec![s("Build and test atlas every night.")],
            steps: vec![
                step(
                    "build",
                    StepState::Done,
                    Some("Atlas Indexer"),
                    &[],
                    "6h",
                    None,
                ),
                step(
                    "test",
                    StepState::Done,
                    Some("Atlas Indexer"),
                    &["build"],
                    "6h",
                    None,
                ),
            ],
            agents: vec![],
            decision: None,
            worktree: None,
            parent: None,
            system: false,
            kdl: None,
        },
    ]
}

fn machines() -> Vec<Machine> {
    vec![
        Machine {
            name: s("lark"),
            online: true,
            platform: s("macOS · arm64"),
            seen: s("now"),
            load: Some(s("2 agents")),
            links: vec![
                (s("harbor"), true, s("direct · 18ms")),
                (s("wren"), false, s("last seen 2d ago")),
            ],
            you_are_here: true,
        },
        Machine {
            name: s("harbor"),
            online: true,
            platform: s("linux · x86_64"),
            seen: s("4s ago"),
            load: Some(s("6 agents · 41% cpu")),
            links: vec![
                (s("lark"), true, s("direct · 18ms")),
                (s("wren"), false, s("last seen 2d ago")),
            ],
            you_are_here: false,
        },
        Machine {
            name: s("wren"),
            online: false,
            platform: s("linux · arm64"),
            seen: s("2d ago"),
            load: None,
            links: vec![],
            you_are_here: false,
        },
    ]
}

fn worktrees() -> Vec<Worktree> {
    let tree = |path: &str,
                host: &str,
                branch: &str,
                ahead,
                behind,
                dirty,
                agents: &[&str],
                missions: &[&str]| Worktree {
        path: s(path),
        host: s(host),
        branch: s(branch),
        ahead,
        behind,
        dirty,
        agents: agents.iter().map(|v| s(v)).collect(),
        missions: missions.iter().map(|v| s(v)).collect(),
    };
    vec![
        tree(
            "~/src/atlas--store-move",
            "harbor",
            "agent/store-move",
            7,
            0,
            0,
            &["agent/fleet/atlas/builder"],
            &["mission/fleet/atlas/store-move"],
        ),
        tree(
            "~/src/site--pricing",
            "harbor",
            "agent/pricing-page",
            2,
            1,
            3,
            &["agent/fleet/docs/writer"],
            &["mission/fleet/site/pricing-page"],
        ),
        tree(
            "~/src/harbor--review-218",
            "harbor",
            "pr/218",
            0,
            0,
            0,
            &["agent/fleet/harbor/reviewer"],
            &["mission/fleet/harbor/pull-request-review"],
        ),
        tree(
            "~/src/harbor",
            "harbor",
            "main",
            0,
            3,
            0,
            &["agent/fleet/rekey/worker"],
            &["mission/fleet/rekey"],
        ),
        tree(
            "~/src/atlas",
            "harbor",
            "main",
            0,
            0,
            1,
            &["agent/fleet/release/captain"],
            &["mission/fleet/release/weekly"],
        ),
        tree(
            "~/src/sketches",
            "lark",
            "main",
            0,
            0,
            5,
            &["agent/fleet/pi/sketcher"],
            &[],
        ),
        tree(
            "~/src/scratch",
            "lark",
            "(unknown)",
            0,
            0,
            0,
            &["session/unmanaged-1"],
            &[],
        ),
    ]
}

fn e(id: &str, at: &str, body: Body) -> Entry {
    Entry {
        id: s(id),
        at: s(at),
        body,
    }
}

fn cos_conversation() -> Vec<Entry> {
    vec![
        e(
            "c1",
            "08:02",
            Body::User(s("Morning. What happened overnight?")),
        ),
        e(
            "c2",
            "08:02",
            Body::Thinking(s(
                "Check missions that changed since 22:00, then faults, then anything waiting on Robin.",
            )),
        ),
        e(
            "c3",
            "08:02",
            Body::Tool {
                title: s("$ st missions ls --since 22:00"),
                state: ToolState::Ok,
                output: vec![
                    s("mission/fleet/atlas/store-move   running  3/5"),
                    s("mission/fleet/atlas/nightly      done     2/2"),
                    s("mission/fleet/rekey              running  1/3"),
                    s("mission/fleet/release/weekly     running  1/3"),
                    s("mission/fleet/docs/handbook      standing"),
                    s("mission/fleet/site/pricing-page  running  0/2"),
                    s("mission/fleet/harbor/nightly     done     4/4"),
                ],
            },
        ),
        e(
            "c4",
            "08:03",
            Body::Assistant(s(
                "A quiet night, with one thing waiting on you.\n\n## Waiting on you\n\n- **Atlas store move** finished its compare step: 0 mismatches across 1.2M rows. The cut-over needs your review.\n\n## Finished\n\n- The atlas nightly build passed.\n- Harbor's nightly audit found nothing new.\n\n## Moving\n\n- The rekey mission rotated 3 of 5 keys; the rest are ready but unclaimed.\n\nWant me to start the rekey worker on the remaining keys?",
            )),
        ),
        e(
            "c5",
            "08:05",
            Body::User(s("Yes, and make sure the release goes out today.")),
        ),
        e(
            "c6",
            "08:05",
            Body::Mail {
                from: s("Chief of Staff"),
                to: s("Rekey Worker"),
                subject: s("Finish the rotation"),
                body: s(
                    "Robin asked for the remaining two keys today. Claim `rotate` when you are free.",
                ),
            },
        ),
        e(
            "c7",
            "08:06",
            Body::Assistant(s(
                "Done. I asked the rekey worker to pick up `rotate`, and I'll watch the weekly release.",
            )),
        ),
        e(
            "c8",
            "08:41",
            Body::Event(s("Release Captain exited · restart 1 of 4")),
        ),
        e(
            "c9",
            "08:44",
            Body::Mail {
                from: s("st"),
                to: s("Chief of Staff"),
                subject: s("Seat release-captain stopped restarting"),
                body: s(
                    "release-captain on harbor exited 4 times in 10 minutes. Restarts are paused until a person acts.",
                ),
            },
        ),
        e(
            "c10",
            "08:45",
            Body::Tool {
                title: s("read ~/.local/state/st/logs/release-captain.log"),
                state: ToolState::Ok,
                output: vec![
                    s("08:40:12 starting codex"),
                    s("08:40:13 error: 401 Unauthorized (api key expired)"),
                    s("08:40:13 exit 1"),
                    s("08:42:30 starting codex"),
                    s("08:42:31 error: 401 Unauthorized (api key expired)"),
                    s("08:42:31 exit 1"),
                ],
            },
        ),
        e(
            "c11",
            "08:46",
            Body::Assistant(s(
                "The release seat is down: **its API key expired at 08:40**, so every restart fails the same way. I raised a fault for you on Home.\n\nOnce the key is renewed, run:\n\n```sh\nst agents restart release-captain\n```\n\nand the `tag` step will retry by itself.",
            )),
        ),
    ]
}

fn atlas_conversation() -> Vec<Entry> {
    let mut entries = vec![
        e(
            "a1",
            "07:10",
            Body::Event(s("step compare ready · atlas/store-move")),
        ),
        e(
            "a2",
            "07:10",
            Body::Assistant(s(
                "Starting the compare step. I'll count rows on both sides, then run the contract queries against each store.",
            )),
        ),
        e(
            "a3",
            "07:11",
            Body::Tool {
                title: s("$ atlas-cli count --store old --store new"),
                state: ToolState::Ok,
                output: vec![s("old  1,204,881"), s("new  1,204,881")],
            },
        ),
    ];
    for (index, table) in [
        "accounts", "invoices", "events", "sessions", "keys", "audit",
    ]
    .iter()
    .enumerate()
    {
        entries.push(e(
            &format!("a4-{index}"),
            "07:14",
            Body::Tool {
                title: format!("$ atlas-cli diff --table {table}"),
                state: ToolState::Ok,
                output: vec![format!("{table}: 0 mismatches")],
            },
        ));
    }
    entries.push(e(
        "a5",
        "07:39",
        Body::Tool {
            title: s("$ cargo test -p atlas-contract"),
            state: ToolState::Ok,
            output: (1..=42)
                .map(|n| format!("test contract::query_{n:02} ... ok"))
                .chain([s(""), s("test result: ok. 42 passed; 0 failed")])
                .collect(),
        },
    ));
    entries.push(e("a6", "07:40", Body::Assistant(s(
        "Both stores agree.\n\n| check | result |\n|---|---|\n| rows | 1,204,881 on both sides |\n| per-table diff | 0 mismatches in 6 tables |\n| contract | 42 of 42 queries identical |\n\nThe next step is the cut-over, which needs a person. I asked Robin for a review and will wait.",
    ))));
    entries.push(e(
        "a7",
        "07:40",
        Body::Event(s("waiting for review · cut-over")),
    ));
    entries
}

fn docs_conversation() -> Vec<Entry> {
    vec![
        e(
            "d1",
            "09:30",
            Body::Event(s("step draft ready · site/pricing-page")),
        ),
        e(
            "d2",
            "09:31",
            Body::Tool {
                title: s("read ~/src/site--pricing/pages/pricing.md"),
                state: ToolState::Ok,
                output: vec![s("# Pricing"), s(""), s("Contact sales.")],
            },
        ),
        e(
            "d3",
            "09:36",
            Body::Tool {
                title: s("edit pages/pricing.md"),
                state: ToolState::Ok,
                output: vec![
                    s("-# Pricing"),
                    s("+# Simple pricing"),
                    s("-Contact sales."),
                    s("+Start free. Pay when your team grows."),
                ],
            },
        ),
        e(
            "d4",
            "09:40",
            Body::Tool {
                title: s("$ npm run build"),
                state: ToolState::Failed,
                output: vec![
                    s("error: unknown shortcode {{plan-table}} in pages/pricing.md:9"),
                    s("build failed"),
                ],
            },
        ),
        e(
            "d5",
            "09:41",
            Body::Assistant(s(
                "The build doesn't know `{{plan-table}}` yet, so I wrote the plans as a plain list for this draft.",
            )),
        ),
        e(
            "d6",
            "09:44",
            Body::Tool {
                title: s("$ npm run build"),
                state: ToolState::Ok,
                output: vec![s("built 48 pages in 3.1s")],
            },
        ),
        e(
            "d7",
            "09:45",
            Body::Assistant(s(
                "Draft 2 is up at the preview link. I asked Robin for feedback before publishing.",
            )),
        ),
    ]
}

/// What the demo streams into the Chief of Staff conversation, a few words per tick.
pub const STREAM: &str = "Update: the rekey worker claimed `rotate` and finished **key 4 of 5**. The last key is rotating now; I'll tell you when the report is written.";

/// A message that arrives partway through the demo.
pub fn late_mail() -> Entry {
    e(
        "late-1",
        "09:52",
        Body::Mail {
            from: s("Rekey Worker"),
            to: s("Chief of Staff"),
            subject: s("All five keys rotated"),
            body: s(
                "Every signing key is rotated and nothing reads the old ones. The report step is next.",
            ),
        },
    )
}

/// An invented terminal screen for the demo: what opening an agent's terminal looks like.
pub fn terminal(name: &str) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    let dim = Style::default().fg(Color::Rgb(0x6c, 0x70, 0x86));
    let accent = Style::default()
        .fg(Color::Rgb(0xfa, 0xb3, 0x87))
        .add_modifier(Modifier::BOLD);
    vec![
        Line::from(Span::styled(
            format!("╭─ {name} ─────────────────────────────────────────╮"),
            dim,
        )),
        Line::from(vec![
            Span::styled("│ ", dim),
            Span::styled("✻ Working on the cut-over review", accent),
        ]),
        Line::from(Span::styled("│", dim)),
        Line::from(Span::styled(
            "│ ⏺ Read(docs/compare-report.md)",
            Style::default(),
        )),
        Line::from(Span::styled("│   ⎿ 42 lines", dim)),
        Line::from(Span::styled(
            "│ ⏺ Waiting for a person to approve the cut-over.",
            Style::default(),
        )),
        Line::from(Span::styled(
            "╰────────────────────────────────────────────────────╯",
            dim,
        )),
        Line::default(),
        Line::from(vec![Span::styled("> ", accent), Span::raw("█")]),
        Line::default(),
        Line::from(Span::styled(
            "  demo: this screen is invented and keys are not sent",
            dim,
        )),
    ]
}
