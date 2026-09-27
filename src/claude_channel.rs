//! Install the Claude Code channel plugin that is embedded in the st2 binary.
//!
//! Claude Code admits channels by plugin and marketplace identity. The plugin is user state, while
//! its allowlist is machine policy. `st2 claude-channel install` owns both steps and elevates only
//! the small policy write.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const MARKETPLACE: &str = "st2";
pub const PLUGIN: &str = "st2-channel";
pub const CHANNEL: &str = "plugin:st2-channel@st2";
pub const ST3_MARKETPLACE: &str = "st3";
pub const ST3_PLUGIN: &str = "st3-channel";
pub const ST3_CHANNEL: &str = "plugin:st3-channel@st3";
const PLUGINS: [&str; 2] = [PLUGIN, ST3_PLUGIN];

const MARKETPLACE_MANIFEST: &[u8] =
    include_bytes!("../claude-channel/.claude-plugin/marketplace.json");
const PLUGIN_MANIFEST: &[u8] =
    include_bytes!("../claude-channel/plugins/st2-channel/.claude-plugin/plugin.json");
const MCP_CONFIG: &[u8] = include_bytes!("../claude-channel/plugins/st2-channel/.mcp.json");
const ST3_PLUGIN_MANIFEST: &[u8] =
    include_bytes!("../claude-channel/plugins/st3-channel/.claude-plugin/plugin.json");
const ST3_MCP_CONFIG: &[u8] = include_bytes!("../claude-channel/plugins/st3-channel/.mcp.json");
const ST3_MARKETPLACE_MANIFEST: &[u8] =
    include_bytes!("../st3-claude-channel/.claude-plugin/marketplace.json");
const POLICY_FILE: &str = "50-st2-channel.json";
const ST3_POLICY_FILE: &str = "50-st3-channel.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    pub marketplace: PathBuf,
    pub policy: PathBuf,
}

pub fn install(no_policy: bool) -> Result<InstallPaths> {
    let marketplace = marketplace_root()?;
    install_marketplace_at(&marketplace)?;
    install_with_claude(&marketplace)?;
    let policy = policy_path()?;
    if !no_policy {
        ensure_policy_with_elevation(&policy)?;
    }
    println!("marketplace\t{}", marketplace.display());
    for plugin in PLUGINS {
        println!("plugin\t{plugin}@{MARKETPLACE}");
    }
    if no_policy {
        println!("policy\tskipped");
    } else {
        println!("policy\t{}", policy.display());
    }
    Ok(InstallPaths {
        marketplace,
        policy,
    })
}

pub fn status() -> Result<()> {
    let marketplace = marketplace_root()?;
    let policy = policy_path()?;
    let assets = verify_marketplace_at(&marketplace).is_ok();
    let policy_ready = policy_is_current_at(&policy);
    let marketplace_ready = marketplace_registration()
        .ok()
        .flatten()
        .is_some_and(|entry| marketplace_entry_matches(&entry, &marketplace));
    let installed = claude_json(["plugin", "list", "--json"]).ok();
    let plugin_ready = installed.as_ref().is_some_and(|value| {
        PLUGINS
            .iter()
            .all(|plugin| json_contains(value, &format!("{plugin}@{MARKETPLACE}")))
    });
    println!("assets\t{}", state(assets));
    println!("marketplace\t{}", state(marketplace_ready));
    println!("plugins\t{}", state(plugin_ready));
    println!("policy\t{}", state(policy_ready));
    if assets && marketplace_ready && plugin_ready && policy_ready {
        Ok(())
    } else {
        bail!("the Claude channel installation is incomplete")
    }
}

pub fn uninstall(keep_policy: bool) -> Result<()> {
    let marketplace = marketplace_root()?;
    let owns_marketplace = marketplace_registration()?
        .is_some_and(|entry| marketplace_entry_matches(&entry, &marketplace));
    for plugin in PLUGINS {
        if plugin_is_installed(plugin)? {
            anyhow::ensure!(
                owns_marketplace,
                "refusing to uninstall {plugin}@{MARKETPLACE} from another marketplace source"
            );
            run_claude(&[
                "plugin",
                "uninstall",
                &format!("{plugin}@{MARKETPLACE}"),
                "--scope",
                "user",
                "--yes",
            ])?;
        }
    }
    if owns_marketplace {
        run_claude(&["plugin", "marketplace", "remove", MARKETPLACE])?;
    }
    if marketplace.exists() {
        fs::remove_dir_all(&marketplace)
            .with_context(|| format!("removing {}", marketplace.display()))?;
    }
    let policy = policy_path()?;
    if !keep_policy && policy.exists() {
        remove_policy_with_elevation(&policy)?;
    }
    println!("uninstalled");
    Ok(())
}

