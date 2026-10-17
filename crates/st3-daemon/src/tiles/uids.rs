//! Tile user IDs: the launcher allocates each from the registry's range and records it in
//! `STATE/uids.toml`, so no tile is a hand-made account.
//!
//! This follows Nomad's dynamic workload users (a pool of IDs that belong to no account, the
//! acquired one kept in the task's state) and systemd's `DynamicUser=`. One thing differs: a
//! Nomad task gives its ID back when it stops, but a tile's state outlives every launch and is
//! owned by its ID. So a tile keeps its ID while its state, or a process of its, exists.
//! `st tiles remove` ends the tile, removes its state and only then gives the ID back, and the
//! launcher never hands out an ID that still owns a file under the state root.
//!
//! Root runs all of this, single-threaded, before any runtime starts: the account enumeration
//! below uses libc's non-reentrant `getpwent` and `getgrent`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

use crate::tile_registry::{Registry, UidRange};

/// Holds the state root's lock: one launcher allocates or releases at a time.
struct Lock(#[allow(dead_code)] fs::File);

fn lock(registry: &Registry) -> Result<Lock> {
    let path = registry.state_root.join("uids.lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    if file.metadata()?.uid() != 0 {
        bail!("{} belongs to someone other than root", path.display());
    }
    // SAFETY: flock takes the open descriptor and an integer.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error()).context("lock the tiles' user IDs");
    }
    Ok(Lock(file))
}

/// The registry with the user IDs on disk now, read under the lock.
fn reload(registry: &Registry) -> Result<Registry> {
    let path = registry.uids_file();
    let text = match fs::read_to_string(&path) {
        Ok(text) => {
            crate::tile_registry::root_controlled(&path)?;
            text
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    registry
        .clone()
        .with_uids(&text)
        .with_context(|| format!("parse {}", path.display()))
}

/// Write `uids` as the record, replacing the old one in a single rename.
fn write(registry: &Registry, uids: &BTreeMap<String, u32>) -> Result<()> {
    let path = registry.uids_file();
    let temporary = registry.state_root.join(".uids.toml.new");
    let mut text = String::from(
        "# Each tile's user ID, allocated by `st tiles`. Root's launcher writes this file.\n",
    );
    for (name, uid) in uids {
        text.push_str(&format!("{name} = {uid}\n"));
    }
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    // The daemon and the gateway read this file, so its mode is set outright: root's umask
    // may be 077.
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    fs::File::open(&registry.state_root)?.sync_all()?;
    Ok(())
}

/// The tile's user ID: the one the launcher gave it, or a new one from the range. Root's state
/// root must exist. Refuses an ID that an account or group has taken since.
pub fn allocate(registry: &mut Registry, name: &str) -> Result<u32> {
    registry.tile(name)?;
    let _lock = lock(registry)?;
    let mut current = reload(registry)?;
    if let Some(uid) = current.uid(name) {
        check_unclaimed(name, uid)?;
        *registry = current;
        return Ok(uid);
    }
    check_range(current.uids)?;
    let mut taken: BTreeSet<u32> = current.allocated().values().copied().collect();
    taken.extend(owners_under(&current.state_root)?);
    let uid = (current.uids.first..=current.uids.last)
        .find(|uid| !taken.contains(uid) && account(*uid).is_none() && group(*uid).is_none())
        .with_context(|| {
            format!(
                "every user ID in {}-{} is taken; widen the registry's uids",
                current.uids.first, current.uids.last
            )
        })?;
    let mut uids = current.allocated().clone();
    uids.insert(name.to_owned(), uid);
    write(&current, &uids)?;
    current = reload(&current)?;
    *registry = current;
    Ok(uid)
}

/// Give a tile's user ID back once `remove` has removed everything it owns. Refused while a
/// process runs as it; afterwards the tile's state directory must be gone.
pub fn release(
    registry: &mut Registry,
    name: &str,
    remove: impl FnOnce(&Registry) -> Result<()>,
) -> Result<()> {
    let _lock = lock(registry)?;
    let current = reload(registry)?;
    let Some(uid) = current.uid(name) else {
        remove(&current)?;
        *registry = current;
        return Ok(());
    };
    let running = processes_of(uid)?;
    if !running.is_empty() {
        bail!("tile {name}'s user ID {uid} still runs processes {running:?}; stop the tile first");
    }
    remove(&current)?;
    let state = current.tile_state(name);
    if fs::symlink_metadata(&state).is_ok() {
        bail!("{} remains; its user ID stays allocated", state.display());
    }
    let mut uids = current.allocated().clone();
    uids.remove(name);
    write(&current, &uids)?;
    *registry = reload(&current)?;
    Ok(())
}

/// No account or group may have the tile's user ID: the tile would be that account.
pub fn check_unclaimed(name: &str, uid: u32) -> Result<()> {
    if let Some(account) = account(uid) {
        bail!(
            "tile {name}'s user ID {uid} now belongs to the account {account}; a tile is never \
             an account. Remove the account, or remove the tile with `st tiles remove`"
        );
    }
    if let Some(group) = group(uid) {
        bail!(
            "tile {name}'s user ID {uid} is also the group {group}'s; a tile's group is its own. \
             Remove the group, or remove the tile with `st tiles remove`"
        );
    }
    Ok(())
}

/// The range must not hold an account, a group or a subordinate range: a tile would be them.
fn check_range(range: UidRange) -> Result<()> {
    if let Some((name, uid)) = accounts().into_iter().find(|(_, uid)| range.contains(*uid)) {
        bail!(
            "the tiles' range {}-{} holds the account {name} ({uid}); choose a range no account \
             uses",
            range.first,
            range.last
        );
    }
    if let Some((name, gid)) = groups().into_iter().find(|(_, gid)| range.contains(*gid)) {
        bail!(
            "the tiles' range {}-{} holds the group {name} ({gid}); choose a range no group uses",
            range.first,
            range.last
        );
    }
    for file in ["/etc/subuid", "/etc/subgid"] {
        let text = match fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {file}")),
        };
        for (owner, first, count) in subordinate_ranges(&text) {
            let last = first.saturating_add(count.saturating_sub(1));
            if count > 0 && range.overlaps(first, last) {
                bail!(
                    "the tiles' range {}-{} overlaps {owner}'s subordinate IDs {first}-{last} in \
                     {file}",
                    range.first,
                    range.last
                );
            }
        }
    }
    Ok(())
}

/// `OWNER:FIRST:COUNT` lines; anything else is skipped, as the shadow tools do.
fn subordinate_ranges(text: &str) -> Vec<(String, u32, u32)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.trim().split(':');
            let owner = fields.next()?;
            let first = fields.next()?.parse().ok()?;
            let count = fields.next()?.parse().ok()?;
            Some((owner.to_owned(), first, count))
        })
        .collect()
}

