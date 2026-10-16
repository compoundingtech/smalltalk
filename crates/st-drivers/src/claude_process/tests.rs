use super::*;
use std::collections::BTreeMap;

fn fixture() -> (Owner, BTreeMap<u32, Process>) {
    let wrapper = Process {
        pid: 10,
        parent: 2,
        birth: 100,
    };
    let provider = Process {
        pid: 20,
        parent: 10,
        birth: 200,
    };
    let owner = Owner {
        version: 1,
        session: "primary".into(),
        sequence: 3,
        runtime: "seat".into(),
        wrapper,
        provider: Some(provider),
        closed: false,
    };
    let nodes = [
        wrapper,
        provider,
        Process {
            pid: 30,
            parent: 20,
            birth: 300,
        }, // primary channel/hook
        Process {
            pid: 40,
            parent: 20,
            birth: 400,
        }, // nested provider
        Process {
            pid: 50,
            parent: 40,
            birth: 500,
        }, // nested channel/hook
        Process {
            pid: 60,
            parent: 20,
            birth: 600,
        }, // generated hook shell
        Process {
            pid: 70,
            parent: 60,
            birth: 700,
        },
    ]
    .into_iter()
    .map(|p| (p.pid, p))
    .collect();
    (owner, nodes)
}

fn read(nodes: &BTreeMap<u32, Process>, pid: u32) -> Result<Process> {
    nodes.get(&pid).copied().context("no such live process")
}

#[test]
fn primary_and_nested_channels_cannot_replace_each_other_in_either_order() {
    let (owner, nodes) = fixture();
    for order in [[30, 50], [50, 30]] {
        let mut epoch = 0;
        let mut admitted = Vec::new();
        for pid in order {
            if prove(
                &owner,
                pid,
                &Caller::Channel,
                |p| read(&nodes, p),
                |_| Ok(vec![]),
            )
            .is_ok()
            {
                epoch += 1;
                admitted.push(pid);
            }
        }
        assert_eq!(epoch, 1);
        assert_eq!(admitted, [30]);
        // Secondary EOF has no admitted epoch or callback and therefore no primary close.
        assert!(
            prove(
                &owner,
                50,
                &Caller::Channel,
                |p| read(&nodes, p),
                |_| Ok(vec![])
            )
            .is_err()
        );
        assert!(
            prove(
                &owner,
                30,
                &Caller::Channel,
                |p| read(&nodes, p),
                |_| Ok(vec![])
            )
            .is_ok()
        );
    }
}

#[test]
fn rewritten_record_cannot_pass_an_independent_wrapper_generation_or_run_a_callback() {
    let dir = tempfile::tempdir().unwrap();
    let sequence = claim(dir.path(), "fixture", "fixture", "current").unwrap();
    let mut owner: Owner = serde_json::from_slice(&read_owner(dir.path()).unwrap()).unwrap();
    let independently_captured = owner.wrapper;
    owner.wrapper.birth += 1;
    write(dir.path(), &owner).unwrap();
    let mut mutated = false;
    assert!(
        with_caller(
            dir.path(),
            "current",
            sequence,
            std::process::id(),
            Some(independently_captured),
            Caller::Wrapper,
            |_| {
                mutated = true;
                Ok(())
            }
        )
        .is_err()
    );
    assert!(!mutated);
}

#[test]
fn hook_trampoline_is_exact_and_nested_hook_cannot_rebind_native_session() {
    let (owner, nodes) = fixture();
    let command = "\"$ST_HOOKS/claude-observe.sh\" SessionStart";
    let role = Caller::Hook {
        trampoline: command,
    };
    let argv = |_: u32| Ok(vec!["/bin/sh".into(), "-c".into(), command.into()]);
    assert!(prove(&owner, 70, &role, |p| read(&nodes, p), argv).is_ok());
    assert!(
        prove(
            &owner,
            50,
            &role,
            |p| read(&nodes, p),
            |_| Ok(vec!["claude".into(), "-p".into()])
        )
        .is_err()
    );
    for argv in [
        vec!["sh", "-c", "unrelated"],
        vec!["python", "-c", command],
        vec!["sh", "-c", command, "extra"],
    ] {
        assert!(
            prove(
                &owner,
                70,
                &role,
                |p| read(&nodes, p),
                |_| Ok(argv.iter().map(|s| s.to_string()).collect())
            )
            .is_err()
        );
    }
}

