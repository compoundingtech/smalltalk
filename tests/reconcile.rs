//! M1 correctness net: the pure reconcile plan over VRS jobs (services).

use std::collections::BTreeMap;
use std::path::PathBuf;

use st2::reconcile::ObservedPtyPresentation;
use st2::reconcile::reconcile_selected;
use st2::reconcile::resolve_task;
use st2::spec::{AgentDesiredState, AgentSpec, JobType, Resource, Task, TaskKind, TaskLifecycle};
use st2::{Session, reconcile as reconcile_result};

fn reconcile<'a>(
    specs: &'a [AgentSpec],
    sessions: &[Session],
    host: &str,
) -> st2::ReconcilePlan<'a> {
    reconcile_result(specs, sessions, host).unwrap()
}

#[test]
fn exact_task_selector_matrix() {
    let specs = vec![
        svc(
            "a",
            None,
            vec![
                task(TaskKind::Pty, "agent", None, Some("run")),
                task(TaskKind::Exec, "ding", Some("host.a.ding"), Some("ding")),
            ],
        ),
        svc(
            "b",
            None,
            vec![task(
                TaskKind::Pty,
                "agent",
                Some("host.a.agent"),
                Some("other"),
            )],
        ),
        svc(
            "remote",
            Some("other"),
            vec![task(TaskKind::Pty, "agent", None, Some("remote"))],
        ),
    ];
    let (_, selected, runtime) = resolve_task(&specs, "host.a.ding", "host").unwrap();
    assert_eq!(selected.name, "ding");
    assert_eq!(runtime, "host.a.ding");
    assert!(resolve_task(&specs, "host.a.agent", "host").is_err()); // explicit-id collision is ambiguous
    assert!(resolve_task(&specs, "a", "host").is_err());
    assert!(resolve_task(&specs, "other.remote.agent", "host").is_err());
    assert!(resolve_task(&specs, "host.a.missing", "host").is_err());
}

#[test]
fn selector_derived_id_success() {
    let s = vec![svc(
        "plain",
        None,
        vec![task(TaskKind::Exec, "work", None, Some("cmd"))],
    )];
    let (owner, t, id) = resolve_task(&s, "host.plain.work", "host").unwrap();
    assert_eq!(
        (
            owner.identity.as_str(),
            t.name.as_str(),
            t.command.as_deref(),
            id.as_str()
        ),
        ("plain", "work", Some("cmd"), "host.plain.work")
    );
}

#[test]
fn selector_explicit_id_and_qualified_alias_return_explicit_runtime() {
    let s = vec![svc(
        "agent",
        None,
        vec![task(TaskKind::Exec, "work", Some("custom"), Some("cmd"))],
    )];
    for selector in ["custom", "host.agent.work"] {
        let (o, t, id) = resolve_task(&s, selector, "host").unwrap();
        assert_eq!(
            (
                o.identity.as_str(),
                t.name.as_str(),
                t.command.as_deref(),
                id.as_str()
            ),
            ("agent", "work", Some("cmd"), "custom")
        );
    }
}

#[test]
fn selector_agent_command_runtime_id_and_inputs_immutable() {
    let s = vec![svc(
        "agent",
        None,
        vec![task(
            TaskKind::Pty,
            "agent",
            Some("host.agent"),
            Some("run"),
        )],
    )];
    let before = s.clone();
    let (o, t, id) = resolve_task(&s, "host.agent", "host").unwrap();
    assert_eq!(
        (
            o.identity.as_str(),
            t.name.as_str(),
            t.command.as_deref(),
            id.as_str()
        ),
        ("agent", "agent", Some("run"), "host.agent")
    );
    assert_eq!(s, before);
    assert!(resolve_task(&s, "host.missing", "host").is_err());
    assert_eq!(s, before);
}

