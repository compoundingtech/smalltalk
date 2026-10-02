//! Two stores sync with nothing but smallclaims: one founds a fleet, the other joins it with a
//! code, and each runs the sync worker over loopback HTTP.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use smallclaims::claim::ClaimInput;
use smallclaims::fleet::join::{self, InviteOptions, JoinOptions, MemberSettings};
use smallclaims::fleet::{FleetFile, FleetMode};
use smallclaims::store::Store;
use smallclaims::store::runtime::Plain;
use smallclaims::sync::{self, Local, WorkerConfig};
use tokio::sync::watch;

/// A member's store, activated from its `fleet.toml`, and its worker running in this process.
struct Member {
    store: Arc<Store>,
    wake: watch::Sender<u64>,
}

impl Member {
    fn start(state_dir: &Path, node: &str) -> Self {
        let file = FleetFile::load(state_dir).unwrap().unwrap();
        let store =
            Arc::new(Store::open(&join::store_path(state_dir), node, Arc::new(Plain)).unwrap());
        smallclaims::fleet::activate(&store, state_dir, &file).unwrap();
        let (wake, _) = watch::channel(0);
        let config = WorkerConfig::from_fleet_file(state_dir, node).unwrap();
        tokio::spawn(sync::run(
            config,
            Local(store.clone()),
            wake.clone(),
            axum::Router::new(),
        ));
        Self { store, wake }
    }

    fn note(&self, subject: &str, text: &str) {
        self.store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "example.note".into(),
                actor: Some("person/ada".into()),
                fields: BTreeMap::from([("text".into(), Value::String(text.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        self.wake.send_modify(|generation| *generation += 1);
    }

    async fn wait_for(&self, subject: &str, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let found = self
                .store
                .claims_for(subject, Some("example.note"))
                .unwrap();
            if found.len() >= count {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{subject} reached {} of {count} claims",
                found.len()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_joins_a_fleet_and_claims_sync_both_ways() {
    let ada_dir = tempfile::tempdir().unwrap();
    let grace_dir = tempfile::tempdir().unwrap();
    let settings = MemberSettings {
        mode: FleetMode::Listening,
        port: Some(join::free_port(42_000).unwrap()),
        transports: Some(Vec::new()),
        advertise_loopback: true,
        ..MemberSettings::default()
    };
    let founded = join::found(ada_dir.path(), "ada", &settings).unwrap();
    let ada = Member::start(ada_dir.path(), "ada");

    // The worker announces ada's loopback listener; an invite carries it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let invitation = loop {
        match join::invite(
            &ada.store,
            &founded.fleet_id,
            "ada",
            &InviteOptions {
                name: Some("grace".into()),
                lifetime: Duration::from_secs(600),
                via: "loopback".into(),
                person: "person/ada".into(),
                migrate: false,
            },
        ) {
            Ok(invitation) => break invitation,
            Err(error) if error.code == "no-endpoints" => {
                assert!(tokio::time::Instant::now() < deadline, "{}", error.message);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("{}: {}", error.code, error.message),
        }
    };

    let joined = join::join(&JoinOptions {
        state_dir: grace_dir.path().to_path_buf(),
        configured_node: "grace".into(),
        code: invitation.code,
        name: None,
        via: None,
        settings: MemberSettings {
            mode: FleetMode::DialOut,
            transports: Some(Vec::new()),
            ..MemberSettings::default()
        },
        legacy_secret_file: None,
        fabric_protocol: None,
        runtime: Arc::new(Plain),
        version: env!("CARGO_PKG_VERSION").into(),
    })
    .await
    .unwrap();
    assert_eq!(joined.fleet_id, founded.fleet_id);
    assert_eq!(joined.sponsor, "ada");
    let grace = Member::start(grace_dir.path(), "grace");

    ada.note("note/plans", "start with the log");
    grace.wait_for("note/plans", 1).await;
    grace.note("note/plans", "then the replicas");
    ada.wait_for("note/plans", 2).await;
    grace.wait_for("note/plans", 2).await;

    for member in [&ada, &grace] {
        let view = member.store.fleet_view().unwrap();
        let mut names = view
            .members
            .iter()
            .filter(|member| member.state == "current")
            .map(|member| member.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["ada", "grace"]);
    }
}
