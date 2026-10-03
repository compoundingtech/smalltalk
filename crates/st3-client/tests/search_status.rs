use st3_client::{ClientError, ErrorCode, ErrorEnvelope, plain_message};

#[test]
fn search_index_status_is_retryable_and_does_not_report_an_unreachable_host() {
    for (code, message) in [
        (
            ErrorCode::SearchIndexBuilding,
            "conversation search index is being built; retry shortly",
        ),
        (ErrorCode::SearchIndexFailed, "inventory failed"),
    ] {
        let wire = serde_json::json!({
            "api_version":"st3.client.v0", "error_version":"st3.client.error.v0",
            "request_id":"search-test", "code":code, "message":message,
            "retryable":true, "details":{"retry_after_ms":1000}
        });
        let envelope: ErrorEnvelope = serde_json::from_value(wire).unwrap();
        assert_eq!(envelope.code, code);
        let rendered = plain_message(Some(&code), message);
        assert!(rendered.contains("indexing"));
        assert!(!rendered.contains("cannot be reached"));
        assert!(ClientError::Api(code, message.into(), Box::new(envelope)).is_transient());
    }
}