#[test]
fn selected_reconcile_launches_missing_and_adopts_live_without_siblings() {
    let specs = vec![
        svc(
            "a",
            None,
            vec![
                task(TaskKind::Exec, "x", None, Some("a")),
                task(TaskKind::Exec, "y", None, Some("b")),
            ],
        ),
        svc("b", None, vec![task(TaskKind::Exec, "z", None, Some("c"))]),
    ];
    let plan = reconcile_selected(&specs, &[], "host", "host.a.x").unwrap();
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.launch[0].tasks.len(), 1);
    assert_eq!(plan.launch[0].tasks[0].pty_id, "host.a.x");
    let plan2 = reconcile_selected(
        &specs,
        &[live("host.a.x"), live("host.a.y"), live("host.b.z")],
        "host",
        "host.a.x",
    )
    .unwrap();
    assert!(plan2.launch.is_empty() && plan2.gc.is_empty() && plan2.teardown.is_empty());
    assert_eq!(
        plan2
            .adopt
            .iter()
            .map(|s| s.identity.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
}

#[test]
fn selected_reconcile_freezes_suspended_dead_keep_but_collects_retired_task_keep() {
    let keep = svc(
        "a",
        None,
        vec![{
            let mut t = task(TaskKind::Exec, "x", None, Some("a"));
            t.keep = true;
            t
        }],
    );
    let specs = [keep];
    let p = reconcile_selected(
        &specs,
        &[Session {
            pty_id: "host.a.x".into(),
            alive: false,
            exit_code: Some(7),
            presentation: None,
        }],
        "host",
        "host.a.x",
    )
    .unwrap();
    assert!(p.launch.is_empty() && p.gc.is_empty() && p.adopt.len() == 1);
    let mut retired = svc(
        "b",
        None,
        vec![{
            let mut t = task(TaskKind::Exec, "x", None, Some("b"));
            t.keep = true;
            t
        }],
    );
    retired.desired_state = AgentDesiredState::Retired { reason: None };
    let specs = [retired];
    let p = reconcile_selected(
        &specs,
        &[Session {
            pty_id: "host.b.x".into(),
            alive: false,
            exit_code: Some(7),
            presentation: None,
        }],
        "host",
        "host.b.x",
    )
    .unwrap();
    assert!(p.teardown.is_empty());
    assert_eq!(p.gc, ["host.b.x"]);
}

#[test]
fn selected_reconcile_holds_adopt_only_dead_or_absent_without_mutation() {
    let mut t = task(TaskKind::Pty, "agent", None, Some("codex"));
    t.lifecycle = TaskLifecycle::AdoptOnly;
    let specs = [svc("a", None, vec![t])];

    for sessions in [
        vec![Session {
            pty_id: "host.a.agent".into(),
            alive: false,
            exit_code: Some(1),
            presentation: None,
        }],
        vec![],
    ] {
        let plan = reconcile_selected(&specs, &sessions, "host", "host.a.agent").unwrap();
        assert!(plan.launch.is_empty());
        assert!(plan.gc.is_empty());
        assert!(plan.adopt.is_empty());
        assert_eq!(plan.held, ["host.a.agent"]);
    }
}

#[test]
fn selected_reconcile_action_ids_are_exact_and_refusals_immutable() {
    let specs = vec![
        svc(
            "a",
            None,
            vec![
                task(TaskKind::Exec, "x", None, Some("a")),
                task(TaskKind::Exec, "y", None, Some("b")),
            ],
        ),
        svc("b", None, vec![task(TaskKind::Exec, "z", None, Some("c"))]),
    ];
    let sessions = vec![live("host.a.y"), live("host.b.z")];
    let before = (specs.clone(), sessions.clone());
    let p = reconcile_selected(&specs, &sessions, "host", "host.a.x").unwrap();
    assert_eq!(
        p.launch
            .iter()
            .flat_map(|l| l.tasks.iter().map(|t| t.pty_id.as_str()))
            .collect::<Vec<_>>(),
        vec!["host.a.x"]
    );
    assert!(p.gc.is_empty() && p.teardown.is_empty());
    assert!(reconcile_selected(&specs, &sessions, "host", "host.a.missing").is_err());
    assert_eq!((specs, sessions), before);
}

#[test]
fn selected_dead_non_keep_gc_and_relaunch_only_selected() {
    let specs = vec![
        svc(
            "a",
            None,
            vec![
                task(TaskKind::Exec, "x", None, Some("a")),
                task(TaskKind::Exec, "y", None, Some("b")),
            ],
        ),
        svc("b", None, vec![task(TaskKind::Exec, "z", None, Some("c"))]),
    ];
    let p = reconcile_selected(
        &specs,
        &[
            Session {
                pty_id: "host.a.x".into(),
                alive: false,
                exit_code: Some(1),
                presentation: None,
            },
            Session {
                pty_id: "host.a.y".into(),
                alive: false,
                exit_code: Some(1),
                presentation: None,
            },
            Session {
                pty_id: "host.b.z".into(),
                alive: false,
                exit_code: Some(1),
                presentation: None,
            },
        ],
        "host",
        "host.a.x",
    )
    .unwrap();
    assert_eq!(p.gc, vec!["host.a.x"]);
    assert_eq!(
        p.launch
            .iter()
            .flat_map(|l| l.tasks.iter().map(|t| t.pty_id.as_str()))
            .collect::<Vec<_>>(),
        vec!["host.a.x"]
    );
    assert!(p.teardown.is_empty());
}
#[test]
fn selected_retired_live_tears_down_only_selected() {
    let mut s = svc(
        "a",
        None,
        vec![
            task(TaskKind::Exec, "x", None, Some("a")),
            task(TaskKind::Exec, "sib", None, Some("b")),
        ],
    );
    s.desired_state = AgentDesiredState::Retired { reason: None };
    let specs = [
        s,
        svc("b", None, vec![task(TaskKind::Exec, "z", None, Some("c"))]),
    ];
    let p = reconcile_selected(
        &specs,
        &[live("host.a.x"), live("host.a.sib"), live("host.b.z")],
        "host",
        "host.a.x",
    )
    .unwrap();
    assert_eq!(
        p.teardown
            .iter()
            .flat_map(|t| t.pty_ids.iter().map(String::as_str))
            .collect::<Vec<_>>(),
        vec!["host.a.x"]
    );
    assert!(p.launch.is_empty() && p.gc.is_empty());
}
#[test]
fn selected_refusals_ambiguous_and_unknown_preserve_inputs() {
    let specs = vec![
        svc(
            "a",
            None,
            vec![task(TaskKind::Exec, "x", Some("dup"), Some("a"))],
        ),
        svc(
            "b",
            None,
            vec![task(TaskKind::Exec, "y", Some("dup"), Some("b"))],
        ),
    ];
    let sessions = vec![live("dup")];
    let before = (specs.clone(), sessions.clone());
    assert!(reconcile_selected(&specs, &sessions, "host", "dup").is_err());
    assert!(reconcile_selected(&specs, &sessions, "host", "none").is_err());
    assert_eq!((specs, sessions), before);
}
#[test]
fn selected_unrunnable_is_runner_action_free() {
    let specs = vec![svc("a", None, vec![task(TaskKind::Exec, "x", None, None)])];
    let before = specs.clone();
    let p = reconcile_selected(&specs, &[], "host", "host.a.x").unwrap();
    assert!(p.launch.is_empty() && p.gc.is_empty() && p.teardown.is_empty());
    assert_eq!(specs, before);
}

#[test]
fn selector_rejects_duplicate_explicit_and_runtime_collision() {
    let dup = vec![
        svc(
            "a",
            None,
            vec![task(TaskKind::Exec, "x", Some("dup"), Some("a"))],
        ),
        svc(
            "b",
            None,
            vec![task(TaskKind::Exec, "y", Some("dup"), Some("b"))],
        ),
    ];
    assert!(resolve_task(&dup, "dup", "host").is_err());
    let collision = vec![
        svc("a", None, vec![task(TaskKind::Exec, "x", None, Some("a"))]),
        svc(
            "b",
            None,
            vec![task(TaskKind::Exec, "y", Some("host.a.x"), Some("b"))],
        ),
    ];
    assert!(resolve_task(&collision, "host.a.x", "host").is_err());
}

fn task(kind: TaskKind, name: &str, id: Option<&str>, command: Option<&str>) -> Task {
    Task {
        kind,
        derived: false,
        name: name.to_string(),
        id: id.map(String::from),
        command: command.map(String::from),
        argv: None,
        cwd: None,
        tags: BTreeMap::new(),
        env: BTreeMap::new(),
        keep: false,
        lifecycle: TaskLifecycle::Service,
    }
}

fn spec(
    identity: &str,
    host: Option<&str>,
    job_type: JobType,
    retired: bool,
    tasks: Vec<Task>,
) -> AgentSpec {
    AgentSpec {
        id: None,
        address: None,
        identity: identity.to_string(),
        name: None,
        description: None,
        host: host.map(String::from),
        role: None,
        job_type,
        workspace: None,
        supervisor: None,
        desired_state: if retired {
            AgentDesiredState::Retired { reason: None }
        } else {
            AgentDesiredState::Running
        },
        keep: false,
        restart: None,
        delivery: None,
        session_driver: None,
        driver: None,
        delivery_readiness: None,
        resources: Vec::new(),
        streams: Vec::new(),
        tasks,
        path: PathBuf::from(format!(
            "/cat/agents/{}/{identity}/agent.kdl",
            host.unwrap_or("this")
        )),
    }
}

fn svc(identity: &str, host: Option<&str>, tasks: Vec<Task>) -> AgentSpec {
    spec(identity, host, JobType::Service, false, tasks)
}

fn suspended(identity: &str, tasks: Vec<Task>) -> AgentSpec {
    let mut spec = svc(identity, None, tasks);
    spec.desired_state = AgentDesiredState::Suspended {
        reason: "Temporarily idle".into(),
    };
    spec
}

#[test]
fn suspended_agents_teardown_live_tasks_reap_dead_nonkeep_and_never_launch() {
    let mut spec = suspended(
        "idle",
        vec![
            task(TaskKind::Pty, "agent", Some("host.idle.agent"), Some("run")),
            task(TaskKind::Exec, "ding", Some("host.idle.ding"), Some("ding")),
            task(
                TaskKind::Exec,
                "stale",
                Some("host.idle.stale"),
                Some("stale"),
            ),
            {
                let mut retained = task(
                    TaskKind::Exec,
                    "retained",
                    Some("host.idle.retained"),
                    Some("retained"),
                );
                retained.keep = true;
                retained
            },
        ],
    );
    spec.tasks[0]
        .env
        .insert("ST_AGENT".into(), "wrong.actor".into());
    let specs = vec![spec];
    let plan = reconcile(
        &specs,
        &[
            live("host.idle.agent"),
            live("host.idle.ding"),
            dead("host.idle.stale"),
            dead("host.idle.retained"),
        ],
        "host",
    );

    assert!(plan.launch.is_empty());
    assert!(plan.adopt.is_empty());
    assert_eq!(plan.gc, ["host.idle.stale"]);
    assert_eq!(plan.teardown.len(), 1);
    assert_eq!(
        plan.teardown[0].pty_ids,
        vec!["host.idle.agent", "host.idle.ding"]
    );

    let mut running = specs[0].clone();
    running.desired_state = AgentDesiredState::Running;
    let error = reconcile_result(&[running], &[], "host").unwrap_err();
    assert!(error.to_string().contains("conflicting ST_AGENT"));
}

#[test]
fn resuming_uses_ordinary_reconcile_and_does_not_override_keep() {
    let mut spec = suspended(
        "idle",
        vec![task(
            TaskKind::Pty,
            "agent",
            Some("host.idle.agent"),
            Some("run"),
        )],
    );
    spec.keep = true;
    spec.desired_state = AgentDesiredState::Running;

    let specs = [spec];
    let plan = reconcile(&specs, &[dead("host.idle.agent")], "host");
    assert!(plan.launch.is_empty());
    assert_eq!(
        plan.adopt.len(),
        1,
        "resume preserves the existing keep contract"
    );
}

fn live(id: &str) -> Session {
    Session {
        pty_id: id.to_string(),
        alive: true,
        exit_code: None,
        presentation: None,
    }
}
fn dead(id: &str) -> Session {
    Session {
        pty_id: id.to_string(),
        alive: false,
        exit_code: None,
        presentation: None,
    }
}

const HOST: &str = "hetz";

#[test]
fn fresh_service_launches_all_tasks_pty_and_exec() {
    let specs = vec![svc(
        "st2-claude",
        Some(HOST),
        vec![
            task(
                TaskKind::Pty,
                "agent",
                Some("hetz.st2-claude"),
                Some("exec claude 'boot'"),
            ),
            task(
                TaskKind::Exec,
                "ding",
                Some("hetz.st2.ding"),
                Some("st2 ding hetz.st2"),
            ),
        ],
    )];
    let plan = reconcile(&specs, &[], HOST);
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.launch[0].tasks.len(), 2);
    let kinds: Vec<TaskKind> = plan.launch[0].tasks.iter().map(|t| t.kind).collect();
    assert!(kinds.contains(&TaskKind::Pty));
    assert!(kinds.contains(&TaskKind::Exec));
}

