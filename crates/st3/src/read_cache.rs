//! First-run page-cache sizing. Page-cache targets exclude schema, statements and
//! allocator overhead; reserve most installed RAM for those, the OS and seats.

const SMALL_MACHINE_BYTES: u64 = 1536 * 1024 * 1024;
const CACHE_ENV: &str = "SMALLCLAIMS_READ_CACHE_KIB";

/// Keep the compiled default unless a small machine needs a lower page-cache target.
/// At most a quarter of installed RAM goes to the retained readers' page caches.
fn sized_cache_kib(memory_bytes: u64, default_kib: usize, readers: usize) -> Option<usize> {
    if memory_bytes == 0 || memory_bytes > SMALL_MACHINE_BYTES || readers == 0 {
        return None;
    }
    let budget_kib = memory_bytes / 1024 / 4 / readers as u64;
    let target = usize::try_from(budget_kib).unwrap_or(usize::MAX).max(1);
    (target < default_kib).then_some(target)
}

/// An operator's valid override takes precedence and survives service installation.
pub(crate) fn override_kib() -> Option<usize> {
    if let Some(value) = configured_override_kib(std::env::var(CACHE_ENV).ok().as_deref()) {
        return Some(value);
    }
    sized_cache_kib(
        available_memory_bytes()?,
        smallclaims::sqlite::READ_CACHE_KIB,
        smallclaims::sqlite::MAX_IDLE_READ_CONNECTIONS,
    )
}

fn configured_override_kib(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=i32::MAX as usize).contains(value))
}

// Containers can expose host MemTotal despite a smaller enforced allocation.
// Include ancestor limits, but do not substitute transient free/available memory.
fn available_memory_bytes() -> Option<u64> {
    let installed = installed_memory_bytes()?;
    let limit = crate::memory::service_memory()
        .ok()
        .flatten()
        .and_then(|groups| groups.into_iter().filter_map(|group| group.max_bytes).min());
    Some(limit.map_or(installed, |limit| installed.min(limit)))
}

#[cfg(target_os = "linux")]
fn installed_memory_bytes() -> Option<u64> {
    meminfo_bytes(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(any(target_os = "linux", test))]
fn meminfo_bytes(meminfo: &str) -> Option<u64> {
    let mut fields = meminfo
        .lines()
        .find(|line| line.starts_with("MemTotal:"))?
        .split_whitespace();
    if fields.next()? != "MemTotal:" {
        return None;
    }
    let kib: u64 = fields.next()?.parse().ok()?;
    if fields.next()? != "kB" || fields.next().is_some() {
        return None;
    }
    kib.checked_mul(1024).filter(|bytes| *bytes > 0)
}

#[cfg(target_os = "macos")]
fn installed_memory_bytes() -> Option<u64> {
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn installed_memory_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_a_valid_operator_override_and_rejects_values_sqlite_cannot_use() {
        assert_eq!(configured_override_kib(Some("4096")), Some(4096));
        for value in [
            None,
            Some("0"),
            Some("-1"),
            Some("invalid"),
            Some("2147483648"),
        ] {
            assert_eq!(configured_override_kib(value), None);
        }
    }

    #[test]
    fn sizes_small_machines_against_the_compiled_cache_and_retained_readers() {
        let gib = 1024 * 1024 * 1024;
        // Both the old and proposed defaults: no dependence on which PR landed.
        assert_eq!(sized_cache_kib(gib, 2048, 128), None);
        assert_eq!(sized_cache_kib(gib, 8192, 128), Some(2048));
        assert_eq!(sized_cache_kib(gib / 2, 2048, 128), Some(1024));
        assert_eq!(sized_cache_kib(gib, 8192, 64), Some(4096));
        assert_eq!(sized_cache_kib(3 * gib, 8192, 128), None);
        assert_eq!(sized_cache_kib(0, 8192, 128), None);
        assert_eq!(sized_cache_kib(gib, 8192, 0), None);
        assert_eq!(sized_cache_kib(1, 8192, 128), Some(1));
        let readers = smallclaims::sqlite::MAX_IDLE_READ_CONNECTIONS;
        let default = smallclaims::sqlite::READ_CACHE_KIB;
        let cap = gib as usize / 1024 / 4 / readers;
        let expected = (cap < default).then_some(cap);
        assert_eq!(sized_cache_kib(gib, default, readers), expected);
    }

    #[test]
    fn parses_installed_memory_without_using_free_memory_or_accepting_bad_units() {
        assert_eq!(
            meminfo_bytes("MemFree: 5 kB\nMemTotal: 1048576 kB\n"),
            Some(1 << 30)
        );
        for input in [
            "MemFree: 1048576 kB",
            "MemTotal: x kB",
            "MemTotal: 0 kB",
            "MemTotal: 1024 MB",
            "MemTotal: 18446744073709551615 kB",
            "MemTotal: 1 kB extra",
        ] {
            assert_eq!(meminfo_bytes(input), None, "{input}");
        }
    }
}
