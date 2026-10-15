//! Debug-only app carrier. The Swift module guards access in Release.
mod bridge;
mod diagnostics;
mod wire;

use anyhow::{Context, Result, bail};
use bridge::Bridge;
use iroh::{EndpointAddr, EndpointId, SecretKey};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char},
    sync::{Mutex, OnceLock},
};
use tokio::runtime::{Builder, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static TARGETS: Mutex<Option<Targets>> = Mutex::new(None);

#[derive(Default)]
struct Targets {
    bridges: HashMap<String, (bridge::PathMode, Bridge)>,
    endpoints: HashMap<bridge::PathMode, iroh::Endpoint>,
    identity: Option<String>,
    legacy: Option<String>,
    next: u64,
}

impl Targets {
    async fn open(
        &mut self,
        key: SecretKey,
        addr: EndpointAddr,
        service: String,
        mode: bridge::PathMode,
    ) -> Result<Value> {
        let identity = key.public().to_string();
        if self
            .identity
            .as_ref()
            .is_some_and(|current| current != &identity)
        {
            bail!("another fabric identity already has active targets");
        }
        if self.bridges.len() >= 2 {
            bail!("the fabric bridge already has two targets");
        }
        if !self.endpoints.contains_key(&mode) {
            self.endpoints
                .insert(mode, Bridge::endpoint(key, mode).await?);
        }
        self.next += 1;
        let handle = format!("target-{}", self.next);
        let endpoint = self.endpoints[&mode].clone();
        let next = match Bridge::start_on(endpoint, addr, service, handle.clone(), false).await {
            Ok(next) => next,
            Err(error) => {
                if !self.bridges.values().any(|(other, _)| *other == mode) {
                    if let Some(endpoint) = self.endpoints.remove(&mode) {
                        endpoint.close().await;
                    }
                }
                return Err(error);
            }
        };
        let description = json!({"handle": handle, "url": next.url, "node": next.node,
            "fabricVersion": "0.2.41+84c3208", "irohVersion": "1.0.2"});
        self.bridges.insert(handle, (mode, next));
        self.identity = Some(identity);
        Ok(description)
    }

    async fn close(&mut self, handle: &str) {
        if let Some((mode, bridge)) = self.bridges.remove(handle) {
            bridge.stop().await;
            if !self.bridges.values().any(|(other, _)| *other == mode) {
                if let Some(endpoint) = self.endpoints.remove(&mode) {
                    endpoint.close().await;
                }
            }
        }
        if self.legacy.as_deref() == Some(handle) {
            self.legacy = None;
        }
        if self.bridges.is_empty() {
            self.identity = None;
        }
    }
}

unsafe fn target(pointer: *const c_char) -> Result<(Target, EndpointAddr)> {
    if pointer.is_null() {
        bail!("missing fabric target");
    }
    let target: Target = serde_json::from_str(unsafe { CStr::from_ptr(pointer) }.to_str()?)
        .context("invalid fabric target JSON")?;
    let node: EndpointId = target
        .node
        .parse()
        .context("invalid fabric member NodeID")?;
    bridge::validate_service(&target.service)?;
    let addr = target
        .addr
        .clone()
        .unwrap_or_else(|| EndpointAddr::new(node));
    if addr.id != node {
        bail!("fabric address hint does not match member NodeID");
    }
    Ok((target, addr))
}

unsafe fn handle(pointer: *const c_char) -> Result<String> {
    if pointer.is_null() {
        bail!("missing fabric target handle");
    }
    Ok(unsafe { CStr::from_ptr(pointer) }.to_str()?.to_owned())
}

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Tokio runtime")
    })
}

#[derive(Deserialize)]
struct Target {
    node: String,
    service: String,
    #[serde(default)]
    addr: Option<EndpointAddr>,
    #[serde(default)]
    mode: bridge::PathMode,
}

fn reply(operation: impl FnOnce() -> Result<Value>) -> *mut c_char {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
    let value = match result {
        Ok(Ok(value)) => json!({"ok": true, "value": value}),
        Ok(Err(error)) => json!({"ok": false, "error": error.to_string()}),
        Err(_) => json!({"ok": false, "error": "fabric native operation failed"}),
    };
    CString::new(value.to_string())
        .expect("JSON has no NUL")
        .into_raw()
}

unsafe fn key(pointer: *const u8, length: usize) -> Result<SecretKey> {
    if pointer.is_null() || length != 32 {
        bail!("invalid fabric identity length");
    }
    // SAFETY: caller provides 32 readable key bytes for the duration of the call.
    let bytes: [u8; 32] = unsafe { std::slice::from_raw_parts(pointer, length) }.try_into()?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// # Safety
/// `secret` must contain `length` readable bytes (32); free the reply with string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn st_fabric_identity(secret: *const u8, length: usize) -> *mut c_char {
    reply(|| Ok(json!({"node": unsafe { key(secret, length)? }.public().to_string()})))
}

