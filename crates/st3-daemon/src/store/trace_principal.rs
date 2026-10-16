//! Evidence of declaration through a verified chain; never an authorization input.
//! Trace attribution follows declaration authors only.
use super::*;

impl Store {
    /// The accountable person declared once by a fleet root. This does not answer whose
    /// accounts, prompts, asks or directive notes a seat uses. Missing roots and cycles
    /// resolve to none; account owners and mission requesters are never inferred here.
    /// Every declaration must have a Verified verdict; the root must additionally be signed
    /// by its named person. Walks stop after 64 declarations and fail closed at that bound.
    pub fn trace_principal(&self, agent: &str) -> Result<Option<String>> {
        self.read_snapshot(|_| {
            let mut actor = agent.to_owned();
            let mut seen = BTreeSet::new();
            const MAX_HOPS: usize = 64;
            for _ in 0..MAX_HOPS {
                if !actor.starts_with("agent/") || !seen.insert(actor.clone()) {
                    return Ok(None);
                }
                let Some((desired, author)) = self.desired_subject_with_writer(&actor)? else {
                    return Ok(None);
                };
                if desired.kind != "agent" {
                    return Ok(None);
                }
                let Some(claim) = self.selected_desired_token(&actor)? else {
                    return Ok(None);
                };
                smallclaims::touched::note_read(|| claim.clone());
                if self.claim_verdict(&claim)? != smallclaims::principal::Verdict::Verified {
                    return Ok(None);
                }
                if let Some(person) = &desired.accountable {
                    // Admission enforces this; also verify the selected root at the read cut.
                    st3_schema::accountable::validate_claim(
                        &actor,
                        "intent.desired",
                        &serde_json::to_value(&desired)?,
                        author.as_deref(),
                    )?;
                    let Some(signature) = self.claim_signature(&claim)? else {
                        return Ok(None);
                    };
                    // A host vouching on behalf of an asserted actor is not person authority.
                    // Field-only signatures may leave declaration metadata unsigned.
                    if signature.signer != *person
                        || signature.on_behalf.is_some()
                        || signature.format.is_some()
                        || !signature.signed_fields.is_empty()
                    {
                        return Ok(None);
                    }
                    return Ok(Some(person.clone()));
                }
                let Some(next) = author else {
                    return Ok(None);
                };
                actor = next;
            }
            Ok(None)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let store = Store::open_memory("alder").unwrap();
        store
            .set_node_key(Arc::new(crate::fleet::MemberKey::generate().unwrap().0))
            .unwrap();
        store
    }

    fn declare(store: &Store, name: &str, child: &str, author: Option<&str>, key: &str) {
        let source = format!("version 2\nagent \"{name}\" {{ {child} command \"true\"; }}");
        let intent = crate::graph::parse_internal_intent(&source, "alder").unwrap();
        let expected = intent
            .subjects
            .keys()
            .map(|subject| {
                (
                    subject.clone(),
                    store
                        .selected_desired_token(subject)
                        .unwrap()
                        .into_iter()
                        .collect(),
                )
            })
            .collect();
        if let Some(person) = author.filter(|actor| actor.starts_with("person/")) {
            store.ensure_principal_key(person).unwrap();
        }
        store.apply_as(&intent, &expected, key, author).unwrap();
        store.seal_local_batches().unwrap();
        store.judge_claims(true).unwrap();
    }

    #[test]
    fn trace_principal_follows_only_declaring_seats_to_an_explicit_root() {
        let store = store();
        declare(
            &store,
            "example/root",
            "accountable \"person/avery\";",
            Some("person/avery"),
            "root",
        );
        declare(
            &store,
            "example/parent",
            "",
            Some("agent/example/root"),
            "parent",
        );
        declare(
            &store,
            "example/child",
            "",
            Some("agent/example/parent"),
            "child",
        );
        for agent in [
            "agent/example/root",
            "agent/example/parent",
            "agent/example/child",
        ] {
            assert_eq!(
                store.trace_principal(agent).unwrap().as_deref(),
                Some("person/avery")
            );
        }
        // The authority walk's 16-hop budget does not truncate a valid declaring-seat chain.
        for i in 0..20 {
            let author = if i == 0 {
                "agent/example/child".into()
            } else {
                format!("agent/example/depth{}", i - 1)
            };
            declare(
                &store,
                &format!("example/depth{i}"),
                "",
                Some(&author),
                &format!("depth{i}"),
            );
        }
        assert_eq!(
            store
                .trace_principal("agent/example/depth19")
                .unwrap()
                .as_deref(),
            Some("person/avery")
        );
        assert_eq!(
            store.agent_person("agent/example/depth19").unwrap(),
            None,
            "the existing authority walk retains its 16-hop limit"
        );
    }

    #[test]
    fn trace_principal_cycles_missing_roots_retirement_and_person_authors_give_none() {
        let store = store();
        declare(&store, "example/a", "", Some("agent/example/b"), "a");
        declare(&store, "example/b", "", Some("agent/example/a"), "b");
        declare(
            &store,
            "example/self",
            "",
            Some("agent/example/self"),
            "self",
        );
        declare(
            &store,
            "example/missing",
            "",
            Some("agent/example/absent"),
            "missing",
        );
        declare(&store, "example/person", "", Some("person/avery"), "person");
        declare(&store, "example/no-author", "", None, "no-author");
        for agent in [
            "agent/example/a",
            "agent/example/b",
            "agent/example/self",
            "agent/example/missing",
            "agent/example/person",
            "agent/example/no-author",
            "agent/example/absent",
            "person/avery",
        ] {
            assert_eq!(store.trace_principal(agent).unwrap(), None, "{agent}");
        }
        declare(
            &store,
            "example/root",
            "accountable \"person/avery\";",
            Some("person/avery"),
            "root",
        );
        declare(
            &store,
            "example/child",
            "",
            Some("agent/example/root"),
            "child",
        );
        declare(
            &store,
            "example/root",
            "",
            Some("person/avery"),
            "remove-accountable",
        );
        assert_eq!(store.trace_principal("agent/example/child").unwrap(), None);
        declare(
            &store,
            "example/root",
            "accountable \"person/avery\";",
            Some("person/avery"),
            "restore-root",
        );
        let stopped =
            crate::graph::parse_internal_intent("version 2\nstop \"agent/example/root\"", "alder")
                .unwrap();
        let expected = BTreeMap::from([(
            "agent/example/root".into(),
            store
                .selected_desired_token("agent/example/root")
                .unwrap()
                .into_iter()
                .collect(),
        )]);
        store
            .apply_as(&stopped, &expected, "retire-root", Some("person/avery"))
            .unwrap();
        assert_eq!(store.trace_principal("agent/example/child").unwrap(), None);
    }

    #[test]
    fn trace_principal_never_uses_bound_accounts_or_mission_requesters() {
        let store = Store::open_memory("alder").unwrap();
        let source = r#"version 2
agent "example/bound" { harness "codex" { account-pool "person/robin"; } }
mission "example/work" state="ready" {
  goal "Run one step."
  agent "worker" { command "true"; }
  step "work" { assigned-to "agent/${ST_MISSION_RUN}/worker"; goal "Work."; }
}"#;
        let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
        store.apply_internal(&intent, "bound-and-mission").unwrap();
        assert_eq!(
            store
                .agent_person("agent/example/bound")
                .unwrap()
                .as_deref(),
            Some("person/robin")
        );
        assert_eq!(store.trace_principal("agent/example/bound").unwrap(), None);
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "example/work".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/avery".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "person-work".into(),
            })
            .unwrap();
        let declarations = intent.missions["example/work"]
            .declarations_kdl
            .as_ref()
            .unwrap();
        let mut execution =
            crate::graph::parse_execution_intent(declarations, "alder", &run.id).unwrap();
        for desired in execution.subjects.values_mut() {
            desired.owner_generation = Some(run.generation.clone());
        }
        let expected = execution
            .subjects
            .keys()
            .map(|subject| (subject.clone(), Vec::new()))
            .collect();
        store
            .apply_as(
                &execution,
                &expected,
                "materialize-worker",
                Some(&run.requester),
            )
            .unwrap();
        let seat = format!("agent/{}/worker", run.id);
        assert_eq!(
            store.agent_person(&seat).unwrap().as_deref(),
            Some("person/avery")
        );
        assert_eq!(store.trace_principal(&seat).unwrap(), None);
    }