/// The elevated half of `install`. This command must not install user-scoped plugin state.
pub fn install_policy() -> Result<PathBuf> {
    let path = policy_path()?;
    install_policy_at(&path)?;
    println!("policy\t{}", path.display());
    Ok(path)
}

/// The elevated half of `uninstall`. This removes only the st2-owned policy fragment.
pub fn uninstall_policy() -> Result<()> {
    let path = policy_path()?;
    remove_policy_at(&path)?;
    println!("policy removed\t{}", path.display());
    Ok(())
}

pub fn verify_installed() -> Result<()> {
    let marketplace = marketplace_root()?;
    verify_marketplace_at(&marketplace)?;
    if !marketplace_is_registered_at(&marketplace)? {
        bail!("the st2 Claude marketplace is not registered; run `st2 claude-channel install`");
    }
    if !plugin_is_installed(PLUGIN)? {
        bail!("the st2 Claude channel plugin is not installed; run `st2 claude-channel install`");
    }
    let policy = policy_path()?;
    if !policy_is_current_at(&policy) {
        bail!("the st2 Claude channel policy is not installed; run `st2 claude-channel install`");
    }
    Ok(())
}

pub fn verify_st3_installed() -> Result<()> {
    let marketplace = st3_marketplace_root()?;
    verify_st3_marketplace_at(&marketplace)?;
    if !st3_marketplace_is_registered_at(&marketplace)? {
        bail!("the st3 Claude marketplace is not registered; run `st3 claude-channel install`");
    }
    if !st3_plugin_is_installed()? {
        bail!("the st3 Claude channel plugin is not installed; run `st3 claude-channel install`");
    }
    let policy = st3_policy_path()?;
    if !st3_policy_is_current_at(&policy) {
        bail!("the st3 Claude channel policy is not installed; run `st3 claude-channel install`");
    }
    Ok(())
}

/// Install the ST3-owned Claude channel under an ST3 marketplace identity. This is deliberately
/// separate from [`install`]: the migration may add or remove ST3 without changing ST2's public
/// plugin identity or command behavior.
pub fn install_st3(no_policy: bool) -> Result<InstallPaths> {
    let marketplace = st3_marketplace_root()?;
    install_st3_marketplace_at(&marketplace)?;
    install_st3_with_claude(&marketplace)?;
    let policy = st3_policy_path()?;
    if !no_policy {
        ensure_st3_policy_with_elevation(&policy)?;
    }
    println!("marketplace\t{}", marketplace.display());
    println!("plugin\t{ST3_PLUGIN}@{ST3_MARKETPLACE}");
    if no_policy {
        println!("policy\tskipped");
    } else {
        println!("policy\t{}", policy.display());
    }
    Ok(InstallPaths {
        marketplace,
        policy,
    })
}

pub fn status_st3() -> Result<()> {
    let marketplace = st3_marketplace_root()?;
    let policy = st3_policy_path()?;
    let assets = verify_st3_marketplace_at(&marketplace).is_ok();
    let marketplace_ready = st3_marketplace_registration()
        .ok()
        .flatten()
        .is_some_and(|entry| marketplace_entry_matches(&entry, &marketplace));
    let installed = claude_json(["plugin", "list", "--json"]).ok();
    let plugin_ready = installed
        .as_ref()
        .is_some_and(|value| json_contains(value, &format!("{ST3_PLUGIN}@{ST3_MARKETPLACE}")));
    let policy_ready = st3_policy_is_current_at(&policy);
    println!("assets\t{}", state(assets));
    println!("marketplace\t{}", state(marketplace_ready));
    println!("plugin\t{}", state(plugin_ready));
    println!("policy\t{}", state(policy_ready));
    if assets && marketplace_ready && plugin_ready && policy_ready {
        Ok(())
    } else {
        bail!("the st3 Claude channel installation is incomplete")
    }
}

