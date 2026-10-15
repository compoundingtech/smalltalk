//! The system call filter every tile process runs under. Once its namespaces are set up, a tile
//! can neither make nor join another namespace, nor change what is mounted, and it cannot reach
//! the kernel interfaces an unprivileged process most often turns into root: io_uring, BPF,
//! userfaultfd, perf events, the kernel keyrings, kcmp and reading or writing another process's
//! memory. Refused calls fail with EPERM; io_uring and clone3 fail with ENOSYS, so a runtime falls
//! back to ordinary system calls (libc falls back from clone3 to clone, whose flags the filter
//! can read). A call from another architecture's system call table ends the process.
//!
//! This narrows the kernel a tile reaches; it is not the boundary itself. The boundary is the
//! tile's own Unix user, its namespaces and its root-owned cgroup. The filter refuses a list of
//! calls rather than allowing one, so a new kernel interface is reachable until it is added here.

use std::io;

/// `AUDIT_ARCH_*` of the architectures a tile can run on.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// x86-64's x32 system calls set this bit in the number.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// Flags of clone that make a namespace.
const CLONE_NAMESPACES: u32 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWCGROUP
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET) as u32;

/// Calls refused with EPERM.
const REFUSED: &[libc::c_long] = &[
    // Namespaces and mounts.
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_open_tree,
    libc::SYS_move_mount,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_mount_setattr,
    // Kernel surfaces an unprivileged process reaches and that tools do not need.
    libc::SYS_bpf,
    libc::SYS_userfaultfd,
    libc::SYS_perf_event_open,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_keyctl,
    libc::SYS_kcmp,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
];

/// Calls that fail with ENOSYS, as on a kernel without them, so callers use their fallback.
const UNAVAILABLE: &[libc::c_long] = &[
    libc::SYS_clone3,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

// Offsets into `struct seccomp_data`.
const NR: u32 = 0;
const ARCH: u32 = 4;
const ARG0_LOW: u32 = 16;

fn statement(code: u32, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    }
}

const LOAD: u32 = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
const EQUAL: u32 = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
const AT_LEAST: u32 = libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K;
const ANY_BIT: u32 = libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K;
const RETURN: u32 = libc::BPF_RET | libc::BPF_K;

const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;
const EPERM: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
const ENOSYS: u32 = libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32;

/// The filter program.
pub fn program() -> Vec<libc::sock_filter> {
    let mut program = vec![
        statement(LOAD, ARCH),
        jump(EQUAL, AUDIT_ARCH, 1, 0),
        statement(RETURN, KILL),
        statement(LOAD, NR),
    ];
    #[cfg(target_arch = "x86_64")]
    {
        program.push(jump(AT_LEAST, X32_SYSCALL_BIT, 0, 1));
        program.push(statement(RETURN, EPERM));
    }
    for &call in REFUSED {
        program.push(jump(EQUAL, call as u32, 0, 1));
        program.push(statement(RETURN, EPERM));
    }
    for &call in UNAVAILABLE {
        program.push(jump(EQUAL, call as u32, 0, 1));
        program.push(statement(RETURN, ENOSYS));
    }
    // clone: refuse namespace flags in its first argument. The accumulator holds the number.
    program.push(jump(EQUAL, libc::SYS_clone as u32, 0, 3));
    program.push(statement(LOAD, ARG0_LOW));
    program.push(jump(ANY_BIT, CLONE_NAMESPACES, 0, 1));
    program.push(statement(RETURN, EPERM));
    program.push(statement(RETURN, ALLOW));
    program
}

/// Install the filter on this thread and every process it starts. The caller has set
/// `PR_SET_NO_NEW_PRIVS`.
pub fn install() -> io::Result<()> {
    let program = program();
    let fprog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: fprog points at a live, correctly sized filter for the duration of the call.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER as libc::c_ulong,
            &fprog as *const libc::sock_fprog as libc::c_ulong,
            0,
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the filter as the kernel would, for one call on this architecture.
    fn verdict(nr: u32, arg0: u32, arch: u32) -> u32 {
        let program = program();
        let mut accumulator = 0u32;
        let mut pc = 0usize;
        loop {
            let op = program[pc];
            let code = op.code as u32;
            pc += 1;
            if code == LOAD {
                accumulator = match op.k {
                    NR => nr,
                    ARCH => arch,
                    ARG0_LOW => arg0,
                    other => panic!("load at {other}"),
                };
            } else if code == RETURN {
                return op.k;
            } else {
                let taken = match code {
                    EQUAL => accumulator == op.k,
                    AT_LEAST => accumulator >= op.k,
                    ANY_BIT => accumulator & op.k != 0,
                    other => panic!("op {other:#x}"),
                };
                pc += if taken { op.jt } else { op.jf } as usize;
            }
        }
    }

    #[test]
    fn the_filter_refuses_namespaces_and_mounts_and_allows_the_rest() {
        let call = |nr: libc::c_long, arg0: u32| verdict(nr as u32, arg0, AUDIT_ARCH);
        for &refused in REFUSED {
            assert_eq!(call(refused, 0), EPERM, "{refused}");
        }
        for &unavailable in UNAVAILABLE {
            assert_eq!(call(unavailable, 0), ENOSYS, "{unavailable}");
        }
        assert_eq!(call(libc::SYS_clone, libc::CLONE_NEWUSER as u32), EPERM);
        assert_eq!(call(libc::SYS_clone, libc::CLONE_NEWNET as u32), EPERM);
        // A thread or a fork is fine.
        let thread = (libc::CLONE_VM
            | libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SIGHAND
            | libc::CLONE_THREAD) as u32;
        assert_eq!(call(libc::SYS_clone, thread), ALLOW);
        assert_eq!(call(libc::SYS_clone, libc::SIGCHLD as u32), ALLOW);
        for allowed in [
            libc::SYS_read,
            libc::SYS_openat,
            libc::SYS_execve,
            libc::SYS_connect,
        ] {
            assert_eq!(call(allowed, 0), ALLOW);
        }
        // Another architecture's table never reaches the rules.
        assert_eq!(verdict(libc::SYS_read as u32, 0, 0x4000_0003), KILL);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            call(X32_SYSCALL_BIT as libc::c_long | libc::SYS_unshare, 0),
            EPERM
        );
    }
}
