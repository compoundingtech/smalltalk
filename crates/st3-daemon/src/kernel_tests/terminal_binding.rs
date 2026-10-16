use crate::terminal_binding::*;
use serde_json::Value;

#[test]
fn durable_binding_claims_require_the_terminal_owners_actor_on_every_admission_path() {
    let intent = crate::parse_intent(
        "version 2\nagent \"example/bound\" { harness \"claude\" {}; bind-terminal \"pty/person/avery/019a0000-0000-7000-8000-000000000001\" incarnation=\"shell:created\" id=\"019a0000-0000-7000-8000-000000000002\"; }",
        "orchid",
    ).unwrap();
    let body = serde_json::to_value(&intent.subjects["agent/example/bound"]).unwrap();
    validate_claim("intent.desired", &body, Some("person/avery")).unwrap();
    for actor in [None, Some("person/intruder"), Some("agent/example/worker")] {
        assert_eq!(
            validate_claim("intent.desired", &body, actor)
                .unwrap_err()
                .code,
            "terminal-owner-forbidden"
        );
    }
    let mut forged = body.clone();
    forged["member"]["terminal_binding"]["subject"] =
        Value::String("pty/person/intruder/019a0000-0000-7000-8000-000000000001".into());
    assert_eq!(
        validate_claim("intent.desired", &forged, Some("person/avery"))
            .unwrap_err()
            .code,
        "invalid-terminal-binding"
    );
}
