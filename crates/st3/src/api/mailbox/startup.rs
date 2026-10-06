//! A native startup wait comes from live local launch evidence, not a retained status POST.
use super::*;

pub(super) fn live_native_incarnation(
    pty_root: &std::path::Path,
    peer: &NativeDeliveryPeer,
    fence: &Fence,
) -> bool {
    if !same_caller_birth(peer) {
        return false;
    }
    let Ok(observations) = st_runtime::PtyRuntime::new(pty_root.to_path_buf()).snapshot() else {
        return false;
    };
    same_caller_birth(peer)
        && observations.iter().any(|observation| {
            matches_incarnation(observation, peer, fence)
                && observation
                    .pid
                    .is_some_and(|pid| is_descendant(peer.pid, pid))
        })
}

fn same_caller_birth(peer: &NativeDeliveryPeer) -> bool {
    peer.start_token
        .is_some_and(|start| st_runtime::process_start_token(peer.pid).ok() == Some(start))
}

fn matches_incarnation(
    observation: &st_runtime::PtyObservation,
    peer: &NativeDeliveryPeer,
    fence: &Fence,
) -> bool {
    observation.status == "running"
        && peer.agent == fence.subject
        && observation.tags.get("st3.subject") == Some(&fence.subject)
        && observation
            .pid
            .zip(observation.created_at.as_deref())
            .is_some_and(|(pid, at)| fence.incarnation == format!("{pid}:{at}"))
}

#[cfg(target_os = "linux")]
fn is_descendant(mut pid: u32, root: u32) -> bool {
    // Kernel peer identity must belong to this particular launch, even when an old
    // process still has the same ST_AGENT. Cycles and unreadable ancestry fail closed.
    let mut seen = std::collections::BTreeSet::new();
    while pid > 1 && seen.insert(pid) {
        if pid == root {
            return true;
        }
        let Some(parent) = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(") ").map(|(_, tail)| tail.to_owned()))
            .and_then(|tail| tail.split_whitespace().nth(1)?.parse::<u32>().ok())
        else {
            return false;
        };
        pid = parent;
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn is_descendant(_pid: u32, _root: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_evidence_binds_subject_incarnation_and_live_launch() {
        let mut observation = st_runtime::PtyObservation {
            name: "fixture".into(),
            status: "running".into(),
            exit_code: None,
            pid: Some(42),
            created_at: Some("launch-time".into()),
            display_name: None,
            tags: std::collections::BTreeMap::from([("st3.subject".into(), "agent/cedar".into())]),
        };
        let peer = NativeDeliveryPeer {
            agent: "agent/cedar".into(),
            transport: "omp-channel",
            pid: 43,
            archives_inbox: false,
            start_token: None,
        };
        let fence = Fence::new("agent/cedar", "42:launch-time", "delivery");
        assert!(matches_incarnation(&observation, &peer, &fence));
        assert!(!matches_incarnation(
            &observation,
            &peer,
            &Fence::new("agent/cedar", "42:older-launch", "delivery")
        ));
        observation
            .tags
            .insert("st3.subject".into(), "agent/birch".into());
        assert!(!matches_incarnation(&observation, &peer, &fence));
        observation
            .tags
            .insert("st3.subject".into(), "agent/cedar".into());
        observation.status = "exited".into();
        assert!(!matches_incarnation(&observation, &peer, &fence));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn startup_wait_requires_the_kernel_peer_to_belong_to_the_launch() {
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let pid = std::process::id();
        let child = OwnedChild(
            crate::test_support::command("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        assert!(is_descendant(child.0.id(), pid));
        assert!(is_descendant(pid, pid));
        assert!(!is_descendant(child.0.id(), u32::MAX));
    }
    #[test]
    fn bootstrap_refuses_missing_or_reused_authenticated_process_identity() {
        let pid = std::process::id();
        let mut peer = NativeDeliveryPeer {
            agent: "agent/cedar".into(),
            transport: "omp-channel",
            pid,
            archives_inbox: false,
            start_token: None,
        };
        assert!(!same_caller_birth(&peer));
        let start = st_runtime::process_start_token(pid).unwrap();
        peer.start_token = Some(start);
        assert!(same_caller_birth(&peer));
        peer.start_token = Some(start.wrapping_add(1));
        assert!(!same_caller_birth(&peer));
    }
}
