//! The two framing helpers every stdio native channel shares.
//!
//! A native channel is a child of the interactive harness, speaking newline-delimited JSON over
//! stdio: the Claude MCP watcher ([`crate::claude_mcp`]) and the pi-family channel
//! ([`crate::pi_channel`]). What a delivered inbox message looks like to the model, and how a
//! frame is terminated on the wire, are st2's decisions rather than each channel's — so they are
//! decided once, here.

use std::io::Write;

use anyhow::Result;
use serde_json::Value;

/// The envelope a delivered inbox message is handed to the model in. A message with no subject is
/// its body verbatim: an empty `Subject:` line would be noise the model has to read past.
pub(crate) fn channel_content(subject: Option<&str>, body: &str) -> String {
    match subject.filter(|value| !value.is_empty()) {
        Some(subject) => format!("Subject: {subject}\n\n{body}"),
        None => body.to_owned(),
    }
}

/// One frame on the wire: compact JSON followed by the newline that terminates it.
pub(crate) fn write_json(out: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_content_reuses_one_subject_and_body_envelope() {
        assert_eq!(
            channel_content(Some("subject"), "body"),
            "Subject: subject\n\nbody"
        );
        assert_eq!(channel_content(None, "body"), "body");
        assert_eq!(channel_content(Some(""), "body"), "body");
    }

    /// The wire is newline-delimited: a reader splitting on newlines must see exactly one frame
    /// per value, and a body carrying its own newline must not be able to forge a second one.
    #[test]
    fn each_frame_is_one_newline_terminated_line() {
        let mut out = Vec::new();
        write_json(&mut out, &serde_json::json!({"a": 1})).unwrap();
        write_json(
            &mut out,
            &serde_json::json!({"content": "line one\nline two"}),
        )
        .unwrap();
        let framed = String::from_utf8(out).unwrap();
        assert_eq!(framed, "{\"a\":1}\n{\"content\":\"line one\\nline two\"}\n");
        assert_eq!(framed.lines().count(), 2);
    }
}
