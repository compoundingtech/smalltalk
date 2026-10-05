//! Pairing and private profile storage shared with `st devices complete`.
use anyhow::Result;
pub use st3_client::device::{Profile, profile_path, read_pairing_code as read_code};
use std::path::Path;

pub async fn pair(
    path: &Path,
    endpoint: &str,
    pairing_id: &str,
    code: &str,
    allow_public_http: bool,
) -> Result<String> {
    let key = st3_client::device::SigningKey::generate(st3_client::device::KeyAlgorithm::P256)?;
    let device = st3_client::device::complete_with_http_policy(
        path,
        endpoint,
        pairing_id,
        code,
        key,
        allow_public_http,
    )
    .await?;
    Ok(device.session.person_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use st3_client::{
        PairedSession,
        device::{Device, validate_endpoint},
    };
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn profile() -> Profile {
        Profile {
            devices: vec![Device {
                endpoint: "https://member.example".into(),
                allow_public_http: false,
                signing_key: None,
                session: PairedSession {
                    kind: "paired-session".into(),
                    device_id: "device/demo".into(),
                    person_id: "person/avery".into(),
                    session_actor: "person/avery/session/demo".into(),
                    credential: "test-only-secret-0000000000000000000000".into(),
                    scopes: vec!["read.projections".into()],
                    expires_at: "2026-10-30T12:00:00Z".into(),
                    device_key_chain: Vec::new(),
                    device_key_proofs: Vec::new(),
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
            !format!("{:?}", loaded.clients().unwrap())
                .contains(&profile.devices[0].session.credential)
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
