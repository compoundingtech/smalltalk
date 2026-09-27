//! Direct OMP actors: live-catalog identity directories owned by one stable PTY session.
//!
//! A direct OMP launch has no declaration. Its entrypoint derives
//! `ST_AGENT=<host>.direct.omp.<encoded-pty-id>` from `PTY_SESSION`, and writers such as the
//! decision store lazily create `agents/<host>/direct.omp.<encoded-pty-id>/` under that identity.
//! The PTY segment codec is reversible and strict (the dotfiles identity spec owns it as
//! `agent-identity encode-pty`/`decode-pty`), so the directory name alone yields the exact PTY
//! session ID — the one fact that joins the directory to the PTY registry. Nothing here reads
//! names, cwd, process ancestry, or mtimes.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// Identity prefix of a direct OMP actor inside its host directory.
pub const DIRECT_OMP_PREFIX: &str = "direct.omp.";

/// PTY's generated session-ID alphabet: digits and lowercase letters without `0 1 i l o`.
const GENERATED_ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
const GENERATED_LEN: usize = 8;
/// Reserved segment namespace for every PTY ID that is not a generated one.
const ENCODED_PREFIX: &str = "x-";

/// One direct OMP actor directory in the live catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectActor {
    pub host: String,
    /// `direct.omp.<encoded-pty-id>`, the directory name under `agents/<host>/`.
    pub identity: String,
    /// The exact PTY session ID the identity decodes to.
    pub pty_id: String,
    pub dir: PathBuf,
}

impl DirectActor {
    /// `<host>.<identity>`, byte-identical to the actor's `ST_AGENT`.
    pub fn id(&self) -> String {
        format!("{}.{}", self.host, self.identity)
    }
}

fn is_generated_pty_id(id: &str) -> bool {
    id.len() == GENERATED_LEN && id.bytes().all(|byte| GENERATED_ALPHABET.contains(&byte))
}

fn is_valid_pty_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.len() <= 255
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The identity segment for a PTY ID; `None` for an ID PTY would not accept.
pub fn encode_pty_id(id: &str) -> Option<String> {
    if !is_valid_pty_id(id) {
        return None;
    }
    if is_generated_pty_id(id) {
        return Some(id.to_owned());
    }
    let mut encoded = String::with_capacity(ENCODED_PREFIX.len() + id.len() * 2);
    encoded.push_str(ENCODED_PREFIX);
    for byte in id.bytes() {
        encoded.push_str(&format!("{byte:02x}"));
    }
    Some(encoded)
}

/// The exact PTY ID an identity segment encodes. Strict: only the canonical encoding of a valid
/// ID decodes, so two segments can never name the same PTY session.
pub fn decode_pty_segment(segment: &str) -> Option<String> {
    if is_generated_pty_id(segment) {
        return Some(segment.to_owned());
    }
    let hex = segment.strip_prefix(ENCODED_PREFIX)?;
    if hex.is_empty()
        || !hex.len().is_multiple_of(2)
        || !hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    let decoded = String::from_utf8(bytes).ok()?;
    (encode_pty_id(&decoded)? == segment).then_some(decoded)
}

/// The exact PTY ID of a `direct.omp.<segment>` identity; `None` for every other identity.
pub fn identity_pty_id(identity: &str) -> Option<String> {
    decode_pty_segment(identity.strip_prefix(DIRECT_OMP_PREFIX)?)
}

/// Every direct OMP actor directory under `<catalog>/agents`, optionally narrowed to one host.
///
/// A directory is a direct actor only when its name decodes exactly and no declaration — parsed or
/// failed — lives under it: a declared identity is a declared actor whatever its name looks like.
/// The walk reads two directory levels and never descends into an actor's contents.
pub fn discover(
    catalog: &Path,
    found: &crate::Discovered,
    host: Option<&str>,
) -> Result<Vec<DirectActor>> {
    let agents = catalog.join("agents");
    let Some(hosts) = read_real_dir_optional(&agents)? else {
        return Ok(Vec::new());
    };
    let declared = declared_identity_dirs(&agents, found);
    let mut actors = Vec::new();
    for host_entry in hosts {
        let Some(host_name) = host_entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if host.is_some_and(|host| host != host_name) || !is_real_dir(&host_entry.path())? {
            continue;
        }
        for entry in sorted_entries(&host_entry.path())? {
            let Some(identity) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(pty_id) = identity_pty_id(&identity) else {
                continue;
            };
            let dir = entry.path();
            if declared.contains(&dir) || !is_real_dir(&dir)? {
                continue;
            }
            actors.push(DirectActor {
                host: host_name.clone(),
                identity,
                pty_id,
                dir,
            });
        }
    }
    Ok(actors)
}

/// The identity directories of `found`'s declarations: `agents/<host>/<identity>` for every spec,
/// declaration, or discovery error whose path sits beneath one.
fn declared_identity_dirs(agents: &Path, found: &crate::Discovered) -> BTreeSet<PathBuf> {
    found
        .specs
        .iter()
        .map(|spec| spec.path.as_path())
        .chain(
            found
                .declarations
                .iter()
                .map(|declaration| declaration.path.as_path()),
        )
        .chain(found.errors.iter().map(|error| error.path.as_path()))
        .filter_map(|path| {
            let mut components = path.strip_prefix(agents).ok()?.components();
            let host = components.next()?;
            let identity = components.next()?;
            Some(agents.join(host).join(identity))
        })
        .collect()
}

