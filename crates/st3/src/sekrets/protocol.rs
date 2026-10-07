//! The gateway's socket protocol: length-prefixed JSON frames over a Unix stream, with file
//! descriptors passed beside a frame (`SCM_RIGHTS`). A caller passes its standard streams and its
//! working directory as descriptors, so a command's output goes straight to the caller and the
//! gateway reads nothing of it, and the gateway opens no path the caller names.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

use super::policy::Policy;

/// A closed peer is an error, not SIGPIPE; received descriptors close on exec. Linux sets both
/// per call; elsewhere SIGPIPE stays the process's own concern and descriptors are marked after.
#[cfg(target_os = "linux")]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const SEND_FLAGS: libc::c_int = 0;
#[cfg(target_os = "linux")]
const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const RECV_FLAGS: libc::c_int = 0;

/// The largest frame either side accepts.
const MAX_FRAME: usize = 1 << 20;
/// The most descriptors one frame carries.
pub const MAX_FDS: usize = 16;

/// The terminal size a caller's terminal has, for a command that runs on a terminal.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
}

/// A daemon's word on which seat a process is, signed with its node key. See `identity`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attestation {
    /// Canonical JSON of `identity::Statement`.
    pub statement: String,
    pub signature: String,
}

/// Where a run's descriptors go. The descriptors arrive in this order beside the request: on a
/// terminal none of stdin, stdout and stderr; otherwise those three; then each checkout directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunRequest {
    #[serde(default)]
    pub profile: Option<String>,
    pub argv: Vec<String>,
    /// The size of the caller's terminal, when the command should run on one.
    #[serde(default)]
    pub tty: Option<Winsize>,
    #[serde(default)]
    pub term: Option<String>,
    /// How many checkout directories follow the standard streams: the working directory first,
    /// then the git directories it uses.
    #[serde(default)]
    pub directories: usize,
    /// How many files the command reads follow the checkout directories; the arguments name
    /// them with placeholders the gateway turns into `/dev/fd/N`. See `files`.
    #[serde(default)]
    pub files: usize,
    /// The working directory, relative to the first checkout directory.
    #[serde(default)]
    pub cwd_within: Option<String>,
    #[serde(default)]
    pub attestation: Option<Attestation>,
    /// Run the profile owner's login for a tool rather than a policy-checked command.
    #[serde(default)]
    pub login: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// Who the gateway takes the caller for, and a nonce for a daemon attestation.
    Hello {
        #[serde(default)]
        attestation: Option<Attestation>,
    },
    Run(RunRequest),
    /// Forward a signal to a running command.
    Signal {
        signal: i32,
    },
    ProfileCreate {
        profile: String,
        #[serde(default)]
        description: Option<String>,
        policy: Policy,
        #[serde(default)]
        default: bool,
    },
    ProfileList,
    ProfileShow {
        profile: String,
    },
    ProfileRemove {
        profile: String,
    },
    PolicySet {
        profile: String,
        policy: Policy,
    },
    /// Store a value the profile's commands get as an environment variable. Write-only.
    Put {
        profile: String,
        name: String,
        value: String,
    },
    Unset {
        profile: String,
        name: String,
    },
    GrantAdd {
        profile: String,
        to: String,
        policy: Policy,
        #[serde(default)]
        until_unix_ms: Option<i64>,
    },
    GrantRemove {
        grant: String,
    },
    GrantList,
    Lock {
        #[serde(default)]
        person: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    Unlock {
        #[serde(default)]
        person: Option<String>,
    },
    /// Register the node key this person's daemon signs seat attestations with.
    Register {
        node: String,
        key: String,
    },
    /// Log entries this person may read, after a sequence number.
    Log {
        #[serde(default)]
        after: i64,
        #[serde(default = "default_log_limit")]
        limit: i64,
        /// Wait up to this long for an entry when there is none yet.
        #[serde(default)]
        wait_ms: u64,
    },
}

