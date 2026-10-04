//! Debug-only app carrier. The Swift module guards access in Release.
mod bridge;
mod wire;

use anyhow::{Context, Result, bail};
use bridge::Bridge;
use iroh::{EndpointAddr, EndpointId, SecretKey};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString, c_char},
    sync::{Mutex, OnceLock},
};
use tokio::runtime::{Builder, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static BRIDGE: Mutex<Option<Bridge>> = Mutex::new(None);

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
        if target_json.is_null() {
            bail!("missing fabric target");
        }
        let target: Target = serde_json::from_str(unsafe { CStr::from_ptr(target_json) }.to_str()?)
            .context("invalid fabric target JSON")?;
        let node: EndpointId = target
            .node
            .parse()
            .context("invalid fabric member NodeID")?;
        bridge::validate_service(&target.service)?;
        let addr = target.addr.unwrap_or_else(|| EndpointAddr::new(node));
        if addr.id != node {
            bail!("fabric address hint does not match member NodeID");
        }
        let mut current = BRIDGE
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric bridge state unavailable"))?;
        runtime().block_on(async {
            if let Some(old) = current.take() { old.stop().await; }
            let next = Bridge::start(key, addr, target.service).await?;
            let description = json!({"url": next.url, "node": next.node, "fabricVersion": "0.2.30+8bd9017", "irohVersion": "1.0.2"});
            *current = Some(next);
            Ok(description)
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn st_fabric_stop() -> *mut c_char {
    reply(|| {
        let mut current = BRIDGE
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric bridge state unavailable"))?;
        if let Some(old) = current.take() {
            runtime().block_on(old.stop());
        }
        Ok(json!(null))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn st_fabric_network_change() -> *mut c_char {
    reply(|| {
        let current = BRIDGE
            .lock()
            .map_err(|_| anyhow::anyhow!("fabric bridge state unavailable"))?;
        if let Some(bridge) = current.as_ref() {
            runtime().block_on(bridge.endpoint.network_change());
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