#[test]
fn pid_reuse_reparenting_and_primary_exit_refuse_before_the_publication_callback() {
    let (owner, nodes) = fixture();
    let proof = prove(
        &owner,
        30,
        &Caller::Channel,
        |p| read(&nodes, p),
        |_| Ok(vec![]),
    )
    .unwrap();
    for pid in [10, 20, 30] {
        for mutate in ["birth", "parent", "exit"] {
            let mut changed = nodes.clone();
            match mutate {
                "birth" => changed.get_mut(&pid).unwrap().birth += 1,
                "parent" => changed.get_mut(&pid).unwrap().parent = 1,
                _ => {
                    changed.remove(&pid);
                }
            }
            assert!(
                validate_proof(&proof, |p| read(&changed, p)).is_err(),
                "{pid}/{mutate}"
            );
        }
    }
}

#[test]
fn legacy_generation_stays_legacy_and_malformed_witness_never_downgrades() {
    let dir = tempfile::tempdir().unwrap();
    let admit = || {
        with_caller(
            dir.path(),
            "old",
            1,
            std::process::id(),
            None,
            Caller::Channel,
            Ok,
        )
    };
    assert_eq!(admit().unwrap(), Admission::Legacy);
    assert!(!dir.path().join(FILE).exists());
    assert!(!dir.path().join(LOCK).exists());
    let _ = lock(dir.path(), true).unwrap();
    fs::write(dir.path().join(FILE), b"malformed").unwrap();
    assert!(admit().is_err());
    // Old daemons ignore this additive local witness. New daemons preserve absence for old
    // wrappers; they do not rewrite it into an allegedly protected generation.
}