fn default_log_limit() -> i64 {
    100
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CallerView {
    Person {
        person: String,
    },
    Agent {
        agent: String,
        person: String,
    },
    /// A process of a known person's Unix user that is neither in a login session nor attested.
    Unidentified {
        person: String,
        reason: String,
    },
}

impl CallerView {
    pub fn principal(&self) -> Option<&str> {
        match self {
            Self::Person { person } => Some(person),
            Self::Agent { agent, .. } => Some(agent),
            Self::Unidentified { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "kebab-case")]
pub enum Reply {
    Hello {
        caller: CallerView,
        nonce: String,
        gateway: String,
    },
    /// The command started. On a terminal, the terminal's controlling side comes with it.
    Started {
        profile: String,
        call: i64,
        /// Something the caller should know, such as where the command runs.
        #[serde(default)]
        note: Option<String>,
    },
    Exited {
        #[serde(default)]
        code: Option<i32>,
        #[serde(default)]
        signal: Option<i32>,
    },
    Refused {
        reason: String,
        #[serde(default)]
        call: Option<i64>,
    },
    Ok {
        #[serde(default)]
        value: serde_json::Value,
    },
    Error {
        message: String,
    },
}

pub fn send<T: Serialize>(stream: &UnixStream, value: &T, fds: &[RawFd]) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    let sent = sendmsg(stream.as_raw_fd(), &frame, fds)?;
    (&*stream).write_all(&frame[sent..])
}

/// The next frame, or `None` at a clean end of stream.
pub fn recv<T: for<'de> Deserialize<'de>>(
    stream: &UnixStream,
) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let mut header = [0_u8; 4];
    let (read, fds) = recvmsg(stream.as_raw_fd(), &mut header)?;
    if read == 0 {
        return Ok(None);
    }
    if read < header.len() {
        (&*stream).read_exact(&mut header[read..])?;
    }
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut body = vec![0_u8; length];
    (&*stream).read_exact(&mut body)?;
    let value = serde_json::from_slice(&body).map_err(io::Error::other)?;
    Ok(Some((value, fds)))
}

fn sendmsg(socket: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    assert!(fds.len() <= MAX_FDS);
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let fd_bytes = std::mem::size_of_val(fds) as u32;
    let mut control = vec![0_u8; unsafe { libc::CMSG_SPACE(fd_bytes) } as usize];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if !fds.is_empty() {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(fd_bytes) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                fd_bytes as usize,
            );
        }
    }
    loop {
        let sent = unsafe { libc::sendmsg(socket, &message, SEND_FLAGS) };
        if sent >= 0 {
            return Ok(sent as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn recvmsg(socket: RawFd, buffer: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    let space =
        unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0_u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;
    let read = loop {
        let read = unsafe { libc::recvmsg(socket, &mut message, RECV_FLAGS) };
        if read >= 0 {
            break read as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    let mut fds = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header);
                let count = ((*header).cmsg_len as usize - (data as usize - header as usize))
                    / std::mem::size_of::<RawFd>();
                for index in 0..count {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(index));
                    if RECV_FLAGS == 0 {
                        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    }
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("too many descriptors in one frame"));
    }
    Ok((read, fds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn frames_carry_descriptors() {
        let (left, right) = UnixStream::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        send(
            &left,
            &Request::Signal { signal: 2 },
            &[file.as_fd().as_raw_fd()],
        )
        .unwrap();
        send(&left, &Request::ProfileList, &[]).unwrap();
        let (first, fds) = recv::<Request>(&right).unwrap().unwrap();
        assert!(matches!(first, Request::Signal { signal: 2 }));
        assert_eq!(fds.len(), 1);
        let (second, fds) = recv::<Request>(&right).unwrap().unwrap();
        assert!(matches!(second, Request::ProfileList));
        assert!(fds.is_empty());
        drop(left);
        assert!(recv::<Request>(&right).unwrap().is_none());
    }
}