/// User IDs that own an entry of the state root, or a tile's `home` or `work` in it, allocated
/// or not: a removed record never frees an ID that still owns a tile's directories. A tile
/// writes nowhere else on the host, so these are the places its ID can own files.
fn owners_under(state_root: &Path) -> Result<BTreeSet<u32>> {
    let mut owners = BTreeSet::new();
    let entries = match fs::read_dir(state_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(owners),
        Err(error) => return Err(error).with_context(|| format!("list {}", state_root.display())),
    };
    for entry in entries {
        let path = entry?.path();
        for path in [path.clone(), path.join("home"), path.join("work")] {
            if let Ok(metadata) = fs::symlink_metadata(&path) {
                owners.insert(metadata.uid());
            }
        }
    }
    owners.remove(&0);
    Ok(owners)
}

/// Processes running as `uid` anywhere on the host.
fn processes_of(uid: u32) -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").context("list /proc")?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let real = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .map(|ids| ids.split_whitespace().any(|id| id == uid.to_string()));
        if real == Some(true) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

fn c_string(pointer: *const libc::c_char) -> String {
    // SAFETY: libc returned this NUL-terminated string in an entry it owns until the next call.
    unsafe { std::ffi::CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

/// The account with user ID `uid`, through the host's name services.
fn account(uid: u32) -> Option<String> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut result = std::ptr::null_mut();
    // SAFETY: the buffer and entry outlive the call, and the result is read only on success.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    (code == 0 && !result.is_null()).then(|| c_string(entry.pw_name))
}

/// The group with group ID `gid`, through the host's name services.
fn group(gid: u32) -> Option<String> {
    let mut entry: libc::group = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 64 * 1024];
    let mut result = std::ptr::null_mut();
    // SAFETY: as for getpwuid_r.
    let code = unsafe {
        libc::getgrgid_r(
            gid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    (code == 0 && !result.is_null()).then(|| c_string(entry.gr_name))
}

/// Every account the name services list.
fn accounts() -> Vec<(String, u32)> {
    let mut all = Vec::new();
    // SAFETY: single-threaded, as the module says; each entry is copied before the next call.
    unsafe {
        libc::setpwent();
        loop {
            let entry = libc::getpwent();
            if entry.is_null() {
                break;
            }
            all.push((c_string((*entry).pw_name), (*entry).pw_uid));
        }
        libc::endpwent();
    }
    all
}

/// Every group the name services list.
fn groups() -> Vec<(String, u32)> {
    let mut all = Vec::new();
    // SAFETY: as for `accounts`.
    unsafe {
        libc::setgrent();
        loop {
            let entry = libc::getgrent();
            if entry.is_null() {
                break;
            }
            all.push((c_string((*entry).gr_name), (*entry).gr_gid));
        }
        libc::endgrent();
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subordinate_ranges_parse_as_the_shadow_tools_write_them() {
        let ranges = subordinate_ranges("ada:100000:65536\n# comment\nroot:1000000:1000000000\n");
        assert_eq!(
            ranges,
            vec![
                ("ada".to_owned(), 100_000, 65_536),
                ("root".to_owned(), 1_000_000, 1_000_000_000)
            ]
        );
    }

    #[test]
    fn an_id_an_account_has_is_never_a_tiles() {
        // SAFETY: getuid has no preconditions.
        let me = unsafe { libc::getuid() };
        if account(me).is_some() {
            assert!(check_unclaimed("atlas", me).is_err());
        }
        // The whole-range check enumerates accounts with getpwent, which is not safe beside
        // the test harness's threads; the root tests cover it.
    }
}