    #[test]
    fn trace_principal_rejects_unsigned_and_node_vouched_person_authorship() {
        let store = Store::open_memory("alder").unwrap();
        let source =
            "version 2\nagent \"example/root\" { accountable \"person/avery\"; command \"true\"; }";
        let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
        let expected = BTreeMap::from([("agent/example/root".into(), Vec::new())]);
        store
            .apply_as(&intent, &expected, "unsigned-root", Some("person/avery"))
            .unwrap();
        let claim = store
            .selected_desired_token("agent/example/root")
            .unwrap()
            .unwrap();
        assert_eq!(
            store.claim_verdict(&claim).unwrap(),
            smallclaims::principal::Verdict::Unsigned
        );
        assert_eq!(store.trace_principal("agent/example/root").unwrap(), None);
        store
            .set_node_key(Arc::new(crate::fleet::MemberKey::generate().unwrap().0))
            .unwrap();
        store.seal_local_batches().unwrap();
        store.judge_claims(true).unwrap();
        assert_eq!(
            store.claim_verdict(&claim).unwrap(),
            smallclaims::principal::Verdict::Verified
        );
        let signature = store.claim_signature(&claim).unwrap().unwrap();
        assert_eq!(signature.signer, "host/alder");
        assert_eq!(signature.on_behalf.as_deref(), Some("person/avery"));
        assert_eq!(store.trace_principal("agent/example/root").unwrap(), None);
    }

