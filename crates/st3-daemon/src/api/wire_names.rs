//! Receive-side spelling aliases; writers retain their deployed spelling.
use axum::http::HeaderMap;

use super::{ApiError, client_v0::validation};

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both
// Sunset: 60 days after the first release shipping this bridge. Switch senders only
// after every fleet member accepts the new names; remove old readers at contraction.
const HEADERS: &[(&str, &str)] = &[
    ("x-st3-person", "x-fractal-person"),
    ("x-st3-client", "x-fractal-client"),
    ("x-st3-features", "x-fractal-features"),
    ("x-st3-raw-origin", "x-fractal-raw-origin"),
];

pub(super) fn receive_headers(headers: &mut HeaderMap) -> Result<(), ApiError> {
    for &(old, new) in HEADERS {
        let mut values = headers.get_all(old).iter().chain(headers.get_all(new).iter());
        if let Some(first) = values.next() {
            if values.any(|value| value != first) {
                return Err(validation(format!("conflicting {old} / {new} headers")));
            }
            if !headers.contains_key(old) {
                let value = first.clone();
                headers.insert(old, value);
            }
        }
    }
    Ok(())
}

pub(super) fn api_version(version: &str) -> bool {
    matches!(version, "st3.client.v0" | "fractal.client.v0")
}

pub(super) fn protocol_matches(value: &str, old: &str) -> bool {
    value == old || old.strip_prefix("st3.client.").is_some_and(|suffix| {
        value.strip_prefix("fractal.client.") == Some(suffix)
    })
}

pub(super) fn selected_protocol<'a>(protocols: &[&'a str], old: &str) -> Option<&'a str> {
    match protocols {
        [protocol] if protocol_matches(protocol, old) => Some(protocol),
        _ => None,
    }
}

pub(super) fn capability(protocol: &str) -> Option<&str> {
    protocol.strip_prefix("st3.cap.")
        .or_else(|| protocol.strip_prefix("fractal.cap."))
        .filter(|value| !value.is_empty())
}

pub(super) fn terminal_protocol<'a>(
    protocols: &[&'a str], old: &str, allow_direct: bool,
) -> Option<(&'a str, Option<&'a str>)> {
    match protocols {
        [protocol] if allow_direct && protocol_matches(protocol, old) => Some((protocol, None)),
        [first, second] => {
            if protocol_matches(first, old) {
                Some((*first, Some(capability(second)?)))
            } else if protocol_matches(second, old) {
                Some((*second, Some(capability(first)?)))
            } else {
                None
            }
        }
        _ => None,
    }
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-wire-fractal-accept-both

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_accept_each_spelling_and_reject_conflicts() {
        for &(old, new) in HEADERS {
            for name in [old, new] {
                let mut headers = HeaderMap::new();
                headers.insert(name, "same".parse().unwrap());
                receive_headers(&mut headers).unwrap();
                assert_eq!(headers[old], "same");
            }
            let mut headers = HeaderMap::new();
            headers.insert(old, "same".parse().unwrap());
            headers.insert(new, "same".parse().unwrap());
            receive_headers(&mut headers).unwrap();
            headers.insert(new, "different".parse().unwrap());
            assert!(receive_headers(&mut headers).is_err());
            headers.remove(new);
            headers.append(old, "different".parse().unwrap());
            assert!(receive_headers(&mut headers).is_err());
        }
    }

    #[test]
    fn terminal_receivers_accept_aliases_and_reject_conflicting_capabilities() {
        for old in ["st3.client.terminal.v0", "st3.client.pty.v0"] {
            let new = old.replacen("st3.", "fractal.", 1);
            for protocol in [old, new.as_str()] {
                for prefix in ["st3.cap.", "fractal.cap."] {
                    let cap = format!("{prefix}secret");
                    assert_eq!(terminal_protocol(&[protocol, &cap], old, false),
                        Some((protocol, Some("secret"))));
                    assert_eq!(terminal_protocol(&[protocol], old, true), Some((protocol, None)));
                }
                assert!(terminal_protocol(&[protocol, "st3.cap.a", "fractal.cap.b"], old, false).is_none());
            }
            assert!(terminal_protocol(&[old, &new], old, false).is_none());
        }
    }

    #[test]
    fn protocols_accept_aliases_but_not_multiple_choices() {
        for suffix in ["terminal.v0", "pty.v0", "conversation.v0", "collections.v0"] {
            let old = format!("st3.client.{suffix}");
            let new = format!("fractal.client.{suffix}");
            assert_eq!(selected_protocol(&[&old], &old), Some(old.as_str()));
            assert_eq!(selected_protocol(&[&new], &old), Some(new.as_str()));
            assert!(selected_protocol(&[&old, &new], &old).is_none());
        }
        for prefix in ["st3.cap.", "fractal.cap."] {
            assert_eq!(capability(&format!("{prefix}secret")), Some("secret"));
            assert!(capability(prefix).is_none());
        }
        for version in ["st3.client.v0", "fractal.client.v0"] {
            assert!(api_version(version));
        }
        assert!(!api_version("fractal.client.v1"));
    }
}
