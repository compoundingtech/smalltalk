//! Opt-in container runtime contract. No host process or raw st socket fallback.
//!
//! The deployment owns the fixed container unit, private daemon and native readiness proof.
//! This module transports lifecycle metadata only. It does not provision containers or grant
//! account authority. See `docs/st3/container-runtime.md` before implementing a controller.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

const MAX_FRAME: usize = 4096;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn is_seat_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_' | b'.')
                })
        })
}

fn deserialize_seat<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if is_seat_identifier(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom("invalid seat identifier"))
    }
}

/// Container selection is explicit and cannot degrade to native execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SeatRuntime {
    Native {},
    Container(ContainerRuntime),
}

/// One trusted, deployment-owned socket, not the host daemon's generic endpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerRuntime {
    #[serde(deserialize_with = "deserialize_seat")]
    pub seat: String,
    pub endpoint: PathBuf,
    /// Bridge UID as observed in the client's user namespace. It must be explicitly mapped,
    /// not an overflow UID shared by unrelated host identities.
    pub bridge_uid: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerPhase {
    Stopped,
    Starting,
    Ready,
    Quiescing,
    Suspended,
    Failed,
}

/// This is the entire host-visible status. Do not add free-form diagnostics or native content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerStatus {
    pub generation: u64,
    pub phase: ContainerPhase,
    pub ready: bool,
    pub in_flight_writes: u32,
}

impl ContainerStatus {
    fn valid(self) -> bool {
        self.ready == (self.phase == ContainerPhase::Ready)
            && (!matches!(self.phase, ContainerPhase::Stopped | ContainerPhase::Suspended)
                || self.in_flight_writes == 0)
    }
}

/// Suspend commits a private handover then destroys volatile state. Resume starts a fresh native
/// session that reads that handover first, not a transcript resume. Handover bytes never cross RPC.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContainerOperation {
    Status,
    Start,
    Stop,
    Suspend,
    Resume,
    /// The adapter must verify the native channel binding, not trust the notification alone.
    NativeReady { incarnation: u64 },
}

impl<'de> Deserialize<'de> for ContainerOperation {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // Internally tagged unit variants ignore extra fields. Empty struct variants do not.
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Status {},
            Start {},
            Stop {},
            Suspend {},
            Resume {},
            NativeReady { incarnation: u64 },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Status {} => Self::Status,
            Wire::Start {} => Self::Start,
            Wire::Stop {} => Self::Stop,
            Wire::Suspend {} => Self::Suspend,
            Wire::Resume {} => Self::Resume,
            Wire::NativeReady { incarnation } => Self::NativeReady { incarnation },
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerRequest {
    pub version: u8,
    #[serde(deserialize_with = "deserialize_seat")]
    pub seat: String,
    pub generation: u64,
    pub operation: ContainerOperation,
}

/// Closed codes keep command output, paths, credentials and content out of the bridge.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerError {
    Forbidden,
    GenerationConflict,
    InvalidTransition,
    NotQuiescent,
    Unavailable,
    InvalidObservation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContainerResponse {
    Status { status: ContainerStatus },
    Error { code: ContainerError },
}

/// Pin each listener to exactly one role and mapped Unix UID. Never accept these from a frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerBridgeRole {
    Controller,
    Seat,
}

/// The host adapter is a capability boundary, not a generic command runner.
///
/// Calls are serialized by the listener. Implementations must reserve generations before start,
/// verify native readiness, and fence dispatch before quiescence. Suspend must commit working
/// context, open tasks and next steps in private persistent storage before teardown. Resume must
/// gate readiness on the fresh session's first read of that committed handover. Stop/suspend
/// acknowledge only after the whole unit is empty and its volatile state erased. Handover bytes
/// must never enter this interface. Errors must use closed codes.
/// A failed mutation must remain observable through `status`, never masquerade as success.
pub trait ContainerControl {
    fn status(&mut self) -> std::result::Result<ContainerStatus, ContainerError>;
    fn apply(
        &mut self,
        generation: u64,
        operation: ContainerOperation,
    ) -> std::result::Result<ContainerStatus, ContainerError>;
}