#[test]
fn all_tasks_live_is_adopted() {
    let specs = vec![svc(
        "a",
        Some(HOST),
        vec![
            task(TaskKind::Pty, "agent", Some("hetz.a"), Some("x")),
            task(TaskKind::Exec, "ding", Some("hetz.a.ding"), Some("y")),
        ],
    )];
    let sessions = vec![live("hetz.a"), live("hetz.a.ding")];
    let plan = reconcile(&specs, &sessions, HOST);
    assert!(plan.launch.is_empty());
    assert_eq!(plan.adopt.len(), 1);
}

#[test]
fn live_pty_presentation_is_exact_id_metadata_and_not_lifecycle_drift() {
    let mut owner = svc(
        "worker",
        Some(HOST),
        vec![
            task(TaskKind::Pty, "agent", Some("hetz.worker"), Some("codex")),
            task(
                TaskKind::Pty,
                "shell",
                Some("hetz.worker.shell"),
                Some("sh"),
            ),
        ],
    );
    owner.name = Some("Build owner".to_owned());
    owner.description = Some("Owns build delivery".to_owned());
    let specs = [owner];
    let plan = reconcile(
        &specs,
        &[live("hetz.worker"), live("hetz.worker.shell")],
        HOST,
    );

    assert!(plan.launch.is_empty());
    assert!(plan.teardown.is_empty());
    assert!(plan.gc.is_empty());
    assert_eq!(plan.presentation.len(), 2);
    let primary = plan
        .presentation
        .iter()
        .find(|item| item.pty_id == "hetz.worker")
        .unwrap();
    assert_eq!(primary.display_name, Some(Some("Build owner".to_owned())));
    assert_eq!(
        primary.tags,
        BTreeMap::from([
            ("agent.presentation.schema".to_owned(), Some("1".to_owned())),
            (
                "agent.actor.path".to_owned(),
                Some("hetz.worker".to_owned())
            ),
            (
                "agent.presentation.description".to_owned(),
                Some("Owns build delivery".to_owned()),
            ),
            ("role".to_owned(), Some("agent".to_owned())),
        ])
    );
    let secondary = plan
        .presentation
        .iter()
        .find(|item| item.pty_id == "hetz.worker.shell")
        .unwrap();
    assert_eq!(secondary.display_name, None);
    assert_eq!(
        secondary.tags,
        BTreeMap::from([
            ("agent.presentation.schema".to_owned(), Some("1".to_owned())),
            (
                "agent.actor.path".to_owned(),
                Some("hetz.worker".to_owned()),
            ),
            (
                "agent.presentation.description".to_owned(),
                Some("Owns build delivery".to_owned()),
            ),
            ("role".to_owned(), None),
        ])
    );
}