pub fn uninstall_st3(keep_policy: bool) -> Result<()> {
    let marketplace = st3_marketplace_root()?;
    let owns_marketplace = st3_marketplace_registration()?
        .is_some_and(|entry| marketplace_entry_matches(&entry, &marketplace));
    if st3_plugin_is_installed()? {
        anyhow::ensure!(
            owns_marketplace,
            "refusing to uninstall {ST3_PLUGIN}@{ST3_MARKETPLACE} from another marketplace source"
        );
        run_claude(&[
            "plugin",
            "uninstall",
            &format!("{ST3_PLUGIN}@{ST3_MARKETPLACE}"),
            "--scope",
            "user",
            "--yes",
        ])?;
    }
    if owns_marketplace {
        run_claude(&["plugin", "marketplace", "remove", ST3_MARKETPLACE])?;
    }
    if marketplace.exists() {
        fs::remove_dir_all(&marketplace)
            .with_context(|| format!("removing {}", marketplace.display()))?;
    }
    let policy = st3_policy_path()?;
    if !keep_policy && policy.exists() {
        remove_st3_policy_with_elevation(&policy)?;
    }
    println!("uninstalled");
    Ok(())
}

pub fn install_st3_policy() -> Result<PathBuf> {
    let path = st3_policy_path()?;
    install_st3_policy_at(&path)?;
    println!("policy\t{}", path.display());
    Ok(path)
}

pub fn uninstall_st3_policy() -> Result<()> {
    let path = st3_policy_path()?;
    remove_policy_at(&path)?;
    println!("policy removed\t{}", path.display());
    Ok(())
}

fn state(ready: bool) -> &'static str {
    if ready { "ready" } else { "missing" }
}

fn data_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share"))
}

fn marketplace_root() -> Result<PathBuf> {
    Ok(data_home()?.join("st2/claude-channel/marketplace"))
}

fn st3_marketplace_root() -> Result<PathBuf> {
    Ok(data_home()?.join("st3/claude-channel/marketplace"))
}

#[cfg(target_os = "linux")]
fn policy_path() -> Result<PathBuf> {
    Ok(PathBuf::from("/etc/claude-code/managed-settings.d").join(POLICY_FILE))
}

#[cfg(target_os = "linux")]
fn st3_policy_path() -> Result<PathBuf> {
    Ok(PathBuf::from("/etc/claude-code/managed-settings.d").join(ST3_POLICY_FILE))
}

#[cfg(target_os = "macos")]
fn policy_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.d")
            .join(POLICY_FILE),
    )
}

