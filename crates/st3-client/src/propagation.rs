//! Outbound daemon context shared by the typed client and the st CLI client.
//! The caller owns SDK/subscriber/propagator initialization; this module never exports.
//! With `trace-propagation` disabled, all injection helpers are no-ops.

#[cfg(feature = "trace-propagation")]
use opentelemetry::propagation::Injector;
#[cfg(feature = "trace-propagation")]
use opentelemetry::trace::{SpanContext, TraceContextExt as _, TraceState};
#[cfg(feature = "trace-propagation")]
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

#[cfg(feature = "trace-propagation")]
struct HeaderSetter<F>(F);

#[cfg(feature = "trace-propagation")]
impl<F: FnMut(&str, String)> Injector for HeaderSetter<F> {
    fn set(&mut self, key: &str, value: String) {
        (self.0)(key, value);
    }
}

/// Move the collector marker to the front while retaining at most 32 W3C entries.
#[cfg(feature = "trace-propagation")]
fn collector_trace_state(state: &TraceState) -> TraceState {
    if state.get("st").is_some() {
        return state
            .insert("st", "c")
            .expect("st=c is a valid W3C tracestate entry");
    }
    // TraceState 0.30 exposes neither iteration nor a length limit. Reconstruct once,
    // keeping the 31 leftmost entries so a full list loses only its rightmost entry.
    let header = state.header();
    TraceState::from_key_value(
        std::iter::once(("st", "c")).chain(
            header
                .split(',')
                .filter_map(|entry| entry.split_once('='))
                .take(31),
        ),
    )
    .expect("existing tracestate entries and st=c are valid")
}

/// Inject the current tracing span's valid OTel context, marking sampling as collector-owned.
/// Without an OTel tracing layer this only checks the context: no allocation or propagator access.
#[cfg(feature = "trace-propagation")]
pub fn inject_current_context(set: impl FnMut(&str, String)) {
    let context = tracing::Span::current().context();
    let span = context.span();
    let parent = span.span_context();
    if !parent.is_valid() {
        return;
    }
    let state = collector_trace_state(parent.trace_state());
    let propagated = SpanContext::new(
        parent.trace_id(),
        parent.span_id(),
        parent.trace_flags(),
        false,
        state,
    );
    let context = context.with_remote_span_context(propagated);
    let mut injector = HeaderSetter(set);
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut injector);
    });
}

/// Add context directly to a reqwest builder without an intermediate header collection.
#[cfg(feature = "trace-propagation")]
pub fn inject_http(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let mut request = Some(request);
    inject_current_context(|key, value| {
        request = Some(
            request
                .take()
                .expect("request builder is present")
                .header(key, value),
        );
    });
    request.expect("request builder is present")
}

/// Add context directly to a hyper or WebSocket handshake's headers.
#[cfg(feature = "trace-propagation")]
pub fn inject_headers(headers: &mut hyper::HeaderMap) {
    inject_current_context(|key, value| {
        if let (Ok(key), Ok(value)) = (
            hyper::header::HeaderName::from_bytes(key.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(key, value);
        }
    });
}

/// No propagation dependencies or context lookup when the feature is disabled.
#[cfg(not(feature = "trace-propagation"))]
pub fn inject_current_context(_set: impl FnMut(&str, String)) {}

/// Return the request unchanged when trace propagation is disabled.
#[cfg(not(feature = "trace-propagation"))]
pub fn inject_http(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
}

/// Leave headers unchanged when trace propagation is disabled.
#[cfg(not(feature = "trace-propagation"))]
pub fn inject_headers(_headers: &mut hyper::HeaderMap) {}

#[cfg(all(test, feature = "trace-propagation"))]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanId, TraceFlags, TraceId, TraceState, TracerProvider as _};
    use tracing_subscriber::prelude::*;

    #[test]
    fn collector_marker_truncates_full_trace_state() {
        let state = TraceState::from_key_value(
            (0..32).map(|index| (format!("vendor{index}"), "value")),
        )
        .unwrap();
        let propagated = collector_trace_state(&state);
        let header = propagated.header();
        assert_eq!(header.split(',').count(), 32);
        assert!(header.starts_with("st=c,"));
        assert_eq!(propagated.get("vendor30"), Some("value"));
        assert_eq!(propagated.get("vendor31"), None);
        assert_eq!(state.get("vendor31"), Some("value"));
    }

    #[test]
    fn collector_marker_replaces_without_growing_full_trace_state() {
        let state = TraceState::from_key_value(
            (0..31)
                .map(|index| (format!("vendor{index}"), "value"))
                .chain(std::iter::once(("st".to_owned(), "old"))),
        )
        .unwrap();
        let propagated = collector_trace_state(&state);
        let header = propagated.header();
        assert_eq!(header.split(',').count(), 32);
        assert!(header.starts_with("st=c,"));
        assert_eq!(propagated.get("vendor30"), Some("value"));
        assert_eq!(header.matches("st=").count(), 1);
        assert_eq!(state.get("st"), Some("old"));
    }

    #[test]
    fn otel_invalid_context_sends_no_headers() {
        let subscriber = tracing_subscriber::registry();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("without_sdk");
            let _entered = span.enter();
            inject_current_context(|_, _| panic!("invalid context must not inject"));
            let request = inject_http(reqwest::Client::new().get("http://localhost/v1/health"))
                .build()
                .unwrap();
            assert!(!request.headers().contains_key("traceparent"));
            assert!(!request.headers().contains_key("tracestate"));
            let mut headers = hyper::HeaderMap::new();
            inject_headers(&mut headers);
            assert!(headers.is_empty());
            let websocket =
                crate::websocket_request("ws://localhost/v1/client/terminal", None, None, None)
                    .unwrap();
            assert!(!websocket.headers().contains_key("traceparent"));
            assert!(!websocket.headers().contains_key("tracestate"));
        });
    }

    #[test]
    fn otel_request_builders_continue_context_with_collector_sampling() {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("client");
            span.set_parent(opentelemetry::Context::new().with_remote_span_context(
                SpanContext::new(
                    TraceId::from_hex("1234567890abcdef1234567890abcdef").unwrap(),
                    SpanId::from_hex("1234567890abcdef").unwrap(),
                    TraceFlags::SAMPLED,
                    true,
                    TraceState::from_key_value([("vendor", "value"), ("st", "old")]).unwrap(),
                ),
            ));
            let _entered = span.enter();
            let context = span.context();
            let parent = context.span();
            let parent = parent.span_context();
            let expected = format!("00-{}-{}-01", parent.trace_id(), parent.span_id());
            let http = inject_http(reqwest::Client::new().get("http://localhost/v1/health"))
                .build()
                .unwrap();
            let mut unix = hyper::Request::builder()
                .uri("/v1/health")
                .body(())
                .unwrap();
            inject_headers(unix.headers_mut());
            let websocket =
                crate::websocket_request("ws://localhost/v1/client/terminal", None, None, None)
                    .unwrap();
            for headers in [http.headers(), unix.headers(), websocket.headers()] {
                assert_eq!(headers["traceparent"], expected);
                assert_eq!(headers["tracestate"], "st=c,vendor=value");
            }
            // The sampling marker is a wire-only change, not a mutation of the current span.
            assert_eq!(parent.trace_state().get("st"), Some("old"));
        });
    }
}