#[test]
fn lifecycle_equal_primary_name_is_cleared_during_live_reconciliation() {
    let mut owner = svc(
        "worker",
        Some(HOST),
        vec![task(
            TaskKind::Pty,
            "agent",
            Some("hetz.worker"),
            Some("codex"),
        )],
    );
    owner.name = Some("hetz.worker".to_owned());

    let specs = [owner];
    let plan = reconcile(&specs, &[live("hetz.worker")], HOST);
    assert_eq!(plan.presentation.len(), 1);
    assert_eq!(plan.presentation[0].display_name, Some(None));
}

#[test]
fn live_pty_presentation_only_queues_observed_drift() {
    let mut owner = svc(
        "worker",
        Some(HOST),
        vec![task(
            TaskKind::Pty,
            "agent",
            Some("hetz.worker"),
            Some("codex"),
        )],
    );
    owner.name = Some("Build owner".to_owned());
    owner.description = Some("Owns build delivery".to_owned());
    let specs = [owner];
    let exact_tags = BTreeMap::from([
        ("agent.presentation.schema".to_owned(), "1".to_owned()),
        ("agent.actor.path".to_owned(), "hetz.worker".to_owned()),
        (
            "agent.presentation.description".to_owned(),
            "Owns build delivery".to_owned(),
        ),
        ("role".to_owned(), "agent".to_owned()),
        ("run.role".to_owned(), "coding-agent".to_owned()),
        ("unrelated".to_owned(), "preserved".to_owned()),
    ]);
    let exact = Session {
        pty_id: "hetz.worker".to_owned(),
        alive: true,
        exit_code: None,
        presentation: Some(ObservedPtyPresentation {
            display_name: Some("Build owner".to_owned()),
            tags: exact_tags.clone(),
        }),
    };
    assert!(reconcile(&specs, &[exact], HOST).presentation.is_empty());

    let drifted = Session {
        pty_id: "hetz.worker".to_owned(),
        alive: true,
        exit_code: None,
        presentation: Some(ObservedPtyPresentation {
            display_name: Some("Old owner".to_owned()),
            tags: exact_tags,
        }),
    };
    assert_eq!(reconcile(&specs, &[drifted], HOST).presentation.len(), 1);
    assert_eq!(
        reconcile(&specs, &[live("hetz.worker")], HOST)
            .presentation
            .len(),
        1
    );
}