#[cfg(target_os = "macos")]
fn st3_policy_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.d")
            .join(ST3_POLICY_FILE),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn policy_path() -> Result<PathBuf> {
    bail!("the Claude channel policy installer supports Linux and macOS")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn st3_policy_path() -> Result<PathBuf> {
    bail!("the Claude channel policy installer supports Linux and macOS")
}

fn embedded_files() -> [(&'static str, &'static [u8]); 5] {
    [
        (".claude-plugin/marketplace.json", MARKETPLACE_MANIFEST),
        (
            "plugins/st2-channel/.claude-plugin/plugin.json",
            PLUGIN_MANIFEST,
        ),
        ("plugins/st2-channel/.mcp.json", MCP_CONFIG),
        (
            "plugins/st3-channel/.claude-plugin/plugin.json",
            ST3_PLUGIN_MANIFEST,
        ),
        ("plugins/st3-channel/.mcp.json", ST3_MCP_CONFIG),
    ]
}

pub fn install_marketplace_at(root: &Path) -> Result<()> {
    for (relative, bytes) in embedded_files() {
        let path = root.join(relative);
        let parent = path.parent().expect("an embedded file has a parent");
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    verify_marketplace_at(root)
}

pub fn verify_marketplace_at(root: &Path) -> Result<()> {
    for (relative, expected) in embedded_files() {
        let path = root.join(relative);
        let actual = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if actual != expected {
            bail!("embedded Claude channel file differs at {}", path.display());
        }
    }
    Ok(())
}

fn st3_embedded_files() -> [(&'static str, &'static [u8]); 3] {
    [
        (".claude-plugin/marketplace.json", ST3_MARKETPLACE_MANIFEST),
        (
            "plugins/st3-channel/.claude-plugin/plugin.json",
            ST3_PLUGIN_MANIFEST,
        ),
        ("plugins/st3-channel/.mcp.json", ST3_MCP_CONFIG),
    ]
}

pub fn install_st3_marketplace_at(root: &Path) -> Result<()> {
    for (relative, bytes) in st3_embedded_files() {
        let path = root.join(relative);
        let parent = path.parent().expect("an embedded file has a parent");
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    verify_st3_marketplace_at(root)
}

pub fn verify_st3_marketplace_at(root: &Path) -> Result<()> {
    for (relative, expected) in st3_embedded_files() {
        let path = root.join(relative);
        let actual = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if actual != expected {
            bail!("embedded Claude channel file differs at {}", path.display());
        }
    }
    Ok(())
}

fn policy_value() -> Value {
    json!({
        "channelsEnabled": true,
        "allowedChannelPlugins": [
            {"marketplace": MARKETPLACE, "plugin": PLUGIN},
            {"marketplace": MARKETPLACE, "plugin": ST3_PLUGIN}
        ]
    })
}

fn policy_bytes() -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(&policy_value())
        .expect("the built-in Claude channel policy is serializable");
    bytes.push(b'\n');
    bytes
}

fn st3_policy_value() -> Value {
    json!({
        "channelsEnabled": true,
        "allowedChannelPlugins": [
            {"marketplace": ST3_MARKETPLACE, "plugin": ST3_PLUGIN}
        ]
    })
}

fn st3_policy_bytes() -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(&st3_policy_value())
        .expect("the built-in st3 channel policy is serializable");
    bytes.push(b'\n');
    bytes
}

fn install_st3_policy_at(path: &Path) -> Result<()> {
    let parent = path.parent().context("the policy path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    fs::write(path, st3_policy_bytes()).with_context(|| format!("writing {}", path.display()))?;
    if st3_policy_is_current_at(path) {
        Ok(())
    } else {
        bail!("Claude channel policy differs at {}", path.display())
    }
}

fn st3_policy_is_current_at(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == st3_policy_bytes())
}

pub fn install_policy_at(path: &Path) -> Result<()> {
    let parent = path.parent().context("the policy path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    fs::write(path, policy_bytes()).with_context(|| format!("writing {}", path.display()))?;
    verify_policy_at(path)
}

pub fn remove_policy_at(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn policy_is_current_at(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == policy_bytes())
}

fn verify_policy_at(path: &Path) -> Result<()> {
    if policy_is_current_at(path) {
        Ok(())
    } else {
        bail!("Claude channel policy differs at {}", path.display())
    }
}

fn ensure_policy_with_elevation(path: &Path) -> Result<()> {
    if policy_is_current_at(path) {
        return Ok(());
    }
    if is_root() {
        return install_policy_at(path);
    }
    run_elevated("install-policy")
}

fn remove_policy_with_elevation(path: &Path) -> Result<()> {
    if is_root() {
        return remove_policy_at(path);
    }
    run_elevated("uninstall-policy")
}

fn ensure_st3_policy_with_elevation(path: &Path) -> Result<()> {
    if st3_policy_is_current_at(path) {
        return Ok(());
    }
    if is_root() {
        return install_st3_policy_at(path);
    }
    run_st3_elevated("install-policy")
}

fn remove_st3_policy_with_elevation(path: &Path) -> Result<()> {
    if is_root() {
        return remove_policy_at(path);
    }
    run_st3_elevated("uninstall-policy")
}

fn run_st3_elevated(action: &str) -> Result<()> {
    let exe = env::current_exe().context("resolving the current st3 executable")?;
    let status = Command::new("sudo")
        .arg(exe)
        .args(["claude-channel", action])
        .status()
        .with_context(|| format!("running the elevated st3 Claude channel {action}"))?;
    if !status.success() {
        bail!("the elevated st3 Claude channel {action} failed with status {status}");
    }
    Ok(())
}

#[cfg(unix)]
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

fn run_elevated(action: &str) -> Result<()> {
    let exe = env::current_exe().context("resolving the current st2 executable")?;
    let status = Command::new("sudo")
        .arg(exe)
        .args(["claude-channel", action])
        .status()
        .with_context(|| format!("running the elevated Claude channel {action}"))?;
    if !status.success() {
        bail!("the elevated Claude channel {action} failed with status {status}");
    }
    Ok(())
}

fn install_with_claude(marketplace: &Path) -> Result<()> {
    match marketplace_registration()? {
        Some(entry) if marketplace_entry_matches(&entry, marketplace) => {
            run_claude(&["plugin", "marketplace", "update", MARKETPLACE])?;
        }
        Some(entry) => {
            bail!(
                "Claude marketplace name '{MARKETPLACE}' already belongs to another source: {entry}"
            );
        }
        None => {
            let path = marketplace
                .to_str()
                .context("the Claude channel marketplace path is not UTF-8")?;
            run_claude(&["plugin", "marketplace", "add", path, "--scope", "user"])?;
        }
    }
    for plugin in PLUGINS {
        if plugin_is_installed(plugin)? {
            run_claude(&[
                "plugin",
                "uninstall",
                &format!("{plugin}@{MARKETPLACE}"),
                "--scope",
                "user",
                "--yes",
                "--keep-data",
            ])?;
        }
        run_claude(&[
            "plugin",
            "install",
            &format!("{plugin}@{MARKETPLACE}"),
            "--scope",
            "user",
            "--yes",
        ])?;
    }
    Ok(())
}

fn install_st3_with_claude(marketplace: &Path) -> Result<()> {
    match st3_marketplace_registration()? {
        Some(entry) if marketplace_entry_matches(&entry, marketplace) => {
            run_claude(&["plugin", "marketplace", "update", ST3_MARKETPLACE])?;
        }
        Some(entry) => {
            bail!(
                "Claude marketplace name '{ST3_MARKETPLACE}' already belongs to another source: {entry}"
            );
        }
        None => {
            let path = marketplace
                .to_str()
                .context("the Claude channel marketplace path is not UTF-8")?;
            run_claude(&["plugin", "marketplace", "add", path, "--scope", "user"])?;
        }
    }
    if st3_plugin_is_installed()? {
        run_claude(&[
            "plugin",
            "uninstall",
            &format!("{ST3_PLUGIN}@{ST3_MARKETPLACE}"),
            "--scope",
            "user",
            "--yes",
            "--keep-data",
        ])?;
    }
    run_claude(&[
        "plugin",
        "install",
        &format!("{ST3_PLUGIN}@{ST3_MARKETPLACE}"),
        "--scope",
        "user",
        "--yes",
    ])
}

fn marketplace_registration() -> Result<Option<Value>> {
    let value = claude_json(["plugin", "marketplace", "list", "--json"])?;
    Ok(value.as_array().and_then(|entries| {
        entries
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(MARKETPLACE))
            .cloned()
    }))
}

fn st3_marketplace_registration() -> Result<Option<Value>> {
    let value = claude_json(["plugin", "marketplace", "list", "--json"])?;
    Ok(value.as_array().and_then(|entries| {
        entries
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(ST3_MARKETPLACE))
            .cloned()
    }))
}

