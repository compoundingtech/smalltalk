//! Notes pairing bootstrap trusts a root-issued PAM login, never ambient person
//! headers, user-service cgroups, mutable native identity, or an orphaned process.
#[cfg(target_os = "linux")]
pub(super) use linux::Peer;

#[cfg(not(target_os = "linux"))]
pub(super) struct Peer;
#[cfg(not(target_os = "linux"))]
impl Peer {
    pub(super) fn capture(_stream: &tokio::net::UnixStream) -> Option<Self> { None }
    pub(super) async fn admitted(self: std::sync::Arc<Self>) -> bool { false }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::Duration;
    use serde_json::Value;
    use tokio::io::AsyncReadExt as _;

    const BUS: &str = "/run/dbus/system_bus_socket";
    const MAX_OUTPUT: u64 = 262_144;

    pub(crate) struct Peer { pid: u32, uid: u32, lifetime: File }

    impl Peer {
        pub(crate) fn capture(stream: &tokio::net::UnixStream) -> Option<Self> {
            let credentials = stream.peer_cred().ok()?;
            // SAFETY: getuid takes no arguments and accesses no caller memory.
            if credentials.uid() != unsafe { libc::getuid() } { return None; }
            let pid = u32::try_from(credentials.pid()?).ok()?;
            let mut descriptor: libc::c_int = -1;
            let mut size = std::mem::size_of_val(&descriptor) as libc::socklen_t;
            // SAFETY: the socket is live, both output pointers refer to correctly
            // sized writable values, and success transfers one owned pidfd.
            let result = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET,
                libc::SO_PEERPIDFD, (&raw mut descriptor).cast(), &raw mut size) };
            if result != 0 { return None; }
            // SAFETY: successful SO_PEERPIDFD returns a new descriptor owned here.
            let lifetime = unsafe { File::from_raw_fd(descriptor) };
            if size as usize != std::mem::size_of_val(&descriptor) { return None; }
            Some(Self { pid, uid: credentials.uid(), lifetime })
        }

        pub(crate) async fn admitted(self: Arc<Self>) -> bool {
            tokio::time::timeout(Duration::from_secs(3), self.admit()).await.unwrap_or(false)
        }

        async fn admit(self: Arc<Self>) -> bool {
            let peer = self.clone();
            let Some((chain, executable)) = tokio::task::spawn_blocking(move || {
                if !live(&peer.lifetime) || super::super::ancestor(peer.pid).is_some() { return None; }
                let chain = chain(peer.pid)?;
                if chain.first()?.lifetime.metadata().ok()?.ino() != peer.lifetime.metadata().ok()?.ino() {
                    return None;
                }
                if !trusted_root_path(Path::new(BUS), false) { return None; }
                Some((chain, busctl()?))
            }).await.ok().flatten() else { return false; };
            let Some(owner) = bus_call(&executable, &[
                "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                "GetNameOwner", "s", "org.freedesktop.login1",
            ]).await else { return false; };
            if owner["type"] != "s" { return false; }
            let Some(issuer) = owner["data"][0].as_str().filter(|name| name.starts_with(':')) else { return false; };
            let Some(identity) = bus_call(&executable, &[
                "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                "GetConnectionUnixUser", "s", issuer,
            ]).await else { return false; };
            if identity["type"] != "u" || identity["data"][0].as_u64() != Some(0) { return false; }
            let Some(sessions) = bus_call(&executable, &[
                issuer, "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager", "ListSessions",
            ]).await else { return false; };
            if sessions["type"] != "a(susso)" { return false; }
            let Some(records) = sessions["data"][0].as_array() else { return false; };
            if records.len() > 256 { return false; }
            for record in records {
                if record[1].as_u64() != Some(u64::from(self.uid)) { continue; }
                let Some(path) = record[4].as_str().filter(|path| path.starts_with("/org/freedesktop/login1/session/")) else { continue; };
                let Some(properties) = bus_call(&executable, &[
                    issuer, path, "org.freedesktop.DBus.Properties",
                    "GetAll", "s", "org.freedesktop.login1.Session",
                ]).await else { return false; };
                let Some(leader) = chain.iter().find(|process| session_matches(&properties, self.uid,
                    process.pid, process.lifetime.metadata().map(|metadata| metadata.ino()).unwrap_or_default())) else { continue; };
                if !live(&leader.lifetime) { return false; }
                let peer = self.clone();
                return tokio::task::spawn_blocking(move || {
                    live(&peer.lifetime) && super::super::ancestor(peer.pid).is_none()
                        && chain.iter().all(Process::unchanged)
                }).await.unwrap_or(false);
            }
            false
        }
    }

    struct Process { pid: u32, parent: u32, start: u64, lifetime: File }
    impl Process {
        fn unchanged(&self) -> bool {
            live(&self.lifetime) && process_stat(self.pid) == Some((self.parent, self.start))
                && live(&self.lifetime)
        }
    }

    fn live(descriptor: &File) -> bool {
        let mut event = libc::pollfd { fd: descriptor.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: event is one initialized writable pollfd; timeout zero never waits.
        unsafe { libc::poll(&raw mut event, 1, 0) == 0 }
    }

    fn process_stat(pid: u32) -> Option<(u32, u64)> {
        let value = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let mut fields = value.rsplit_once(") ")?.1.split_whitespace();
        fields.next()?;
        let parent = fields.next()?.parse().ok()?;
        let start = fields.nth(17)?.parse().ok()?;
        Some((parent, start))
    }

    fn chain(mut pid: u32) -> Option<Vec<Process>> {
        let mut processes = Vec::new();
        while pid > 1 && processes.len() < 64 {
            if processes.iter().any(|process: &Process| process.pid == pid) { return None; }
            // SAFETY: pidfd_open has scalar arguments and returns a newly owned
            // descriptor or -1. It does not dereference caller-provided pointers.
            let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if descriptor < 0 { return None; }
            // SAFETY: the successful syscall transferred this descriptor to us.
            let lifetime = unsafe { File::from_raw_fd(descriptor as libc::c_int) };
            let (parent, start) = process_stat(pid)?;
            let process = Process { pid, parent, start, lifetime };
            if !process.unchanged() { return None; }
            processes.push(process);
            pid = parent;
        }
        (pid == 1).then_some(processes)
    }

    fn trusted_root_path(path: &Path, executable: bool) -> bool {
        let Ok(metadata) = std::fs::symlink_metadata(path) else { return false; };
        if metadata.uid() != 0 || (executable && (!metadata.is_file() || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0)) || (!executable && !metadata.file_type().is_socket()) { return false; }
        let mut parent = path.parent();
        while let Some(directory) = parent {
            let Ok(metadata) = std::fs::symlink_metadata(directory) else { return false; };
            // Sticky root-owned directories protect each root-owned child from
            // replacement (notably the Nix store); all children are checked.
            if metadata.uid() != 0 || !metadata.is_dir()
                || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0) { return false; }
            parent = directory.parent();
        }
        path.is_absolute()
    }

    fn busctl() -> Option<PathBuf> {
        for directory in std::env::split_paths(&std::env::var_os("PATH")?) {
            let Ok(path) = std::fs::canonicalize(directory.join("busctl")) else { continue; };
            if trusted_root_path(&path, true) { return Some(path); }
        }
        None
    }

    async fn bus_call(executable: &Path, arguments: &[&str]) -> Option<Value> {
        let mut child = tokio::process::Command::new(executable)
            .env_clear().args(["--system", "--address=unix:path=/run/dbus/system_bus_socket",
                "--no-pager", "--json=short", "--timeout=1", "call"])
            .args(arguments).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
            .kill_on_drop(true).spawn().ok()?;
        let mut bytes = Vec::new();
        child.stdout.take()?.take(MAX_OUTPUT + 1).read_to_end(&mut bytes).await.ok()?;
        if bytes.len() as u64 > MAX_OUTPUT || !child.wait().await.ok()?.success() { return None; }
        serde_json::from_slice(&bytes).ok()
    }

    fn session_matches(properties: &Value, uid: u32, leader: u32, lifetime: u64) -> bool {
        if properties["type"] != "a{sv}" { return false; }
        let fields = &properties["data"][0];
        fields["Service"]["type"] == "s" && fields["Service"]["data"] == "sshd"
            && fields["User"]["type"] == "(uo)" && fields["User"]["data"][0].as_u64() == Some(u64::from(uid))
            && fields["Leader"]["type"] == "u" && fields["Leader"]["data"].as_u64() == Some(u64::from(leader))
            && fields["LeaderPIDFDId"]["type"] == "t" && fields["LeaderPIDFDId"]["data"].as_u64() == Some(lifetime)
            && lifetime != 0 && fields["State"]["type"] == "s"
            && matches!(fields["State"]["data"].as_str(), Some("active" | "online"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn login_admission_requires_live_exact_issuer_lifetime_and_pam_service() {
            let admitted = serde_json::json!({"type":"a{sv}","data":[{
                "Service":{"type":"s","data":"sshd"}, "User":{"type":"(uo)","data":[1000,"/user/1000"]},
                "Leader":{"type":"u","data":42}, "LeaderPIDFDId":{"type":"t","data":1234},
                "State":{"type":"s","data":"active"}
            }]});
            assert!(session_matches(&admitted, 1000, 42, 1234));
            assert!(!session_matches(&admitted, 1001, 42, 1234));
            assert!(!session_matches(&admitted, 1000, 43, 1234));
            assert!(!session_matches(&admitted, 1000, 42, 1235));
            for (field, substitute) in [("State", "closing"), ("Service", "systemd-user"), ("Service", "login")] {
                let mut denied = admitted.clone();
                denied["data"][0][field]["data"] = substitute.into();
                assert!(!session_matches(&denied, 1000, 42, 1234));
            }
        }
    }
}
