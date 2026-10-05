//! Native settlement outbox. Commands are reserved by the owner and are never retried here.
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt as _;
use st3::client::Client;
use st3::mailbox::Fence;
use st3_schema::harness_control::{Binding, ControlCommand, NativeObservation, NativeReceipt};

#[derive(Default, Serialize, Deserialize)]
pub struct NativeControls {
    observation: Option<NativeObservation>,
    binding: Option<Binding>,
    outbox: Option<PathBuf>,
}
impl NativeControls {
    pub fn is_active(&self) -> bool {
        self.observation.is_some() || self.binding.is_some()
    }
    pub fn bind_outbox(&mut self, dir: PathBuf) -> Result<()> {
        std::fs::create_dir_all(&dir)?;
        self.outbox = Some(dir);
        Ok(())
    }
    pub fn accept(&mut self, line: &str, subject: &str, fence: Option<&Fence>) -> Result<bool> {
        let Ok(mut frame) = serde_json::from_str::<Value>(line) else { return Ok(false); };
        match frame["type"].as_str() {
            Some("harness_control_state") => {
                frame.as_object_mut().context("native state is not an object")?.remove("type");
                self.observation = Some(serde_json::from_value(frame)?);
                // A native lifecycle observation invalidates the previous delivery baseline.
                self.binding = None;
                Ok(true)
            }
            Some("harness_control_receipt") => {
                let fence = fence.context("native control has no mailbox fence")?;
                frame.as_object_mut().context("native receipt is not an object")?.remove("type");
                frame["subject"] = json!(subject);
                let receipt: NativeReceipt = serde_json::from_value(frame)?;
                let dir = self.outbox.as_ref().context("native control has no durable outbox")?;
                let name = hex::encode(Sha256::digest(receipt.operation_id.as_bytes()));
                let pending = dir.join(format!("{name}.pending"));
                let final_path = dir.join(format!("{name}.json"));
                let bytes = serde_json::to_vec(&json!({"fence":fence,"receipt":receipt}))?;
                let mut file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(&pending)?;
                std::io::Write::write_all(&mut file, &bytes)?;
                file.sync_all()?;
                std::fs::rename(pending, final_path)?;
                std::fs::File::open(dir)?.sync_all()?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
    pub async fn flush(&mut self, client: &Client, fence: Option<&Fence>, stdout: &mut tokio::io::Stdout, dispatch: bool) -> Result<()> {
        let Some(fence) = fence else { return Ok(()); };
        if let Some(dir) = &self.outbox {
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if path.extension().is_none_or(|extension| extension != "json") { continue; }
                let mut request: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                // Re-exec rotates the mailbox lease, not the native operation identity.
                // Only the same seat and graph incarnation may republish stored proof.
                if request.pointer("/fence/subject").and_then(Value::as_str) == Some(fence.subject.as_str())
                    && request.pointer("/fence/incarnation").and_then(Value::as_str) == Some(fence.incarnation.as_str()) {
                    request["fence"] = json!(fence);
                }
                match client.post::<_, Value>("/v1/harness-control/receipts", &request).await {
                    Ok(_) => std::fs::remove_file(&path)?,
                    Err(error) if matches!(st3::client::api_error_code(&error), Some("stale-mailbox-session" | "stale-harness-control" | "already-settled")) => {
                        tracing::warn!("native control settlement retained as rejected evidence: {error:#}");
                        std::fs::rename(&path, path.with_extension("rejected"))?;
                    }
                    Err(error) => return Err(error),
                }
                std::fs::File::open(dir)?.sync_all()?;
            }
        }
        if let Some(observation) = &self.observation {
            let binding: Binding = client.post("/v1/harness-control/state", &json!({"fence":fence,"observation":observation})).await?;
            stdout.write_all(format!("{}\n", json!({"type":"harness_control_binding","binding":binding})).as_bytes()).await?;
            stdout.flush().await?;
            self.binding = Some(binding);
            self.observation = None;
        }
        if dispatch && self.binding.is_some() {
            let command: Option<ControlCommand> = client.post("/v1/harness-control/next", fence).await?;
            if let Some(command) = command {
                let command = serde_json::to_value(command)?;
                stdout.write_all(format!("{}\n", json!({"type":"harness_control","command":command})).as_bytes()).await?;
                stdout.flush().await?;
            }
        }
        Ok(())
    }
}