fn st3_marketplace_is_registered_at(root: &Path) -> Result<bool> {
    Ok(
        st3_marketplace_registration()?
            .is_some_and(|entry| marketplace_entry_matches(&entry, root)),
    )
}

fn marketplace_is_registered_at(root: &Path) -> Result<bool> {
    Ok(marketplace_registration()?.is_some_and(|entry| marketplace_entry_matches(&entry, root)))
}

fn marketplace_entry_matches(entry: &Value, root: &Path) -> bool {
    entry.get("source").and_then(Value::as_str) == Some("directory")
        && entry.get("path").and_then(Value::as_str) == root.to_str()
}

fn plugin_is_installed(plugin: &str) -> Result<bool> {
    let value = claude_json(["plugin", "list", "--json"])?;
    Ok(json_contains(&value, &format!("{plugin}@{MARKETPLACE}")))
}

fn st3_plugin_is_installed() -> Result<bool> {
    let value = claude_json(["plugin", "list", "--json"])?;
    Ok(json_contains(
        &value,
        &format!("{ST3_PLUGIN}@{ST3_MARKETPLACE}"),
    ))
}

fn json_contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value == needle,
        Value::Array(values) => values.iter().any(|value| json_contains(value, needle)),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| key == needle || json_contains(value, needle)),
        _ => false,
    }
}