#[test]
fn stale_session_sequence_and_closed_generation_never_run_the_callback() {
    let dir = tempfile::tempdir().unwrap();
    let seq = claim(dir.path(), "fixture", "fixture", "current").unwrap();
    for (session, sequence) in [("previous", seq), ("current", seq + 1)] {
        let mut ran = false;
        assert!(
            with_caller(
                dir.path(),
                session,
                sequence,
                std::process::id(),
                None,
                Caller::Wrapper,
                |_| {
                    ran = true;
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!ran);
    }
    assert_eq!(
        with_caller(
            dir.path(),
            "current",
            seq,
            std::process::id(),
            None,
            Caller::Wrapper,
            Ok
        )
        .unwrap(),
        Admission::Primary
    );
    ended(dir.path(), "current", seq).unwrap();
    assert!(
        with_caller(
            dir.path(),
            "current",
            seq,
            std::process::id(),
            None,
            Caller::Wrapper,
            Ok
        )
        .is_err()
    );
}

/// Ordinary hosted kernel control only. Every child is this test executable; no Claude,
/// daemon, provider, network call, terminal or live seat participates.
#[test]
fn kernel_primary_and_nested_helpers_with_identical_credentials_are_isolated() {
    use std::process::{Command, Stdio};
    const ROLE: &str = "ST_TEST_CLAUDE_PROCESS_ROLE";
    const TEST: &str = "claude_process::tests::kernel_primary_and_nested_helpers_with_identical_credentials_are_isolated";
    let role = std::env::var(ROLE).ok();
    let command = |role: &str| {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", TEST, "--nocapture"]).env(ROLE, role);
        cmd
    };
    if let Some(role) = role {
        let dir = std::path::PathBuf::from(std::env::var_os("ST_TEST_CLAUDE_PROCESS_DIR").unwrap());
        let seq = std::env::var("ST_CLAUDE_SESSION_SEQ")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(std::env::var("ST_CLAUDE_SESSION").unwrap(), "kernel");
        match role.as_str() {
            "provider" => {
                use std::io::Read as _;
                let own = process(std::process::id()).unwrap();
                assert!(own.birth > 0);
                assert_eq!(process(own.pid).unwrap(), own);
                let argv = arguments(own.pid).unwrap();
                assert!(
                    argv.iter().any(|arg| arg == TEST),
                    "real argv parse, including Darwin KERN_PROCARGS2"
                );
                // Real startup race: helpers act while the actual wrapper's provider slot is
                // still None. Parent waits for this acknowledgement before recording spawned.
                assert!(command("primary").status().unwrap().success());
                assert!(command("nested").status().unwrap().success());
                fs::write(dir.join("startup-proof-complete"), b"yes").unwrap();
                std::io::stdin().read_exact(&mut [0u8; 1]).unwrap();
                assert!(command("primary").status().unwrap().success());
                assert!(command("primary-hook").status().unwrap().success());
                assert!(command("nested").status().unwrap().success());
            }
            "nested" => {
                assert!(command("secondary").status().unwrap().success());
                assert!(command("secondary-hook").status().unwrap().success());
            }
            _ => {
                let hook = role.ends_with("-hook");
                let caller = if hook {
                    Caller::Hook {
                        trampoline: "\"$ST_HOOKS/claude-observe.sh\" SessionStart",
                    }
                } else {
                    Caller::Channel
                };
                let result = with_caller(
                    &dir,
                    "kernel",
                    seq,
                    std::process::id(),
                    None,
                    caller,
                    |admission| {
                        if hook {
                            fs::write(dir.join("native-binding-control"), role.as_bytes())?;
                        }
                        Ok(admission)
                    },
                );
                if role.starts_with("primary") {
                    assert_eq!(result.unwrap(), Admission::Primary);
                } else {
                    assert!(result.is_err());
                }
            }
        }
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let seq = claim(dir.path(), "fixture", "fixture", "kernel").unwrap();
    let mut child = command("provider")
        .env("ST_TEST_CLAUDE_PROCESS_DIR", dir.path())
        .env("ST_CLAUDE_SESSION_SEQ", seq.to_string())
        .env("ST_CLAUDE_SESSION", "kernel")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    wait_fixture(&dir.path().join("startup-proof-complete"));
    let before: Owner = serde_json::from_slice(&read_owner(dir.path()).unwrap()).unwrap();
    assert!(before.provider.is_none());
    spawned(dir.path(), "kernel", seq, child.id()).unwrap();
    use std::io::Write as _;
    child.stdin.take().unwrap().write_all(b"g").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        fs::read(dir.path().join("native-binding-control")).unwrap(),
        b"primary-hook"
    );
    ended(dir.path(), "kernel", seq).unwrap();
}

#[test]
fn startup_and_reexec_keep_the_exact_primary_and_never_admit_a_nested_candidate() {
    let (mut owner, nodes) = fixture();
    owner.provider = None;
    assert!(
        prove(
            &owner,
            30,
            &Caller::Channel,
            |p| read(&nodes, p),
            |_| Ok(vec![])
        )
        .is_ok()
    );
    assert!(
        prove(
            &owner,
            50,
            &Caller::Channel,
            |p| read(&nodes, p),
            |_| Ok(vec![])
        )
        .is_err()
    );
    owner.provider = Some(nodes[&20]);
    // A driver reexec retains PID, birth, provider and session sequence. A different image
    // does not fabricate a new parent; a replacement generation must claim a new witness.
    let retained = prove(
        &owner,
        30,
        &Caller::Channel,
        |p| read(&nodes, p),
        |_| Ok(vec![]),
    )
    .unwrap();
    validate_proof(&retained, |p| read(&nodes, p)).unwrap();
    owner.provider.as_mut().unwrap().birth += 1;
    assert!(
        prove(
            &owner,
            30,
            &Caller::Channel,
            |p| read(&nodes, p),
            |_| Ok(vec![])
        )
        .is_err()
    );
}

#[test]
fn hooks_never_create_ownership_files_and_oversize_never_becomes_legacy() {
    let dir = tempfile::tempdir().unwrap();
    let admit = || {
        with_caller(
            dir.path(),
            "hook",
            1,
            std::process::id(),
            None,
            Caller::Hook {
                trampoline: "\"$ST_HOOKS/claude-observe.sh\" SessionStart",
            },
            Ok,
        )
    };
    assert_eq!(admit().unwrap(), Admission::Legacy);
    assert!(!dir.path().join(FILE).exists());
    assert!(!dir.path().join(LOCK).exists());
    let mut published = false;
    let error = with_caller(
        dir.path(),
        "hook",
        1,
        std::process::id(),
        Some(process(std::process::id()).unwrap()),
        Caller::Wrapper,
        |_| {
            published = true;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(failure_kind(&error), Some(FailureKind::Refused));
    assert!(
        !published,
        "known protected generation cannot downgrade during admission"
    );
    assert!(!dir.path().join(FILE).exists());
    assert!(!dir.path().join(LOCK).exists());
    fs::write(dir.path().join(FILE), b"{}").unwrap();
    assert_eq!(
        failure_kind(&admit().unwrap_err()),
        Some(FailureKind::Unavailable)
    );
    assert!(
        !dir.path().join(LOCK).exists(),
        "a hook cannot mint even the lock"
    );
    drop(lock(dir.path(), true).unwrap());
    fs::write(dir.path().join(FILE), vec![b' '; 4097]).unwrap();
    assert_eq!(
        failure_kind(&admit().unwrap_err()),
        Some(FailureKind::Refused)
    );
}

#[test]
fn witness_replacement_after_kernel_proof_and_unreadable_hook_birth_never_publish() {
    let (owner, nodes) = fixture();
    let bytes = serde_json::to_vec(&owner).unwrap();
    let mut replacement = owner.clone();
    replacement.provider = None;
    let changed = serde_json::to_vec(&replacement).unwrap();
    let replace_dir = tempfile::tempdir().unwrap();
    fs::write(replace_dir.path().join(FILE), &bytes).unwrap();
    let mut called = false;
    let result = check_processes(
        &owner,
        &bytes,
        30,
        &Caller::Hook {
            trampoline: "generated",
        },
        |pid| read(&nodes, pid),
        |_| Ok(vec!["claude".into()]),
        || {
            let next = replace_dir.path().join("replacement");
            fs::write(&next, &changed)?;
            fs::rename(next, replace_dir.path().join(FILE))?;
            read_owner(replace_dir.path())
        },
    );
    if result.is_ok() {
        called = true;
    }
    assert_eq!(
        failure_kind(&result.unwrap_err()),
        Some(FailureKind::Refused)
    );
    assert!(!called);
    let result = check_processes(
        &owner,
        &bytes,
        30,
        &Caller::Hook {
            trampoline: "generated",
        },
        |pid| {
            if pid == 30 {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
            } else {
                read(&nodes, pid)
            }
        },
        |_| Ok(vec![]),
        || Ok(bytes.clone()),
    );
    assert_eq!(
        failure_kind(&result.unwrap_err()),
        Some(FailureKind::Unavailable)
    );
    // A real unavailable kernel caller on a recorded generation must not run the callback.
    let dir = tempfile::tempdir().unwrap();
    let seq = claim(dir.path(), "fixture", "fixture", "hook").unwrap();
    let error = with_caller(
        dir.path(),
        "hook",
        seq,
        u32::MAX,
        None,
        Caller::Hook {
            trampoline: "generated",
        },
        |_| {
            called = true;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(failure_kind(&error), Some(FailureKind::Unavailable));
    assert!(!called);
}

#[test]
fn only_unavailable_proof_gets_one_recheck_and_publication_is_never_retried() {
    let mut captures = 0;
    let result = proof_with_recheck_clock(
        || {
            captures += 1;
            if captures == 1 {
                Err(unavailable(
                    std::io::Error::from(std::io::ErrorKind::Interrupted).into(),
                ))
            } else {
                Ok(Admission::Primary)
            }
        },
        || Duration::ZERO,
        || {},
    );
    assert_eq!(result.unwrap(), Admission::Primary);
    assert_eq!(captures, 2);
    captures = 0;
    let error = proof_with_recheck_clock::<()>(
        || {
            captures += 1;
            Err(refused("secondary fixture").into())
        },
        || Duration::ZERO,
        || {},
    )
    .unwrap_err();
    assert_eq!(failure_kind(&error), Some(FailureKind::Refused));
    assert_eq!(captures, 1);
    captures = 0;
    let error = proof_with_recheck_clock::<()>(
        || {
            captures += 1;
            Err(unavailable(
                std::io::Error::from(std::io::ErrorKind::Interrupted).into(),
            ))
        },
        || Duration::ZERO,
        || {},
    )
    .unwrap_err();
    assert_eq!(captures, 2);
    assert_eq!(failure_kind(&error), Some(FailureKind::Unavailable));
    assert!(diagnostic(&error).len() < 128);
    captures = 0;
    let mut paused = false;
    let error = proof_with_recheck_clock::<()>(
        || {
            captures += 1;
            Err(unavailable(
                std::io::Error::from(std::io::ErrorKind::Interrupted).into(),
            ))
        },
        || PROOF_BUDGET,
        || {
            paused = true;
        },
    )
    .unwrap_err();
    assert_eq!(captures, 1, "expired budget cannot admit another capture");
    assert!(!paused);
    assert_eq!(failure_kind(&error), Some(FailureKind::Unavailable));
    let dir = tempfile::tempdir().unwrap();
    let seq = claim(dir.path(), "fixture", "fixture", "hook").unwrap();
    let mut writes = 0;
    let error = with_caller::<()>(
        dir.path(),
        "hook",
        seq,
        std::process::id(),
        None,
        Caller::Wrapper,
        |_| {
            writes += 1;
            Err(unavailable(
                std::io::Error::from(std::io::ErrorKind::Interrupted).into(),
            ))
        },
    )
    .unwrap_err();
    assert_eq!(failure_kind(&error), Some(FailureKind::Unavailable));
    assert_eq!(
        writes, 1,
        "even a typed action error must never replay publication"
    );
}

#[test]
fn process_and_harness_lock_contention_is_bounded_unavailable_not_secondary() {
    let dir = tempfile::tempdir().unwrap();
    let seq = claim(dir.path(), "fixture", "fixture", "hook").unwrap();
    for held in [
        dir.path().join(LOCK),
        dir.path().join(".harness-state.lock"),
    ] {
        let guard = crate::flock::FileLock::hold_blocking(
            crate::flock::open(&held, crate::flock::Open::Existing).unwrap(),
            crate::flock::Mode::Exclusive,
        )
        .unwrap();
        let started = Instant::now();
        let mut wrote = false;
        let error = with_caller(
            dir.path(),
            "hook",
            seq,
            std::process::id(),
            None,
            Caller::Wrapper,
            |_| {
                wrote = true;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "new proof cannot queue on a live holder"
        );
        assert_eq!(failure_kind(&error), Some(FailureKind::Unavailable));
        assert!(diagnostic(&error).contains("publication withheld"));
        assert!(!wrote);
        drop(guard);
        assert_eq!(
            with_caller(
                dir.path(),
                "hook",
                seq,
                std::process::id(),
                None,
                Caller::Wrapper,
                Ok
            )
            .unwrap(),
            Admission::Primary
        );
    }
}

#[test]
fn early_provider_window_requires_a_non_subreaper_wrapper() {
    assert!(ensure_no_subreaper(false).is_ok());
    assert_eq!(
        failure_kind(&ensure_no_subreaper(true).unwrap_err()),
        Some(FailureKind::Refused)
    );
    // This kernel read is about the isolated test wrapper, never a running agent/provider.
    assert!(!subreaper().unwrap());
}

#[test]
fn deeper_launcher_shim_is_a_deliberate_new_direct_child_refusal() {
    let (owner, mut nodes) = fixture();
    // This chain can be legitimate under the old any-descendant rule. It is deliberately
    // unsupported by the stricter shape, never proof that an installed provider is broken.
    nodes.insert(
        80,
        Process {
            pid: 80,
            parent: 20,
            birth: 800,
        },
    );
    nodes.get_mut(&30).unwrap().parent = 80;
    let error = prove(
        &owner,
        30,
        &Caller::Channel,
        |pid| read(&nodes, pid),
        |_| Ok(vec![]),
    )
    .unwrap_err();
    assert_eq!(failure_kind(&error), Some(FailureKind::Refused));
    assert!(error.to_string().contains("direct-parent ownership"));
}

fn wait_fixture(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture barrier {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Real kernel exit/reparent/zombie/reexec controls. Children are only this test executable.
/// Birth-reuse remains an injected mismatch against real captured identities; no PID allocator
/// or machine setting is changed to force literal PID recycling.
#[test]
fn kernel_orphan_zombie_wrapper_exit_and_reexec_keep_generation_fences() {
    use std::process::{Command, Stdio};
    const ROLE: &str = "ST_TEST_KERNEL_LIFECYCLE_ROLE";
    const TEST: &str = "claude_process::tests::kernel_orphan_zombie_wrapper_exit_and_reexec_keep_generation_fences";
    let command = |role: &str| {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", TEST, "--nocapture"]).env(ROLE, role);
        cmd
    };
    if let Ok(role) = std::env::var(ROLE) {
        let dir =
            std::path::PathBuf::from(std::env::var_os("ST_TEST_KERNEL_LIFECYCLE_DIR").unwrap());
        match role.as_str() {
            "exit" => {}
            "wrapper-source" => {
                let seq = claim(&dir, "fixture", "fixture", "orphan").unwrap();
                // Waiting here would remove the reparenting fault this branch must create.
                // The root waits this wrapper; its child exits after the bounded release barrier.
                #[expect(clippy::zombie_processes, reason = "deliberate private orphan fixture; wrapper exits before the released child")]
                let child = command("orphan-provider").spawn().unwrap();
                spawned(&dir, "orphan", seq, child.id()).unwrap();
                // Deliberately exit without ended: this models a wrapper disappearing with
                // a retained witness. The root reaps this wrapper before releasing the orphan.
            }
            "orphan-provider" => {
                wait_fixture(&dir.join("release-orphan"));
                let owner: Owner = serde_json::from_slice(&read_owner(&dir).unwrap()).unwrap();
                assert_ne!(
                    process(std::process::id()).unwrap().parent,
                    owner.wrapper.pid
                );
                assert!(command("orphan-channel").status().unwrap().success());
                fs::write(dir.join("orphan-done"), b"yes").unwrap();
            }
            "orphan-channel" | "reexec-channel" => {
                let owner: Owner = serde_json::from_slice(&read_owner(&dir).unwrap()).unwrap();
                let mut published = false;
                let result = with_caller(
                    &dir,
                    &owner.session,
                    owner.sequence,
                    std::process::id(),
                    None,
                    Caller::Channel,
                    |_| {
                        published = true;
                        Ok(())
                    },
                );
                if role == "orphan-channel" {
                    assert!(result.is_err());
                    assert!(!published);
                } else {
                    result.unwrap();
                    assert!(published);
                }
            }
            "reexec-before" => {
                let seq = claim(&dir, "fixture", "fixture", "reexec").unwrap();
                // The same parent PID reaps the recorded child with waitpid after exec below.
                #[expect(clippy::zombie_processes, reason = "owned child is explicitly reaped by reexec-after using its recorded PID")]
                let child = command("held-provider").spawn().unwrap();
                spawned(&dir, "reexec", seq, child.id()).unwrap();
                use std::os::unix::process::CommandExt as _;
                panic!("fixture exec failed: {}", command("reexec-after").exec());
            }
            "held-provider" => {
                wait_fixture(&dir.join("reexec-ready"));
                assert!(command("reexec-channel").status().unwrap().success());
                fs::write(dir.join("reexec-proof"), b"yes").unwrap();
            }
            "reexec-after" => {
                let owner: Owner = serde_json::from_slice(&read_owner(&dir).unwrap()).unwrap();
                assert_eq!(process(std::process::id()).unwrap(), owner.wrapper);
                let provider = owner.provider.unwrap();
                assert_eq!(
                    process(provider.pid).unwrap(),
                    provider,
                    "exec retained the exact provider birth"
                );
                fs::write(dir.join("reexec-ready"), b"yes").unwrap();
                wait_fixture(&dir.join("reexec-proof"));
                let mut status = 0;
                // SAFETY: this is the direct child recorded before exec, still owned by this
                // same wrapper PID; waitpid cannot reap a different process on PID reuse.
                assert_eq!(
                    unsafe { libc::waitpid(provider.pid as i32, &mut status, 0) },
                    provider.pid as i32
                );
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 0);
                ended(&dir, &owner.session, owner.sequence).unwrap();
            }
            _ => panic!("unknown fixture role"),
        }
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let spawn = |role: &str| {
        let mut cmd = command(role);
        cmd.env("ST_TEST_KERNEL_LIFECYCLE_DIR", dir.path());
        cmd
    };
    // Observe termination without reaping: EOF alone can race the final exit syscall.
    let mut child = spawn("exit").stdout(Stdio::null()).spawn().unwrap();
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: waitid writes siginfo for this exact direct child. WNOWAIT retains the
    // zombie for the kernel-source assertion; Child::wait below owns its final reap.
    assert_eq!(
        unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        },
        0
    );
    assert!(
        process(child.id()).is_err(),
        "an unreaped exited child cannot provide a live birth/parent proof"
    );
    assert!(child.wait().unwrap().success());
    let mut wrapper = spawn("wrapper-source").spawn().unwrap();
    assert!(wrapper.wait().unwrap().success());
    fs::write(dir.path().join("release-orphan"), b"yes").unwrap();
    wait_fixture(&dir.path().join("orphan-done"));
    let execdir = tempfile::tempdir().unwrap();
    let mut wrapper = command("reexec-before")
        .env("ST_TEST_KERNEL_LIFECYCLE_DIR", execdir.path())
        .spawn()
        .unwrap();
    assert!(wrapper.wait().unwrap().success());
    assert!(execdir.path().join("reexec-proof").exists());
    let owner: Owner = serde_json::from_slice(&read_owner(execdir.path()).unwrap()).unwrap();
    assert!(
        owner.closed,
        "real provider exit closes retained ownership after exec"
    );
}

#[test]
fn typed_context_and_standard_sources_preserve_failure_classification() {
    #[derive(Debug)]
    struct Source(Failure);
    impl std::fmt::Display for Source {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("fixture source wrapper")
        }
    }
    impl std::error::Error for Source {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    for kind in [FailureKind::Refused, FailureKind::Unavailable] {
        let context = anyhow::Error::new(std::io::Error::from(
            std::io::ErrorKind::Interrupted,
        ))
        .context(Failure { kind, reason: "fixture typed context" });
        assert_eq!(failure_kind(&context), Some(kind));
        let source = anyhow::Error::new(Source(Failure {
            kind,
            reason: "fixture standard source",
        }));
        assert_eq!(failure_kind(&source), Some(kind));
    }
}
