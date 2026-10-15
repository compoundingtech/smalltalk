//! Linux proof harness for the same FFI exported to Swift; never reads member configuration.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString},
    io::{BufRead, Write},
    path::Path,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let path = Path::new(args.get(1).context("provide an isolated key file")?);
    let key = if path.exists() {
        std::fs::read(path)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected a 32-byte proof key"))?
    } else {
        let key = iroh::SecretKey::generate().to_bytes();
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
            .write_all(&key)?;
        key
    };
    if args.get(2).is_some_and(|arg| arg == "--identity") {
        println!("{}", iroh::SecretKey::from_bytes(&key).public());
        return Ok(());
    }
    for line in std::io::stdin().lock().lines() {
        let command: Value = serde_json::from_str(&line?)?;
        let argument = CString::new(if command["op"] == "open" {
            command["target"].to_string()
        } else {
            command["handle"].as_str().unwrap_or_default().into()
        })?;
        let pointer = unsafe {
            match command["op"].as_str() {
                Some("open") => st_fabric_bridge::st_fabric_open_target(
                    key.as_ptr(),
                    key.len(),
                    argument.as_ptr(),
                ),
                Some("close") => st_fabric_bridge::st_fabric_close_target(argument.as_ptr()),
                Some("stats") => st_fabric_bridge::st_fabric_target_stats(argument.as_ptr()),
                _ => bail!("unknown bridge proof operation"),
            }
        };
        let result = unsafe { CStr::from_ptr(pointer).to_string_lossy().into_owned() };
        unsafe { st_fabric_bridge::st_fabric_string_free(pointer) };
        println!(
            "{}",
            json!({"id":command["id"], "result":serde_json::from_str::<Value>(&result)?})
        );
        std::io::stdout().flush()?;
    }
    st_fabric_bridge::st_fabric_stop_all();
    Ok(())
}