impl ContainerRuntime {
    /// Authenticate the server before sending even the metadata request.
    pub fn request(
        &self,
        generation: u64,
        operation: ContainerOperation,
    ) -> Result<ContainerResponse> {
        anyhow::ensure!(is_seat_identifier(&self.seat), "invalid seat identifier");
        let stream = UnixStream::connect(&self.endpoint).context("connect container bridge")?;
        let deadline = Instant::now() + IO_TIMEOUT;
        authenticate(&stream, self.bridge_uid)?;
        let mut stream = DeadlineStream { stream, deadline };
        write_frame(
            &mut stream,
            &ContainerRequest {
                version: 1,
                seat: self.seat.clone(),
                generation,
                operation,
            },
        )?;
        let response = read_frame(&mut stream)?;
        if let ContainerResponse::Status { status } = response {
            anyhow::ensure!(status.valid(), "invalid container bridge observation");
            anyhow::ensure!(
                matches!(operation, ContainerOperation::Status)
                    || observation_matches(generation, operation, status),
                "container bridge returned an unfenced observation"
            );
        }
        Ok(response)
    }
}

/// Serve one bounded request on an already accepted socket. The caller owns a private listener
/// and serializes access to its fixed control adapter. This never contacts the raw st endpoint.
pub fn serve_container_request(
    stream: UnixStream,
    expected_uid: u32,
    seat: &str,
    role: ContainerBridgeRole,
    control: &mut impl ContainerControl,
) -> Result<()> {
    let deadline = Instant::now() + IO_TIMEOUT;
    authenticate(&stream, expected_uid)?;
    let mut stream = DeadlineStream { stream, deadline };
    let request: ContainerRequest = read_frame(&mut stream)?;
    let response = match dispatch(&request, seat, role, control) {
        Ok(status) => ContainerResponse::Status { status },
        Err(code) => ContainerResponse::Error { code },
    };
    write_frame(&mut stream, &response)
}

fn dispatch(
    request: &ContainerRequest,
    seat: &str,
    role: ContainerBridgeRole,
    control: &mut impl ContainerControl,
) -> std::result::Result<ContainerStatus, ContainerError> {
    if request.version != 1
        || !is_seat_identifier(seat)
        || request.seat != seat
        || (role == ContainerBridgeRole::Seat
            && !matches!(request.operation, ContainerOperation::Status | ContainerOperation::NativeReady { .. }))
        || (role == ContainerBridgeRole::Controller
            && matches!(request.operation, ContainerOperation::NativeReady { .. }))
    {
        return Err(ContainerError::Forbidden);
    }
    let current = control.status()?;
    if !current.valid() {
        return Err(ContainerError::InvalidObservation);
    }
    if request.operation == ContainerOperation::Status {
        return Ok(current);
    }
    if request.generation != current.generation {
        return Err(ContainerError::GenerationConflict);
    }
    let permitted = match request.operation {
        ContainerOperation::Start => current.phase == ContainerPhase::Stopped,
        ContainerOperation::Resume => current.phase == ContainerPhase::Suspended,
        ContainerOperation::Suspend => current.phase == ContainerPhase::Ready,
        ContainerOperation::NativeReady { incarnation } => {
            incarnation != 0 && current.phase == ContainerPhase::Starting
        }
        ContainerOperation::Stop => true,
        ContainerOperation::Status => unreachable!(),
    };
    if !permitted {
        return Err(ContainerError::InvalidTransition);
    }
    if request.operation == ContainerOperation::Suspend && current.in_flight_writes != 0 {
        return Err(ContainerError::NotQuiescent);
    }
    let status = control.apply(request.generation, request.operation)?;
    if !status.valid() || !observation_matches(request.generation, request.operation, status) {
        return Err(ContainerError::InvalidObservation);
    }
    Ok(status)
}

fn observation_matches(
    generation: u64,
    operation: ContainerOperation,
    status: ContainerStatus,
) -> bool {
    match operation {
        ContainerOperation::Start | ContainerOperation::Resume => {
            generation.checked_add(1) == Some(status.generation)
                && matches!(status.phase, ContainerPhase::Starting | ContainerPhase::Ready)
        }
        ContainerOperation::Stop => status.generation == generation && status.phase == ContainerPhase::Stopped,
        ContainerOperation::Suspend => status.generation == generation && status.phase == ContainerPhase::Suspended,
        ContainerOperation::NativeReady { .. } => status.generation == generation && status.phase == ContainerPhase::Ready,
        ContainerOperation::Status => true,
    }
}

/// One absolute I/O deadline, not an inactivity timeout that a slow peer can extend.
struct DeadlineStream {
    stream: UnixStream,
    deadline: Instant,
}