#[test]
fn resource_only_changes_do_not_replace_or_relaunch_a_live_task() {
    let mut spec = svc(
        "a",
        Some(HOST),
        vec![task(TaskKind::Pty, "agent", Some("hetz.a"), Some("x"))],
    );
    spec.resources.push(
        Resource::new(
            "work".into(),
            "github-issue://example/project/41".into(),
            "Current implementation task.".into(),
        )
        .unwrap(),
    );

    let specs = [spec];
    let plan = reconcile(&specs, &[live("hetz.a")], HOST);
    assert_eq!(plan.adopt.len(), 1);
    assert!(plan.launch.is_empty());
    assert!(plan.teardown.is_empty());
    assert!(plan.gc.is_empty());
}

#[test]
fn one_dead_task_launches_only_the_missing_one() {
    let specs = vec![svc(
        "a",
        Some(HOST),
        vec![
            task(TaskKind::Pty, "agent", Some("hetz.a"), Some("x")),
            task(TaskKind::Exec, "ding", Some("hetz.a.ding"), Some("y")),
        ],
    )];
    let plan = reconcile(&specs, &[live("hetz.a")], HOST);
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.launch[0].tasks.len(), 1);
    assert_eq!(plan.launch[0].tasks[0].pty_id, "hetz.a.ding");
}

