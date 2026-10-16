//! Durable delivery holds, separate from runtime presence and harness protocol holds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{ClaimInput, St3Error};
use crate::store::Store;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HoldRequest {
    pub subject: String,
    pub actor: String,
    pub held: bool,
    pub until_unix_ms: u64,
    pub reason: String,
    pub idempotency_key: String,
    /// Used only when a replacement adopts an older provider's unexpired DND.
    #[serde(default)]
    pub legacy_adoption: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HoldView {
    pub subject: String,
    pub active: bool,
    pub until_unix_ms: Option<u64>,
    pub reason: Option<String>,
    pub actor: Option<String>,
    pub claim: Option<String>,
}

pub fn view(store: &Store, subject: &str, now: u64) -> anyhow::Result<HoldView> {
    let claim = store.latest_claim(subject, Some("delivery.hold"))?;
    let fields = claim.as_ref().and_then(|claim| claim.body.get("fields"));
    let until = fields.and_then(|fields| fields["until_unix_ms"].as_u64());
    Ok(HoldView {
        subject: subject.into(),
        active: fields.is_some_and(|fields| fields["held"] == true)
            && until.is_some_and(|until| now < until),
        until_unix_ms: until,
        reason: fields
            .and_then(|fields| fields["reason"].as_str())
            .map(str::to_owned),
        actor: claim.as_ref().and_then(|claim| claim.actor.clone()),
        claim: claim.map(|claim| claim.id),
    })
}

pub fn input(request: HoldRequest, now: u64) -> Result<ClaimInput, St3Error> {
    st3_schema::registry()
        .validate_subject(&request.subject)
        .map_err(|error| St3Error::new(error.code, error.message))?;
    let own = request.actor == request.subject;
    if !request.subject.starts_with("agent/")
        || !(own || request.actor.starts_with("person/"))
        || (request.legacy_adoption && !own)
    {
        return Err(St3Error::new(
            "delivery-hold-forbidden",
            "a delivery hold requires the seat's actor or a person; legacy adoption requires the seat",
        ));
    }
    st3_schema::registry()
        .validate_subject(&request.actor)
        .map_err(|error| St3Error::new(error.code, error.message))?;
    if request.reason.trim().is_empty()
        || request.reason.len() > 2048
        || (request.held && request.until_unix_ms <= now)
        || (!request.held && request.until_unix_ms != 0)
        || (request.legacy_adoption
            && (!request.held || request.until_unix_ms > now.saturating_add(16 * 60 * 1000)))
    {
        return Err(St3Error::new(
            "invalid-delivery-hold",
            "a hold needs a nonempty reason and future expiry; a release has expiry zero",
        ));
    }
    Ok(ClaimInput {
        subject: request.subject,
        kind: "delivery.hold".into(),
        actor: Some(request.actor),
        fields: BTreeMap::from([
            ("held".into(), Value::Bool(request.held)),
            ("until_unix_ms".into(), Value::from(request.until_unix_ms)),
            ("reason".into(), Value::String(request.reason)),
            (
                "legacy_adoption".into(),
                Value::Bool(request.legacy_adoption),
            ),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(request.idempotency_key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(held: bool, until: u64, key: &str) -> HoldRequest {
        HoldRequest {
            subject: "agent/eval/held".into(),
            actor: "person/alex".into(),
            held,
            until_unix_ms: until,
            reason: "quiet interval".into(),
            idempotency_key: key.into(),
            legacy_adoption: false,
        }
    }

    #[test]
    fn expiry_release_restart_and_adoption_preserve_graph_authority() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("claims.sqlite3");
        {
            let store = Store::open(&database, "eval").unwrap();
            assert!(!view(&store, "agent/eval/held", 100).unwrap().active);
            store
                .append_claim(&input(request(true, 200, "hold"), 100).unwrap())
                .unwrap();
            assert!(view(&store, "agent/eval/held", 199).unwrap().active);
            assert!(!view(&store, "agent/eval/held", 200).unwrap().active);
        }
        let store = Store::open(&database, "eval").unwrap();
        assert!(view(&store, "agent/eval/held", 150).unwrap().active);
        let release = store
            .append_claim(&input(request(false, 0, "release"), 150).unwrap())
            .unwrap();
        let mut adoption = request(true, 250, "adoption");
        adoption.actor = adoption.subject.clone();
        adoption.legacy_adoption = true;
        let imported = store.append_claim(&input(adoption, 150).unwrap()).unwrap();
        assert_eq!(
            imported.id, release.id,
            "adoption cannot reassert a released hold"
        );
        assert!(!view(&store, "agent/eval/held", 150).unwrap().active);
    }

    #[test]
    fn only_dedicated_authorized_operations_accept_holds() {
        let store = Store::open_memory("eval").unwrap();
        let claim = input(request(true, 200, "hold"), 100).unwrap();
        assert!(store.append_client_claim(&claim).is_err());
        let mut foreign = request(true, 200, "foreign");
        foreign.actor = "agent/eval/other".into();
        assert!(input(foreign, 100).is_err());
        assert!(input(request(true, 100, "expired"), 100).is_err());
        assert!(input(request(false, 200, "bad-release"), 100).is_err());
    }
}
