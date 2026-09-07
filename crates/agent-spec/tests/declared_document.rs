use std::path::Path;

use agent_spec::{DeclaredSeverity, DeclaredValue, parse_declared_document};

const MANAGED: &str = r#"agent "worker" {
  host "example"
  supervisor "root"
  supervisor "replacement"
  env {
    AGENT_PERSONA "worker"
    ST_AGENT "example.worker"
  }
  render {
    copy "_templates/bus.st2.md" ".st2/bus.md"
  }
  pty "agent" { argv "/opt/publisher/bin/publisher" "agent" "launch" }
  meta { managed-by "agent-spec-authoring"; authored-by "example.root" }
}
"#;

#[test]
fn preserves_the_complete_typed_kdl_tree_and_duplicate_occurrences() {
    let parsed = parse_declared_document(Path::new("agents/example/worker/agent.kdl"), MANAGED);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let document = parsed.document.expect("document");
    assert_eq!(document.source, MANAGED);
    assert_eq!(document.agents.len(), 1);

    let agent = &document.agents[0];
    assert_eq!(
        agent.identity().and_then(DeclaredValue::as_str),
        Some("worker")
    );
    assert_eq!(agent.fields_named("supervisor").count(), 2);
    assert_eq!(
        agent
            .field("env")
            .and_then(|env| env.child("AGENT_PERSONA"))
            .and_then(|entry| entry.argument(0))
            .and_then(DeclaredValue::as_str),
        Some("worker")
    );
    assert_eq!(
        agent
            .field("pty")
            .and_then(|task| task.child("argv"))
            .expect("argv")
            .arguments()
            .filter_map(DeclaredValue::as_str)
            .collect::<Vec<_>>(),
        ["/opt/publisher/bin/publisher", "agent", "launch"]
    );
    assert_eq!(
        &MANAGED[agent.span.offset..agent.span.offset + agent.span.length],
        agent.source()
    );
}

#[test]
fn syntax_failures_are_stable_source_located_diagnostics() {
    let source = "agent \"worker\" {\n  host \"example\n}\n";
    let parsed = parse_declared_document(Path::new("candidate.kdl"), source);
    assert!(parsed.document.is_none());
    assert!(!parsed.diagnostics.is_empty());
    assert!(parsed.diagnostics.iter().all(|diagnostic| {
        diagnostic.code.as_str() == "kdl-syntax"
            && diagnostic.severity == DeclaredSeverity::Error
            && diagnostic.source == Path::new("candidate.kdl")
            && diagnostic.span.line >= 2
            && diagnostic.span.column >= 1
    }));
}

#[test]
fn unnamed_tasks_and_reserved_schedules_are_causal_red_shape_errors() {
    let source = r#"agent "worker" {
  pty { argv "run" }
  schedule "daily" { command "true" }
}
"#;
    let parsed = parse_declared_document(Path::new("candidate.kdl"), source);
    assert!(parsed.document.is_some());
    assert_eq!(
        parsed
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code.as_str())
            .collect::<Vec<_>>(),
        ["task-name-missing", "unsupported-schedule"]
    );
    assert_eq!(
        serde_json::to_value(&parsed.diagnostics).unwrap(),
        serde_json::json!([
            {
                "severity": "error",
                "code": "task-name-missing",
                "source": "candidate.kdl",
                "span": { "offset": 19, "length": 18, "line": 2, "column": 3 },
                "message": "pty task must have one positional string name"
            },
            {
                "severity": "error",
                "code": "unsupported-schedule",
                "source": "candidate.kdl",
                "span": { "offset": 40, "length": 35, "line": 3, "column": 3 },
                "message": "scheduled work is reserved for a future contract and is not implemented"
            }
        ])
    );
}

#[test]
fn duplicate_routing_fields_are_shape_errors() {
    let source = r#"agent "worker" {
  id "first"
  id "second"
  address "first"
  address "second"
  identity "first"
  identity "second"
  host "first"
  host "second"
}"#;
    let parsed = parse_declared_document(Path::new("candidate.kdl"), source);
    assert!(parsed.document.is_some());
    assert_eq!(
        parsed
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code.as_str())
            .collect::<Vec<_>>(),
        [
            "duplicate-routing-field",
            "duplicate-routing-field",
            "duplicate-routing-field",
            "duplicate-routing-field"
        ]
    );
    assert!(!parsed.is_valid());
}

#[test]
fn parsing_is_deterministic_across_a_small_mutation_corpus() {
    let mutations = [
        MANAGED.to_owned(),
        MANAGED.replace("host \"example\"", "host \"other\""),
        MANAGED.replace("supervisor \"root\"", "supervisor 42"),
        MANAGED.replace("argv \"/nix", "argv #true \"/nix"),
        format!("note \"ignored top level\"\n{MANAGED}"),
    ];
    for source in mutations {
        let first = parse_declared_document(Path::new("candidate.kdl"), &source);
        let second = parse_declared_document(Path::new("candidate.kdl"), &source);
        assert_eq!(first, second, "non-deterministic parse for {source}");
    }
}