#[test]
fn exited_session_is_reaped_and_relaunched() {
    let specs = vec![svc(
        "a",
        Some(HOST),
        vec![task(TaskKind::Pty, "agent", Some("hetz.a"), Some("x"))],
    )];
    let plan = reconcile(&specs, &[dead("hetz.a")], HOST);
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.gc, vec!["hetz.a"]); // reap the corpse, then respawn
}

#[test]
fn adopt_only_task_holds_dead_or_absent_generation_without_replacement() {
    let mut t = task(TaskKind::Pty, "agent", Some("hetz.a"), Some("x"));
    t.lifecycle = TaskLifecycle::AdoptOnly;
    let specs = vec![svc("a", Some(HOST), vec![t])];

    for sessions in [vec![dead("hetz.a")], vec![]] {
        let plan = reconcile(&specs, &sessions, HOST);
        assert!(plan.launch.is_empty());
        assert!(plan.gc.is_empty());
        assert_eq!(plan.held, vec!["hetz.a"]);
    }
}

#[test]
fn leaving_adopt_only_explicitly_restores_replacement_lifecycle() {
    let specs = vec![svc(
        "a",
        Some(HOST),
        vec![task(TaskKind::Pty, "agent", Some("hetz.a"), Some("x"))],
    )];
    let plan = reconcile(&specs, &[dead("hetz.a")], HOST);
    assert_eq!(plan.gc, vec!["hetz.a"]);
    assert_eq!(plan.launch[0].tasks[0].pty_id, "hetz.a");
    assert!(plan.held.is_empty());
}

#[test]
fn dead_keep_task_is_frozen_not_reaped() {
    let mut t = task(TaskKind::Pty, "agent", Some("hetz.a"), Some("x"));
    t.keep = true;
    let specs = vec![svc("a", Some(HOST), vec![t])];
    let plan = reconcile(&specs, &[dead("hetz.a")], HOST);
    assert!(plan.launch.is_empty());
    assert!(plan.gc.is_empty());
    assert_eq!(plan.adopt.len(), 1); // present (frozen)
}

#[test]
fn retired_with_live_sessions_is_torn_down() {
    let specs = vec![spec(
        "old",
        Some(HOST),
        JobType::Service,
        true,
        vec![task(TaskKind::Pty, "agent", Some("hetz.old"), Some("x"))],
    )];
    let plan = reconcile(&specs, &[live("hetz.old")], HOST);
    assert_eq!(plan.teardown.len(), 1);
    assert_eq!(plan.teardown[0].pty_ids, vec!["hetz.old"]);
}

