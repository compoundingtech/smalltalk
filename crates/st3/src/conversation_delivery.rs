//! An exact native identity retained by the existing fenced consumption receipt.
//! No transcript text, native payload, or client idempotency key is retained here.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeReceipt {
    pub session_id: String,
    pub message_id: String,
}

impl NativeReceipt {
    pub(crate) fn valid(&self) -> bool {
        self.session_id
            .strip_prefix("session/external-")
            .is_some_and(|id| id.len() == 24 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            && !self.message_id.is_empty()
            && self.message_id.len() <= 256
            && self.message_id.bytes().all(|byte| byte.is_ascii_graphic())
    }
}

/// Called only after the existing native consumption predicate matched, and only with
/// the current wrapper's verified Claude binding. Queue acceptance/removal is not a
/// normalized user-record identity and deliberately supplies no link.
pub(crate) fn claude_record(record: &Value, native_session: &str) -> Option<NativeReceipt> {
    if record["type"] != "user" || record.pointer("/message/role")?.as_str()? != "user" {
        return None;
    }
    if record
        .get("sessionId")
        .is_some_and(|id| id.as_str() != Some(native_session))
    {
        return None;
    }
    // This is the same explicit native id that normalize_claude preserves. Never use
    // its ordinal fallback: an ordinal is not a receipt's stable record identity.
    let id = record
        .pointer("/message/id")
        .and_then(Value::as_str)
        .or_else(|| record.get("uuid").and_then(Value::as_str))?;
    let receipt = NativeReceipt {
        session_id: crate::external_sessions::external_session_id(
            crate::external_sessions::ExternalDriver::Claude,
            native_session,
        ),
        message_id: id.to_owned(),
    };
    receipt.valid().then_some(receipt)
}

/// Hash only the receipt's immutable meaning, not its acceptance timestamp or local
/// row position. This is an equality witness after a table copy, not an authority token.
pub(crate) fn receipt_fingerprint(
    subject: &str,
    actor: &str,
    origin: &str,
    body: &Value,
) -> anyhow::Result<String> {
    use sha2::{Digest as _, Sha256};
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        subject,
        "message.read",
        actor,
        origin,
        // Claim storage canonicalizes object keys; serde_json preserves insertion order.
        // Hash the same meaning before append and after reading that stored body.
        smallclaims::hash::canonical_json_value(body),
    ))?)))
}

/// Multiple graph receipts for a native turn are not a single-valued relationship.
/// Keep the ambiguity rather than choosing the newest claim or comparing message text.
pub(crate) fn unique_links(
    session: &str,
    receipts: impl IntoIterator<Item = (String, NativeReceipt)>,
) -> BTreeMap<String, String> {
    let mut candidates: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (subject, receipt) in receipts {
        if !subject.starts_with("message/")
            || subject.len() <= 8
            || subject.len() > 512
            || subject.chars().any(char::is_whitespace)
            || receipt.session_id != session
            || !receipt.valid()
        {
            continue;
        }
        candidates
            .entry(receipt.message_id)
            .and_modify(|candidate| *candidate = None)
            .or_insert(Some(subject));
    }
    candidates
        .into_iter()
        .filter_map(|(native, subject)| subject.map(|subject| (native, subject)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn receipt_fingerprint_survives_canonical_storage_but_detects_changed_meaning() {
        let appended: Value = serde_json::from_str(
            r#"{"fields":{"status":"read"},"evidence":[],"_operation":{"request_digest":"digest","id":"operation/fixture"}}"#,
        ).unwrap();
        let stored = smallclaims::hash::canonical_json_text(&appended).unwrap();
        assert_ne!(serde_json::to_string(&appended).unwrap(), stored);
        let mut restored: Value = serde_json::from_str(&stored).unwrap();
        let fingerprint = |body: &Value| {
            receipt_fingerprint("message/fixture", "agent/fixture", "fixture", body).unwrap()
        };
        assert_eq!(fingerprint(&appended), fingerprint(&restored));
        restored["fields"]["status"] = json!("closed");
        assert_ne!(fingerprint(&appended), fingerprint(&restored));
    }

    #[test]
    fn only_an_explicit_user_identity_in_the_bound_session_is_eligible() {
        let user = json!({"type":"user", "uuid":"native-user", "sessionId":"native-session",
            "message":{"role":"user", "content":"An invented delivery"}});
        let proof = claude_record(&user, "native-session").unwrap();
        assert_eq!(proof.message_id, "native-user");
        assert!(claude_record(&user, "other-session").is_none());
        let mut missing = user.clone();
        missing.as_object_mut().unwrap().remove("uuid");
        assert!(claude_record(&missing, "native-session").is_none());
        assert!(
            claude_record(
                &json!({"type":"queue-operation", "operation":"remove",
            "reason":"absorbed_mid_turn", "commandUuid":"native-user"}),
                "native-session"
            )
            .is_none()
        );
    }

    #[test]
    fn two_receipts_for_one_turn_do_not_choose_a_message() {
        let receipt = NativeReceipt {
            session_id: "session/external-0123456789abcdef01234567".into(),
            message_id: "native-one".into(),
        };
        assert_eq!(
            unique_links(
                &receipt.session_id,
                [("message/one".into(), receipt.clone())]
            )["native-one"],
            "message/one"
        );
        assert!(
            unique_links(
                &receipt.session_id,
                [
                    ("message/one".into(), receipt.clone()),
                    ("message/two".into(), receipt.clone())
                ]
            )
            .is_empty()
        );
        assert!(unique_links(&receipt.session_id, std::iter::empty()).is_empty());
    }
}
