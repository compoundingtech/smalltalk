//! Kernel process identity for native Unix caller and bootstrap ancestry fences.
//! Darwin field definitions: https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc_info.h
struct Identity {
    parent: u32,
    birth: u64,
}

pub(super) fn birth(pid: u32) -> Option<u64> {
    identity(pid).map(|identity| identity.birth)
}

pub(super) fn is_descendant(mut pid: u32, root: u32) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    while pid > 1 && seen.len() < 64 && seen.insert(pid) {
        let Some(identity) = identity(pid) else {
            return false;
        };
        if pid == root {
            return true;
        }
        pid = identity.parent;
    }
    false
}

#[cfg(target_os = "linux")]
fn identity(pid: u32) -> Option<Identity> {
    parse_linux_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(target_os = "linux")]
fn parse_linux_stat(stat: &str) -> Option<Identity> {
    let (_, tail) = stat.rsplit_once(") ")?;
    let mut fields = tail.split_whitespace();
    if matches!(fields.next()?, "Z" | "X" | "x") {
        return None;
    }
    let parent = fields.next()?.parse().ok()?;
    let birth = fields.nth(17)?.parse().ok()?;
    Some(Identity { parent, birth })
}

#[cfg(target_os = "macos")]
fn identity(pid: u32) -> Option<Identity> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: the kernel receives the matching libc ABI structure and its exact size;
    // the output is read only when every byte of that structure was returned.
    let written = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            i32::try_from(size).ok()?,
        )
    };
    if written != i32::try_from(size).ok()? {
        return None;
    }
    // SAFETY: the full structure was initialized by proc_pidinfo above.
    let info = unsafe { info.assume_init() };
    // PROC_FLAG_INEXIT is defined in Apple's proc_info.h (not exported by libc).
    const IN_EXIT: u32 = 4;
    if info.pbi_pid != pid
        || info.pbi_status == libc::SZOMB
        || info.pbi_flags & IN_EXIT != 0
        || info.pbi_start_tvusec >= 1_000_000
    {
        return None;
    }
    let birth = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)?;
    Some(Identity {
        parent: info.pbi_ppid,
        birth,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn identity(_pid: u32) -> Option<Identity> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn process_stat_identity_uses_birth_and_refuses_zombies_or_truncation() {
        let stat = "42 (fixture ) name) S 7 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1234 0";
        let id = parse_linux_stat(stat).unwrap();
        assert_eq!((id.parent, id.birth), (7, 1234));
        assert!(parse_linux_stat(&stat.replace(") S ", ") Z ")).is_none());
        assert!(parse_linux_stat("42 (fixture) S 7").is_none());
    }
}