/// dotfiles#1535: retiring an agent that still declares `resource` bindings (including a `work://`
/// URI) must admit and reconcile to zero live tasks without error — the Resource references are
/// declaration metadata, not a reason to refuse admission — and flipping back to running must
/// relaunch the seat. Task-identity admission never consults resources; this pins that contract.
#[test]
fn retired_agent_keeps_resources_and_still_reconciles_to_zero_live_tasks() {
    let with_resources = || {
        let mut spec = spec(
            "worker",
            Some(HOST),
            JobType::Service,
            true,
            vec![task(TaskKind::Pty, "agent", Some("hetz.worker"), Some("x"))],
        );
        spec.resources.push(
            Resource::new(
                "work".into(),
                "work://hetz/current-task".into(),
                "Current implementation task.".into(),
            )
            .unwrap(),
        );
        spec.resources.push(
            Resource::new(
                "issue".into(),
                "github-issue://example/project/41".into(),
                "Tracking issue.".into(),
            )
            .unwrap(),
        );
        spec
    };

    // Retired with a live seat: admits (no TaskIdentityAdmissionError), tears down the seat,
    // settles retirement, launches nothing. The declared resources do not block any of this.
    let retired = [with_resources()];
    let plan = reconcile_result(&retired, &[live("hetz.worker")], HOST)
        .expect("retired specs with resources are admitted");
    assert_eq!(plan.teardown.len(), 1);
    assert_eq!(plan.teardown[0].pty_ids, vec!["hetz.worker"]);
    assert_eq!(plan.settle_retirement.len(), 1);
    assert!(plan.launch.is_empty());

    // Once the seat is gone, retirement is complete: nothing to tear down, gc, or launch.
    let plan = reconcile_result(&retired, &[], HOST).expect("retired specs with resources admit");
    assert!(plan.teardown.is_empty());
    assert!(plan.launch.is_empty());
    assert!(plan.gc.is_empty());

    // Un-retire (desired-state running): the identical spec, now running, relaunches its seat with
    // its resources still intact.
    let mut unretired = with_resources();
    unretired.desired_state = AgentDesiredState::Running;
    assert_eq!(unretired.resources.len(), 2);
    let specs = [unretired];
    let plan = reconcile_result(&specs, &[], HOST).expect("running specs with resources admit");
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.launch[0].tasks[0].pty_id, "hetz.worker");
    assert!(plan.settle_retirement.is_empty());
}

#[test]
fn other_host_specs_are_skipped() {
    let specs = vec![
        svc(
            "here",
            Some(HOST),
            vec![task(TaskKind::Pty, "agent", Some("hetz.here"), Some("x"))],
        ),
        svc(
            "there",
            Some("silber"),
            vec![task(
                TaskKind::Pty,
                "agent",
                Some("silber.there"),
                Some("y"),
            )],
        ),
    ];
    let plan = reconcile(&specs, &[], HOST);
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.launch[0].spec.identity, "here");
    assert_eq!(plan.other_host.len(), 1);
    assert_eq!(plan.other_host[0].identity, "there");
}

#[test]
fn host_none_defaults_to_this_host_with_fallback_id() {
    let specs = vec![svc(
        "local",
        None,
        vec![task(TaskKind::Pty, "agent", None, Some("x"))],
    )];
    let plan = reconcile(&specs, &[], HOST);
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.launch[0].tasks[0].pty_id, "hetz.local.agent"); // <bus_id>.<name>
}

#[test]
fn unrendered_job_without_commands_is_unrunnable() {
    let specs = vec![svc(
        "nr",
        Some(HOST),
        vec![task(TaskKind::Pty, "agent", None, None)],
    )];
    let plan = reconcile(&specs, &[], HOST);
    assert!(plan.launch.is_empty());
    assert_eq!(plan.unrunnable.len(), 1);
}

#[test]
fn generated_ding_only_job_is_unrunnable_and_does_not_launch() {
    let mut ding = task(
        TaskKind::Exec,
        "ding",
        Some("hetz.nr.ding"),
        Some("st2 ding --identity hetz.nr --root $ST_ROOT"),
    );
    ding.derived = true;
    let specs = vec![svc("nr", Some(HOST), vec![ding])];
    let plan = reconcile(&specs, &[], HOST);
    assert!(plan.launch.is_empty());
    assert_eq!(plan.unrunnable.len(), 1);
}

#[test]
fn generated_ding_launches_alongside_authored_work() {
    let agent = task(TaskKind::Pty, "agent", Some("hetz.runnable"), Some("codex"));
    let mut ding = task(
        TaskKind::Exec,
        "ding",
        Some("hetz.runnable.ding"),
        Some("st2 ding --identity hetz.runnable --root $ST_ROOT"),
    );
    ding.derived = true;
    let specs = vec![svc("runnable", Some(HOST), vec![agent, ding])];
    let plan = reconcile(&specs, &[], HOST);
    assert_eq!(plan.launch.len(), 1);
    assert_eq!(plan.launch[0].tasks.len(), 2);
}

