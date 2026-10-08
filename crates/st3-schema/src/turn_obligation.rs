//! Optional bounded receipt evidence, admitted identically for local and replicated claims.
use super::*;

pub(crate) fn validate(fields: &BTreeMap<String, Value>) -> Result<(), ValidationError> {
    let Some(ledger) = fields.get("turn_obligation") else {
        return Ok(());
    };
    let invalid = || {
        error(
            "invalid-turn-obligation",
            "turn obligation must contain bounded, fenced source receipts and exact terminal evidence",
        )
    };
    if ledger["sequence"].as_u64().is_none() || ledger["unknown"].as_bool().is_none() {
        return Err(invalid());
    }
    if ledger
        .get("unknown_tool_outcome")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid());
    }
    let open = ledger["open"].as_array().ok_or_else(invalid)?;
    let terminal = ledger["terminal"].as_array().ok_or_else(invalid)?;
    if open.len() > 32
        || terminal.len() > 64
        || serde_json::to_vec(ledger).map_err(|_| invalid())?.len() > 24 * 1024
    {
        return Err(invalid());
    }
    let text = |value: &Value| {
        value
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 4096)
    };
    if let Some(evidence) = ledger
        .get("unknown_evidence")
        .filter(|value| !value.is_null())
        && (evidence["ownership_sequence"]
            .as_u64()
            .is_none_or(|n| n == 0)
            || evidence["source_sequence"].as_u64().is_none()
            || evidence["observed_at_ms"].as_u64().is_none()
            || !text(&evidence["reason"])
            || (!evidence["provider_incarnation"].is_null()
                && !text(&evidence["provider_incarnation"])))
    {
        return Err(invalid());
    }
    let receipt = |value: &Value| -> Result<(), ValidationError> {
        if !text(&value["provider_incarnation"])
            || value["ownership_sequence"].as_u64().is_none_or(|n| n == 0)
            || value["source_sequence"].as_u64().is_none_or(|n| n == 0)
            || value["started_at_ms"].as_u64().is_none()
            || value["pending_human"].as_bool().is_none()
            || value["tool_outcome_unknown"].as_bool().is_none()
        {
            return Err(invalid());
        }
        for name in [
            "runtime_incarnation",
            "desired_revision",
            "native_session_id",
            "native_turn_id",
        ] {
            if !value[name].is_null() && !text(&value[name]) {
                return Err(invalid());
            }
        }
        if let Some(sequence) = fields.get("ownership_sequence").and_then(Value::as_u64) {
            if value["ownership_sequence"]
                .as_u64()
                .is_some_and(|entry| entry > sequence)
            {
                return Err(invalid());
            }
            if fields.get("evidence_incarnation") == value.get("provider_incarnation")
                && (value["ownership_sequence"].as_u64() != Some(sequence)
                    || (!value["runtime_incarnation"].is_null()
                        && fields.get("incarnation_id") != value.get("runtime_incarnation")))
            {
                return Err(invalid());
            }
        }
        if value.get("pending_tool_ids").is_some_and(|ids| {
            ids.as_array()
                .is_none_or(|ids| ids.len() > 128 || ids.iter().any(|id| !text(id)))
        }) {
            return Err(invalid());
        }
        Ok(())
    };
    for entry in open {
        receipt(entry)?;
    }
    let validate_terminal = |proof: &Value| -> Result<(), ValidationError> {
        receipt(&proof["obligation"])?;
        if !matches!(
            proof["outcome"].as_str(),
            Some("completed" | "cancelled" | "failed")
        ) || !text(&proof["evidence_provider_incarnation"])
            || proof["observed_at_ms"].as_u64().is_none()
        {
            return Err(invalid());
        }
        // Cross-provider closure requires all native and launch fences. A producer receipt
        // alone proves a terminal only from the provider that retained that start boundary.
        if proof["evidence_provider_incarnation"] != proof["obligation"]["provider_incarnation"]
            && ["desired_revision", "native_session_id", "native_turn_id"]
                .iter()
                .any(|key| !text(&proof["obligation"][key]))
        {
            return Err(invalid());
        }
        Ok(())
    };
    for proof in terminal {
        validate_terminal(proof)?;
    }
    if let Some(results) = ledger.get("tool_results") {
        let results = results.as_array().ok_or_else(invalid)?;
        if results.len() > 64 {
            return Err(invalid());
        }
        for result in results {
            validate_terminal(&result["terminal"])?;
            if !text(&result["tool_id"])
                || !text(&result["evidence_provider_incarnation"])
                || result["observed_at_ms"].as_u64().is_none()
                || result["terminal"]["obligation"]["pending_tool_ids"]
                    .as_array()
                    .is_none_or(|ids| ids.iter().any(|id| id == &result["tool_id"]))
                || (result["evidence_provider_incarnation"]
                    != result["terminal"]["obligation"]["provider_incarnation"]
                    && ["desired_revision", "native_session_id", "native_turn_id"]
                        .iter()
                        .any(|key| !text(&result["terminal"]["obligation"][key])))
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_acknowledgement(
    fields: &BTreeMap<String, Value>,
) -> Result<(), ValidationError> {
    let invalid = || {
        error(
            "invalid-turn-acknowledgement",
            "acknowledgement needs one to 32 distinct receipt/evidence pairs, a captured declaration/cut and a reason",
        )
    };
    let receipts = fields
        .get("receipts")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if receipts.is_empty()
        || receipts.len() > 32
        || fields["captured_cut"].as_u64().is_none()
        || fields["source_revision"]
            .as_str()
            .is_none_or(|s| s.is_empty() || s.len() > 4096)
        || fields["reason"]
            .as_str()
            .is_none_or(|s| s.trim().is_empty() || s.len() > 4096)
    {
        return Err(invalid());
    }
    let mut seen = std::collections::BTreeSet::new();
    for receipt in receipts {
        let key = receipt["receipt"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096)
            .ok_or_else(invalid)?;
        if !seen.insert(key)
            || receipt["source_claim"]
                .as_str()
                .is_none_or(|s| s.is_empty() || s.len() > 4096)
        {
            return Err(invalid());
        }
    }
    Ok(())
}