    #[test]
    fn trace_principal_requires_the_exact_person_and_complete_verified_content() {
        use smallclaims::principal::{
            ClaimSignature, FIELDS_FORMAT, Verdict, content_digest, fields_signing_bytes,
        };
        for case in ["verified", "held", "invalid", "other-person", "fields-only"] {
            let store = store();
            let device = crate::fleet::MemberKey::generate().unwrap().0;
            let signer = if case == "other-person" {
                "person/robin"
            } else {
                "person/avery"
            };
            let chain = if case == "held" {
                vec!["claim/missing-delegation".into()]
            } else {
                store
                    .enroll_device_key(signer, device.public(), "example device")
                    .unwrap()
            };
            let source = "version 2\nagent \"example/root\" { accountable \"person/avery\"; command \"true\"; }";
            let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
            let expected = BTreeMap::from([("agent/example/root".into(), Vec::new())]);
            store
                .apply_as(&intent, &expected, "root", Some("person/avery"))
                .unwrap();
            let root = store
                .latest_claim("agent/example/root", Some("intent.desired"))
                .unwrap()
                .unwrap();
            let content =
                content_digest(&root.subject, &root.kind, root.actor.as_deref(), &root.body);
            let mut signature = ClaimSignature::sign(
                &device,
                &content,
                signer,
                None,
                chain,
                root.accepted_at_unix_ms as u64,
            );
            if case == "invalid" {
                signature.signature = device.sign(b"different content");
            }
            if case == "fields-only" {
                signature.format = Some(FIELDS_FORMAT.into());
                signature.signed_fields = vec!["kind".into()];
                signature.signature = device.sign(&fields_signing_bytes(
                    &root.subject,
                    &root.kind,
                    root.actor.as_deref(),
                    &root.body["fields"],
                    &signature.signed_fields,
                    &signature.signer,
                    None,
                    &signature.key,
                    &signature.chain,
                    &signature.nonce,
                    signature.signed_at_unix_ms,
                ));
            }
            store
                .connection
                .batched(|tx| {
                    smallclaims::store::principals::store_claim_signature_tx(
                        tx, &root.id, &signature,
                    )
                })
                .unwrap()
                .unwrap();
            store.seal_local_batches().unwrap();
            store.judge_claims(true).unwrap();
            match case {
                "held" => assert!(matches!(
                    store.claim_verdict(&root.id).unwrap(),
                    Verdict::Held(_)
                )),
                "invalid" => assert!(matches!(
                    store.claim_verdict(&root.id).unwrap(),
                    Verdict::Invalid(_)
                )),
                _ => assert_eq!(
                    store.claim_verdict(&root.id).unwrap(),
                    Verdict::Verified,
                    "{case}"
                ),
            }
            let expected = if case == "verified" {
                Some("person/avery")
            } else {
                None
            };
            assert_eq!(
                store.trace_principal(&root.subject).unwrap().as_deref(),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn trace_principal_rejects_unsigned_or_invalid_intermediate_declarations() {
        for invalid in [false, true] {
            let store = store();
            declare(
                &store,
                "example/root",
                "accountable \"person/avery\";",
                Some("person/avery"),
                "root",
            );
            declare(
                &store,
                "example/child",
                "",
                Some("agent/example/middle"),
                "child",
            );
            let source = "version 2\nagent \"example/middle\" { command \"true\"; }";
            let intent = crate::graph::parse_internal_intent(source, "alder").unwrap();
            let expected = BTreeMap::from([("agent/example/middle".into(), Vec::new())]);
            store
                .apply_as(&intent, &expected, "middle", Some("agent/example/root"))
                .unwrap();
            let middle = store
                .latest_claim("agent/example/middle", Some("intent.desired"))
                .unwrap()
                .unwrap();
            assert_eq!(store.trace_principal("agent/example/child").unwrap(), None);
            if invalid {
                let key = crate::fleet::MemberKey::generate().unwrap().0;
                let signature = smallclaims::principal::ClaimSignature::sign(
                    &key,
                    "wrong content",
                    "host/alder",
                    None,
                    Vec::new(),
                    middle.accepted_at_unix_ms as u64,
                );
                store
                    .connection
                    .batched(|tx| {
                        smallclaims::store::principals::store_claim_signature_tx(
                            tx, &middle.id, &signature,
                        )
                    })
                    .unwrap()
                    .unwrap();
            }
            store.seal_local_batches().unwrap();
            store.judge_claims(true).unwrap();
            if invalid {
                assert!(matches!(
                    store.claim_verdict(&middle.id).unwrap(),
                    smallclaims::principal::Verdict::Invalid(_)
                ));
                assert_eq!(store.trace_principal("agent/example/child").unwrap(), None);
            } else {
                assert_eq!(
                    store.claim_verdict(&middle.id).unwrap(),
                    smallclaims::principal::Verdict::Verified
                );
                assert_eq!(
                    store
                        .trace_principal("agent/example/child")
                        .unwrap()
                        .as_deref(),
                    Some("person/avery")
                );
            }
        }
    }

    #[test]
    fn trace_principal_hop_budget_includes_the_root_and_fails_closed() {
        let store = store();
        declare(
            &store,
            "example/root",
            "accountable \"person/avery\";",
            Some("person/avery"),
            "root",
        );
        for i in 0..64 {
            let parent = if i == 0 {
                "agent/example/root".into()
            } else {
                format!("agent/example/chain{}", i - 1)
            };
            declare(
                &store,
                &format!("example/chain{i}"),
                "",
                Some(&parent),
                &format!("chain{i}"),
            );
        }
        assert_eq!(
            store
                .trace_principal("agent/example/chain62")
                .unwrap()
                .as_deref(),
            Some("person/avery")
        );
        assert_eq!(
            store.trace_principal("agent/example/chain63").unwrap(),
            None
        );
    }
}