/// # Safety
/// Key bytes and the NUL-terminated UTF-8 target JSON must remain readable throughout the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn st_fabric_dial(
    secret: *const u8,
    length: usize,
    target_json: *const c_char,
) -> *mut c_char {
    reply(|| {
        let key = unsafe { key(secret, length)? };
        let (target, addr) = unsafe { target(target_json)? };
        let mut guard = TARGETS
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric target state unavailable"))?;
        let current = guard.get_or_insert_with(Targets::default);
        runtime().block_on(async {
            if let Some(old) = current.legacy.clone() {
                current.close(&old).await;
            }
            let description = current.open(key, addr, target.service, target.mode).await?;
            current.legacy = description["handle"].as_str().map(str::to_owned);
            Ok(description)
        })
    })
}

/// # Safety
/// Key bytes and target JSON remain readable throughout the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn st_fabric_open_target(
    secret: *const u8,
    length: usize,
    target_json: *const c_char,
) -> *mut c_char {
    reply(|| {
        let key = unsafe { key(secret, length)? };
        let (target, addr) = unsafe { target(target_json)? };
        let mut guard = TARGETS
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric target state unavailable"))?;
        runtime().block_on(guard.get_or_insert_with(Targets::default).open(
            key,
            addr,
            target.service,
            target.mode,
        ))
    })
}

/// # Safety
/// The handle is a readable NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn st_fabric_close_target(target_handle: *const c_char) -> *mut c_char {
    reply(|| {
        let handle = unsafe { handle(target_handle)? };
        let mut guard = TARGETS
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric target state unavailable"))?;
        if let Some(current) = guard.as_mut() {
            runtime().block_on(current.close(&handle));
        }
        Ok(json!(null))
    })
}

/// # Safety
/// The handle is a readable NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn st_fabric_target_stats(target_handle: *const c_char) -> *mut c_char {
    reply(|| {
        Ok(diagnostics::snapshot_for(Some(&unsafe {
            handle(target_handle)?
        })))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn st_fabric_stats() -> *mut c_char {
    reply(|| {
        let guard = TARGETS
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric target state unavailable"))?;
        Ok(diagnostics::snapshot_for(Some(
            guard
                .as_ref()
                .and_then(|current| current.legacy.as_deref())
                .unwrap_or("no-gateway"),
        )))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn st_fabric_stop() -> *mut c_char {
    reply(|| {
        let mut guard = TARGETS
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric target state unavailable"))?;
        if let Some(current) = guard.as_mut() {
            if let Some(handle) = current.legacy.clone() {
                runtime().block_on(current.close(&handle));
            }
        }
        Ok(json!(null))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn st_fabric_stop_all() -> *mut c_char {
    reply(|| {
        let mut guard = TARGETS
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric target state unavailable"))?;
        if let Some(current) = guard.as_mut() {
            let handles: Vec<_> = current.bridges.keys().cloned().collect();
            runtime().block_on(async {
                for handle in handles {
                    current.close(&handle).await;
                }
            });
        }
        Ok(json!(null))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn st_fabric_target_count() -> usize {
    TARGETS
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|current| current.bridges.len()))
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn st_fabric_network_change() -> *mut c_char {
    reply(|| {
        let guard = TARGETS
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric target state unavailable"))?;
        if let Some(current) = guard.as_ref() {
            runtime().block_on(async {
                for endpoint in current.endpoints.values() {
                    endpoint.network_change().await;
                }
            });
        }
        Ok(json!(null))
    })
}

/// # Safety
/// `pointer` must be a reply returned by this library and freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn st_fabric_string_free(pointer: *mut c_char) {
    if !pointer.is_null() {
        drop(unsafe { CString::from_raw(pointer) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn targets_share_the_endpoint_and_close_independently() -> Result<()> {
        let key = SecretKey::generate();
        let owner = EndpointAddr::new(SecretKey::generate().public());
        let mut targets = Targets::default();
        let first = targets
            .open(
                key.clone(),
                owner.clone(),
                "demo-client/0".into(),
                bridge::PathMode::Direct,
            )
            .await?;
        let second = targets
            .open(
                key.clone(),
                owner.clone(),
                "demo-client/0".into(),
                bridge::PathMode::Direct,
            )
            .await?;
        assert_ne!(first["url"], second["url"]);
        assert_eq!(
            targets.endpoints.len(),
            1,
            "one endpoint for this identity/path mode"
        );
        assert_eq!(targets.bridges.len(), 2);
        assert!(
            targets
                .open(key, owner, "demo-client/0".into(), bridge::PathMode::Direct)
                .await
                .is_err()
        );
        let second_url = second["url"]
            .as_str()
            .unwrap()
            .trim_start_matches("http://");
        targets.close(first["handle"].as_str().unwrap()).await;
        assert_eq!(targets.endpoints.len(), 1);
        // Stopping one listener leaves the other's accept loop running.
        let stream = tokio::net::TcpStream::connect(second_url).await?;
        drop(stream);
        targets.close(second["handle"].as_str().unwrap()).await;
        assert!(targets.endpoints.is_empty());
        assert!(targets.bridges.is_empty());
        assert!(tokio::net::TcpStream::connect(second_url).await.is_err());
        Ok(())
    }
}