fn is_real_dir(path: &Path) -> Result<bool> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    Ok(metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn read_real_dir_optional(path: &Path) -> Result<Option<Vec<fs::DirEntry>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(sorted_entries(path)?))
        }
        Ok(_) => anyhow::bail!(
            "catalog agents path is not a real directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read directory {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read directory entries {}", path.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_pty_id_is_its_own_segment() {
        assert_eq!(encode_pty_id("e2jcd9pf").as_deref(), Some("e2jcd9pf"));
        assert_eq!(decode_pty_segment("e2jcd9pf").as_deref(), Some("e2jcd9pf"));
    }

    #[test]
    fn every_other_valid_id_round_trips_through_the_hex_namespace() {
        for id in [
            "web",
            "Dev3.Main_1",
            "e2jcd9p",
            "e2jcd9pfx",
            "abcdefgi",
            "-",
            "a..b",
        ] {
            let segment = encode_pty_id(id).unwrap();
            assert!(segment.starts_with("x-"), "{id} -> {segment}");
            assert_eq!(
                decode_pty_segment(&segment).as_deref(),
                Some(id),
                "{segment}"
            );
        }
        assert_eq!(encode_pty_id("web").as_deref(), Some("x-776562"));
    }

    #[test]
    fn non_canonical_and_invalid_segments_never_decode() {
        let generated_as_hex = format!(
            "x-{}",
            "e2jcd9pf"
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        for segment in [
            "",
            "x-",
            "x-7",
            "x-7g",
            // Uppercase hex is not the canonical spelling.
            "x-776F62",
            // The hex spelling of a generated ID is not its canonical segment.
            generated_as_hex.as_str(),
            // `.`, `..`, and `a/b` are not PTY IDs.
            "x-2e",
            "x-2e2e",
            "x-612f62",
            "E2JCD9PF",
            "e2jcd9p",
        ] {
            assert_eq!(decode_pty_segment(segment), None, "{segment:?}");
        }
    }

    #[test]
    fn identity_pty_id_requires_the_direct_omp_prefix() {
        assert_eq!(
            identity_pty_id("direct.omp.e2jcd9pf").as_deref(),
            Some("e2jcd9pf")
        );
        assert_eq!(identity_pty_id("direct.codex.e2jcd9pf"), None);
        assert_eq!(identity_pty_id("e2jcd9pf"), None);
        assert_eq!(identity_pty_id("direct.omp.not-canonical"), None);
    }

    fn write(root: &Path, relative: &str, body: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn discovery_finds_undeclared_direct_directories_only() {
        let temporary = tempfile::tempdir().unwrap();
        let catalog = temporary.path().canonicalize().unwrap();
        write(
            &catalog,
            "agents/h/direct.omp.e2jcd9pf/resources/decisions/1-a.md",
            "x",
        );
        write(
            &catalog,
            "agents/h/direct.omp.x-776562/resources/tmp/note",
            "x",
        );
        write(
            &catalog,
            "agents/other/direct.omp.2ahzpbs3/resources/tmp/note",
            "x",
        );
        // A declaration makes the directory a declared actor, whatever its name.
        write(
            &catalog,
            "agents/h/direct.omp.238hjuj3/agent.kdl",
            "agent \"direct.omp.238hjuj3\" { host \"h\"; command \"true\" }\n",
        );
        // Not a canonical segment, not a direct actor, and not a directory.
        fs::create_dir_all(catalog.join("agents/h/direct.omp.Bad")).unwrap();
        fs::create_dir_all(catalog.join("agents/h/lead")).unwrap();
        write(&catalog, "agents/h/direct.omp.2bk3wt7v", "a file");
        std::os::unix::fs::symlink(
            catalog.join("agents/h/direct.omp.e2jcd9pf"),
            catalog.join("agents/h/direct.omp.3bk3wt7v"),
        )
        .unwrap();

        let found = crate::discover_strict(&catalog);
        let all = discover(&catalog, &found, None).unwrap();
        let ids = all.iter().map(DirectActor::id).collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "h.direct.omp.e2jcd9pf",
                "h.direct.omp.x-776562",
                "other.direct.omp.2ahzpbs3"
            ]
        );
        assert_eq!(all[1].pty_id, "web");
        assert_eq!(all[0].dir, catalog.join("agents/h/direct.omp.e2jcd9pf"));

        let local = discover(&catalog, &found, Some("h")).unwrap();
        assert_eq!(local.len(), 2);
        assert!(local.iter().all(|actor| actor.host == "h"));
    }

    #[test]
    fn a_catalog_without_agents_has_no_direct_actors() {
        let temporary = tempfile::tempdir().unwrap();
        let found = crate::discover_strict(temporary.path());
        assert!(discover(temporary.path(), &found, None).unwrap().is_empty());
    }
}
