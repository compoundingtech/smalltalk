//! A device profile is client state, never a graph replica or a peer configuration.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use st3_client::{Client, PairedSession, PairingComplete};
use std::{
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
pub struct Device {
    pub endpoint: String,
    pub session: PairedSession,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Profile {
    pub devices: Vec<Device>,
}

impl Profile {
    pub fn person(&self) -> Result<&str> {
        let person = &self
            .devices
            .first()
            .context("No paired member; run stui pair URL PAIRING_ID")?
            .session
            .person_id;
        ensure!(
            person.starts_with("person/") && person.len() > 7,
            "Invalid paired person"
        );
        ensure!(
            self.devices
                .iter()
                .all(|device| &device.session.person_id == person),
            "All paired members must delegate the same person"
        );
        Ok(person)
    }

    pub fn clients(&self) -> Vec<Client> {
        self.devices
            .iter()
            .map(|device| Client::fabric_loopback(&device.endpoint, &device.session.credential))
            .collect()
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
            "The stui device profile must be a private regular file (chmod 600)"
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "The stui device profile must belong to this user"
        );
        ensure!(
            metadata.len() <= 1024 * 1024,
            "The stui device profile is too large"
        );
        let mut profile: Self =
            serde_json::from_reader(file).context("Read stui device profile")?;
        profile.person()?;
        for device in &mut profile.devices {
            device.endpoint = validate_endpoint(&device.endpoint)?;
            ensure!(
                device.session.credential.len() >= 32,
                "Invalid device credential"
            );
        }
        Ok(Some(profile))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.person()?;
        let parent = path
            .parent()
            .context("Device profile needs a parent directory")?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec(self)?)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

pub fn profile_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("Set XDG_CONFIG_HOME or HOME for the stui device profile")?;
    ensure!(base.is_absolute(), "The config directory must be absolute");
    Ok(base.join("st3/stui-devices.json"))
}

fn validate_endpoint(endpoint: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(endpoint).context("Member gateway must be an HTTP or HTTPS URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "Use the member gateway's HTTP or HTTPS origin, without credentials or a path"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Complete the member's single-use challenge without putting its code in shell history.
pub async fn pair(path: &Path, endpoint: &str, pairing_id: &str, code: &str) -> Result<String> {
    let endpoint = validate_endpoint(endpoint)?;
    let mut profile = Profile::load(path)?.unwrap_or_default();
    let session = Client::fabric_pairing(&endpoint)
        .pairing_complete(
            pairing_id,
            &PairingComplete {
                api_version: st3_client::API_VERSION.into(),
                code: code.trim().into(),
                device_public_key: format!("stui-device-{}", uuid::Uuid::now_v7()),
                key_storage: None,
            },
        )
        .await
        .context("Complete device pairing")?
        .value;
    if let Some(existing) = profile.devices.first() {
        ensure!(
            existing.session.person_id == session.person_id,
            "This member delegates a different person; use a separate XDG_CONFIG_HOME"
        );
    }
    let person = session.person_id.clone();
    profile.devices.retain(|device| device.endpoint != endpoint);
    profile.devices.push(Device { endpoint, session });
    profile.save(path)?;
    Ok(person)
}

pub fn read_code() -> Result<String> {
    if !io::stdin().is_terminal() {
        let mut code = String::new();
        io::stdin().read_line(&mut code)?;
        ensure!(
            !code.trim().is_empty(),
            "A pairing code is required on stdin"
        );
        return Ok(code);
    }
    use crossterm::{
        event::{self, Event, KeyCode, KeyModifiers},
        terminal,
    };
    eprint!("Pairing code: ");
    io::stderr().flush()?;
    terminal::enable_raw_mode()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
            eprintln!();
        }
    }
    let _restore = Restore;
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, stopping.clone())?;
    }
    let mut code = String::new();
    loop {
        ensure!(
            !stopping.load(std::sync::atomic::Ordering::Relaxed),
            "Pairing cancelled"
        );
        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Enter => break,
                KeyCode::Esc => anyhow::bail!("Pairing cancelled"),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    anyhow::bail!("Pairing cancelled")
                }
                KeyCode::Char(character) => code.push(character),
                KeyCode::Backspace => {
                    code.pop();
                }
                _ => {}
            }
        }
    }
    ensure!(!code.trim().is_empty(), "A pairing code is required");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn profile() -> Profile {
        Profile {
            devices: vec![Device {
                endpoint: "https://member.example".into(),
                session: PairedSession {
                    kind: "paired-session".into(),
                    device_id: "device/demo".into(),
                    person_id: "person/avery".into(),
                    session_actor: "person/avery/session/demo".into(),
                    credential: "test-only-secret-0000000000000000000000".into(),
                    scopes: vec!["read.projections".into()],
                    expires_at: "2026-10-30T12:00:00Z".into(),
                    device_key_chain: Vec::new(),
                },
            }],
        }
    }

    #[test]
    fn grants_are_private_and_never_printed_in_client_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config/devices.json");
        let profile = profile();
        profile.save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let loaded = Profile::load(&path).unwrap().unwrap();
        assert_eq!(loaded.person().unwrap(), "person/avery");
        assert!(
            !format!("{:?}", loaded.clients()).contains(&profile.devices[0].session.credential)
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Profile::load(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.path().join("link.json");
        symlink(path, &link).unwrap();
        assert!(Profile::load(&link).is_err());
    }

    #[test]
    fn profiles_cannot_mix_people_or_embed_secrets_in_gateway_urls() {
        let mut profile = profile();
        let mut other = profile.devices[0].clone();
        other.session.person_id = "person/blair".into();
        profile.devices.push(other);
        assert!(profile.person().is_err());
        for invalid in [
            "file:///tmp/member",
            "https://user:secret@member.example",
            "https://member.example?secret=bad",
            "https://member.example/path",
            "https://member.example#fragment",
        ] {
            assert!(validate_endpoint(invalid).is_err(), "{invalid}");
        }
    }
}