fn claude_json<const N: usize>(args: [&str; N]) -> Result<Value> {
    let output = claude_output(&args)?;
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("decoding `claude {}` output", args.join(" ")))
}

fn claude_output(args: &[&str]) -> Result<Output> {
    let output = Command::new("claude")
        .args(args)
        .output()
        .with_context(|| format!("running `claude {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`claude {}` failed with status {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn run_claude(args: &[&str]) -> Result<()> {
    let status = Command::new("claude")
        .args(args)
        .status()
        .with_context(|| format!("running `claude {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`claude {}` failed with status {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_marketplace_is_complete_and_byte_exact() {
        let temp = tempfile::tempdir().unwrap();
        install_marketplace_at(temp.path()).unwrap();
        verify_marketplace_at(temp.path()).unwrap();

        let marketplace: Value = serde_json::from_slice(
            &fs::read(temp.path().join(".claude-plugin/marketplace.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(marketplace["name"], MARKETPLACE);
        assert_eq!(marketplace["plugins"][0]["name"], PLUGIN);
        assert_eq!(marketplace["plugins"][1]["name"], ST3_PLUGIN);

        let mcp: Value = serde_json::from_slice(
            &fs::read(temp.path().join("plugins/st2-channel/.mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(mcp["mcpServers"]["st2"]["command"], "st2");
        assert_eq!(
            mcp["mcpServers"]["st2"]["args"],
            json!(["driver", "claude-mcp"])
        );
        let plugin: Value = serde_json::from_slice(
            &fs::read(
                temp.path()
                    .join("plugins/st2-channel/.claude-plugin/plugin.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"));

        let st3_mcp: Value = serde_json::from_slice(
            &fs::read(temp.path().join("plugins/st3-channel/.mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(st3_mcp["mcpServers"]["st3"]["command"], "st3");
        assert_eq!(
            st3_mcp["mcpServers"]["st3"]["args"],
            json!(["driver", "claude-mcp"])
        );
        let st3_plugin: Value = serde_json::from_slice(
            &fs::read(
                temp.path()
                    .join("plugins/st3-channel/.claude-plugin/plugin.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(st3_plugin["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn policy_fragment_approves_only_the_stable_plugin_identities() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(POLICY_FILE);
        install_policy_at(&path).unwrap();
        assert!(policy_is_current_at(&path));
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value, policy_value());
        remove_policy_at(&path).unwrap();
        remove_policy_at(&path).unwrap();
    }

    #[test]
    fn st3_has_an_independent_marketplace_and_policy_identity() {
        let temp = tempfile::tempdir().unwrap();
        install_st3_marketplace_at(temp.path()).unwrap();
        verify_st3_marketplace_at(temp.path()).unwrap();

        let marketplace: Value = serde_json::from_slice(
            &fs::read(temp.path().join(".claude-plugin/marketplace.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(marketplace["name"], ST3_MARKETPLACE);
        assert_eq!(marketplace["plugins"][0]["name"], ST3_PLUGIN);
        assert_eq!(marketplace["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(ST3_CHANNEL, "plugin:st3-channel@st3");

        let policy = temp.path().join(ST3_POLICY_FILE);
        install_st3_policy_at(&policy).unwrap();
        assert!(st3_policy_is_current_at(&policy));
        let value: Value = serde_json::from_slice(&fs::read(policy).unwrap()).unwrap();
        assert_eq!(value, st3_policy_value());
        assert_eq!(value["allowedChannelPlugins"][0]["marketplace"], "st3");
    }

    #[test]
    fn recursive_json_lookup_finds_plugin_ids_without_a_cli_schema_dependency() {
        let value = json!([{"id": "st2-channel@st2", "nested": {"ready": true}}]);
        assert!(json_contains(&value, "st2-channel@st2"));
        assert!(!json_contains(&value, "other@st2"));
    }

    #[test]
    fn marketplace_registration_must_point_to_the_installed_asset_root() {
        let root = Path::new("/data/st2/claude-channel/marketplace");
        assert!(marketplace_entry_matches(
            &json!({"name":"st2","source":"directory","path":root}),
            root,
        ));
        assert!(!marketplace_entry_matches(
            &json!({"name":"st2","source":"github","repo":"other/st2"}),
            root,
        ));
    }
}