impl DeadlineStream {
    fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "container bridge I/O deadline exceeded",
            ))
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.remaining()?;
        self.stream.flush()
    }
}

#[cfg(target_os = "linux")]
fn authenticate(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    use std::os::fd::AsRawFd as _;
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials points to writable storage of `length` bytes. getsockopt initializes
    // it on success; both return code and returned length are checked before assume_init.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("authenticate container bridge peer");
    }
    anyhow::ensure!(
        length as usize == std::mem::size_of::<libc::ucred>(),
        "invalid container bridge peer credentials"
    );
    // SAFETY: the successful call and exact length check above establish initialization.
    let credentials = unsafe { credentials.assume_init() };
    anyhow::ensure!(credentials.uid == expected_uid, "container bridge peer refused");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn authenticate(_stream: &UnixStream, _expected_uid: u32) -> Result<()> {
    anyhow::bail!("container bridge requires Linux peer authentication")
}

fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(bytes.len() <= MAX_FRAME, "container bridge frame too large");
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut impl Read) -> Result<T> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(length <= MAX_FRAME, "container bridge frame too large");
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    // Do not include serde's error: malformed enum values can contain request content.
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid container bridge frame"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Control {
        status: ContainerStatus,
        mutations: usize,
    }

    impl ContainerControl for Control {
        fn status(&mut self) -> std::result::Result<ContainerStatus, ContainerError> {
            Ok(self.status)
        }

        fn apply(&mut self, generation: u64, operation: ContainerOperation) -> std::result::Result<ContainerStatus, ContainerError> {
            self.mutations += 1;
            self.status = ContainerStatus {
                generation: generation + u64::from(matches!(operation, ContainerOperation::Start | ContainerOperation::Resume)),
                phase: match operation {
                    ContainerOperation::Start | ContainerOperation::Resume => ContainerPhase::Starting,
                    ContainerOperation::NativeReady { .. } => ContainerPhase::Ready,
                    ContainerOperation::Suspend => ContainerPhase::Suspended,
                    ContainerOperation::Stop => ContainerPhase::Stopped,
                    ContainerOperation::Status => unreachable!(),
                },
                ready: matches!(operation, ContainerOperation::NativeReady { .. }),
                in_flight_writes: 0,
            };
            Ok(self.status)
        }
    }

    fn control() -> Control {
        Control {
            status: ContainerStatus { generation: 0, phase: ContainerPhase::Stopped, ready: false, in_flight_writes: 0 },
            mutations: 0,
        }
    }

    fn request(generation: u64, operation: ContainerOperation) -> ContainerRequest {
        ContainerRequest { version: 1, seat: "synthetic".into(), generation, operation }
    }

    fn call(control: &mut Control, role: ContainerBridgeRole, operation: ContainerOperation) -> ContainerStatus {
        dispatch(&request(control.status.generation, operation), "synthetic", role, control).unwrap()
    }

    #[test]
    fn lifecycle_uses_a_new_generation_for_cold_resume() {
        let mut c = control();
        assert!(!call(&mut c, ContainerBridgeRole::Controller, ContainerOperation::Start).ready);
        assert!(call(&mut c, ContainerBridgeRole::Seat, ContainerOperation::NativeReady { incarnation: 7 }).ready);
        assert_eq!(call(&mut c, ContainerBridgeRole::Controller, ContainerOperation::Suspend).phase, ContainerPhase::Suspended);
        assert_eq!(call(&mut c, ContainerBridgeRole::Controller, ContainerOperation::Resume).generation, 2);
        assert_eq!(dispatch(&request(1, ContainerOperation::NativeReady { incarnation: 7 }), "synthetic", ContainerBridgeRole::Seat, &mut c), Err(ContainerError::GenerationConflict));
        call(&mut c, ContainerBridgeRole::Seat, ContainerOperation::NativeReady { incarnation: 8 });
        assert_eq!(call(&mut c, ContainerBridgeRole::Controller, ContainerOperation::Stop).phase, ContainerPhase::Stopped);
    }

    #[test]
    fn foreign_seats_and_roles_never_reach_the_adapter() {
        let mut c = control();
        assert_eq!(dispatch(&request(0, ContainerOperation::Status), "other", ContainerBridgeRole::Controller, &mut c), Err(ContainerError::Forbidden));
        for operation in [ContainerOperation::Start, ContainerOperation::Stop, ContainerOperation::Suspend, ContainerOperation::Resume] {
            assert_eq!(dispatch(&request(0, operation), "synthetic", ContainerBridgeRole::Seat, &mut c), Err(ContainerError::Forbidden));
        }
        assert_eq!(dispatch(&request(0, ContainerOperation::NativeReady { incarnation: 1 }), "synthetic", ContainerBridgeRole::Controller, &mut c), Err(ContainerError::Forbidden));
        assert_eq!(c.mutations, 0);
    }

    #[test]
    fn suspend_refuses_in_flight_writes_and_ready_requires_starting() {
        let mut c = control();
        assert_eq!(dispatch(&request(0, ContainerOperation::NativeReady { incarnation: 1 }), "synthetic", ContainerBridgeRole::Seat, &mut c), Err(ContainerError::InvalidTransition));
        call(&mut c, ContainerBridgeRole::Controller, ContainerOperation::Start);
        call(&mut c, ContainerBridgeRole::Seat, ContainerOperation::NativeReady { incarnation: 1 });
        c.status.in_flight_writes = 1;
        assert_eq!(dispatch(&request(1, ContainerOperation::Suspend), "synthetic", ContainerBridgeRole::Controller, &mut c), Err(ContainerError::NotQuiescent));
        assert_eq!(c.mutations, 2);
    }

    #[test]
    fn schemas_reject_content_and_generic_apis() {
        for json in [
            r#"{"version":1,"seat":"synthetic","generation":0,"operation":{"kind":"exec","argv":["sh"]}}"#,
            r#"{"version":1,"seat":"synthetic","generation":0,"operation":{"kind":"status"},"content":"not allowed"}"#,
            r#"{"version":1,"seat":"free text is forbidden","generation":0,"operation":{"kind":"status"}}"#,
            r#"{"version":1,"seat":"synthetic","generation":0,"operation":{"kind":"status","content":"not allowed"}}"#,
            r#"{"version":1,"seat":"synthetic","generation":0,"operation":{"kind":"start","argv":["sh"]}}"#,
        ] {
            assert!(serde_json::from_str::<ContainerRequest>(json).is_err());
        }
        assert!(!is_seat_identifier(&"a".repeat(257)));
        assert!(!is_seat_identifier("agent/../other"));
        let status = ContainerResponse::Status { status: control().status };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &status).unwrap();
        assert_eq!(read_frame::<ContainerResponse>(&mut bytes.as_slice()).unwrap(), status);
        assert!(read_frame::<ContainerRequest>(&mut ((MAX_FRAME as u32 + 1).to_be_bytes().as_slice())).is_err());
    }

    #[test]
    fn runtime_selection_round_trips_without_a_native_fallback() {
        let runtime = SeatRuntime::Container(ContainerRuntime { seat: "synthetic".into(), endpoint: "/run/synthetic/control.sock".into(), bridge_uid: 123 });
        assert_eq!(serde_json::from_slice::<SeatRuntime>(&serde_json::to_vec(&runtime).unwrap()).unwrap(), runtime);
        assert!(serde_json::from_str::<SeatRuntime>(r#"{"kind":"unknown"}"#).is_err());
        assert!(serde_json::from_str::<SeatRuntime>(r#"{"kind":"native","content":"not allowed"}"#).is_err());
    }

    #[test]
    fn expired_deadline_refuses_reads_and_writes_even_with_bytes_available() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(&[1]).unwrap();
        let mut stream = DeadlineStream { stream, deadline: Instant::now() };
        assert_eq!(stream.read(&mut [0]).unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(stream.write(&[1]).unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unix_transport_authenticates_before_dispatch() {
        let (mut client, server) = UnixStream::pair().unwrap();
        // SAFETY: getuid has no arguments or memory preconditions.
        let uid = unsafe { libc::getuid() };
        let thread = std::thread::spawn(move || serve_container_request(server, uid, "synthetic", ContainerBridgeRole::Controller, &mut control()));
        authenticate(&client, uid).unwrap();
        write_frame(&mut client, &request(0, ContainerOperation::Start)).unwrap();
        let response: ContainerResponse = read_frame(&mut client).unwrap();
        assert!(matches!(response, ContainerResponse::Status { status: ContainerStatus { generation: 1, phase: ContainerPhase::Starting, .. } }));
        thread.join().unwrap().unwrap();
        let (_, server) = UnixStream::pair().unwrap();
        assert!(serve_container_request(server, uid.wrapping_add(1), "synthetic", ContainerBridgeRole::Controller, &mut control()).is_err());
    }
}