#[test]
fn agent_level_keep_pins_all_task_targets() {
    let mut s = svc(
        "kept",
        Some(HOST),
        vec![
            task(TaskKind::Pty, "agent", Some("hetz.kept"), Some("x")),
            task(TaskKind::Exec, "ding", Some("hetz.kept.ding"), Some("y")),
        ],
    );
    s.keep = true;
    let plan = reconcile(std::slice::from_ref(&s), &[], HOST);
    assert!(plan.launch[0].tasks.iter().all(|t| t.keep));
}

#[test]
fn workspace_is_carried_into_task_targets_for_cwd_defaulting() {
    let mut s = svc(
        "w",
        Some(HOST),
        vec![task(TaskKind::Pty, "agent", Some("hetz.w"), Some("x"))],
    );
    s.workspace = Some("/repos/w".into());
    let plan = reconcile(std::slice::from_ref(&s), &[], HOST);
    assert_eq!(
        plan.launch[0].tasks[0].workspace.as_deref(),
        Some("/repos/w")
    );
}

#[test]
fn declared_supervisor_is_the_single_source_for_the_spawn_environment() {
    let mut t = task(TaskKind::Pty, "agent", Some("hetz.w"), Some("x"));
    // A stale hand-authored value must not be able to disagree with the normative spec field.
    t.env
        .insert("ST_SUPERVISOR".into(), "stale-env-value".into());
    let mut s = svc("w", Some(HOST), vec![t]);
    s.supervisor = Some("lead".into());

    let plan = reconcile(std::slice::from_ref(&s), &[], HOST);
    assert_eq!(
        plan.launch[0].tasks[0]
            .env
            .get("ST_SUPERVISOR")
            .map(String::as_str),
        Some("lead")
    );

    // Removing the declaration removes the derived variable, even if an old renderer still emits
    // one. Absence must not preserve a phantom supervision relationship.
    s.supervisor = None;
    let plan = reconcile(std::slice::from_ref(&s), &[], HOST);
    assert!(!plan.launch[0].tasks[0].env.contains_key("ST_SUPERVISOR"));
}

/// `plan.live` is the restart cap's only positive evidence, and the adopt path is where an
/// already-running task supplies it. Dropping the push there is invisible to every lifecycle
/// assertion — the task is still adopted, still not launched, still not reaped — while the cap
/// silently stops being told the task is up and starts forgiving its failure budget on silence.
#[test]
fn adopting_a_live_task_proves_it_alive_to_the_restart_cap() {
    let specs = vec![svc(
        "worker",
        None,
        vec![task(TaskKind::Pty, "agent", None, Some("run"))],
    )];
    // Take the runtime id from the launch plan itself rather than restating the derivation.
    let runtime = reconcile(&specs, &[], HOST).launch[0].tasks[0]
        .pty_id
        .clone();

    let plan = reconcile(&specs, &[live(&runtime)], HOST);

    assert_eq!(
        plan.live,
        vec![runtime.clone()],
        "an adopted live task must be reported as proved alive"
    );
    // The lifecycle side is unchanged, which is exactly why `live` needs its own assertion.
    assert_eq!(plan.adopt.len(), 1);
    assert!(plan.launch.is_empty());
    assert!(plan.gc.is_empty());
}

/// The same evidence, on the task-scoped path. `reconcile_selected` has its own adopt arm and its
/// own `plan.live` push; covering the whole-catalog path says nothing about this one, and a `st2`
/// invocation scoped to a single task still feeds the restart cap.
#[test]
fn selecting_a_live_task_proves_it_alive_to_the_restart_cap() {
    let specs = vec![svc(
        "worker",
        None,
        vec![task(TaskKind::Pty, "agent", None, Some("run"))],
    )];
    let runtime = reconcile(&specs, &[], HOST).launch[0].tasks[0]
        .pty_id
        .clone();

    let plan = reconcile_selected(&specs, &[live(&runtime)], HOST, &runtime).unwrap();

    assert_eq!(
        plan.live,
        vec![runtime.clone()],
        "a selected live task must be reported as proved alive"
    );
    assert_eq!(plan.adopt.len(), 1);
    assert!(plan.launch.is_empty());
}
