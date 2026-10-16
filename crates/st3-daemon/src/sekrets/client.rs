//! The caller's side: connect to the gateway, say who we are, pass our standard streams and
//! checkout as descriptors, and relay a terminal when the command runs on one.

use std::fs::File;
use std::io::{self, IsTerminal as _, Read as _, Write as _};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::protocol::{self, Attestation, CallerView, Reply, Request, RunRequest, Winsize};

/// Where a command's standard streams go.
pub enum Streams {
    /// A terminal the gateway makes, relayed to this one.
    Terminal,
    /// These descriptors, passed to the command as they are.
    Fds([RawFd; 3]),
}

pub struct Connection {
    stream: UnixStream,
    pub caller: CallerView,
    pub nonce: String,
    pub gateway: String,
}

/// Where the gateway listens: `ST_SEKRETS_SOCKET`, else the default.
pub fn socket_path() -> PathBuf {
    std::env::var_os("ST_SEKRETS_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| super::gateway::DEFAULT_SOCKET.into())
}

impl Connection {
    pub fn open(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket).with_context(|| {
            format!(
                "sekrets is not set up on this host: no gateway at {} (see `st sekrets setup`)",
                socket.display()
            )
        })?;
        let mut connection = Self {
            stream,
            caller: CallerView::Unidentified {
                person: String::new(),
                reason: String::new(),
            },
            nonce: String::new(),
            gateway: String::new(),
        };
        connection.hello(None)?;
        Ok(connection)
    }

    /// Say hello again with a daemon's attestation, which the gateway checks against this process.
    pub fn hello(&mut self, attestation: Option<Attestation>) -> Result<()> {
        match self.request(&Request::Hello { attestation }, &[])? {
            Reply::Hello {
                caller,
                nonce,
                gateway,
            } => {
                self.caller = caller;
                self.nonce = nonce;
                self.gateway = gateway;
                Ok(())
            }
            other => bail!("unexpected reply from the gateway: {other:?}"),
        }
    }

    /// One request and its reply.
    pub(crate) fn call(&self, request: &Request) -> Result<Reply> {
        self.request(request, &[])
    }

    fn request(&self, request: &Request, fds: &[RawFd]) -> Result<Reply> {
        protocol::send(&self.stream, request, fds).context("send to the sekrets gateway")?;
        let Some((reply, _)) =
            protocol::recv::<Reply>(&self.stream).context("read from the sekrets gateway")?
        else {
            bail!("the sekrets gateway closed the connection");
        };
        if let Reply::Error { message } = &reply {
            bail!("sekrets gateway: {message}");
        }
        Ok(reply)
    }

    /// A management request; a refusal is an error with the gateway's reason.
    pub fn manage(&self, request: &Request) -> Result<serde_json::Value> {
        match self.request(request, &[])? {
            Reply::Ok { value } => Ok(value),
            Reply::Refused { reason, .. } => bail!("refused: {reason}"),
            other => bail!("unexpected reply from the gateway: {other:?}"),
        }
    }

    /// Run a command through the gateway and return its exit code: on this terminal when both
    /// standard input and output are one, else with this process's standard streams.
    pub fn run(self, run: RunRequest) -> Result<i32> {
        let cwd = std::env::current_dir().context("read the working directory")?;
        let streams = if io::stdin().is_terminal() && io::stdout().is_terminal() {
            Streams::Terminal
        } else {
            Streams::Fds([0, 1, 2])
        };
        self.run_in(run, &cwd, streams)
    }

    pub fn run_in(self, mut run: RunRequest, cwd: &Path, streams: Streams) -> Result<i32> {
        let directories = checkout_directories(cwd)?;
        run.directories = directories.len();
        let files = super::files::pass_files(&mut run.argv, cwd)?;
        run.files = files.len();
        if 3 + directories.len() + files.len() > protocol::MAX_FDS {
            bail!(
                "a command can read at most {} passed files",
                protocol::MAX_FDS - 3 - directories.len()
            );
        }
        let interactive = matches!(streams, Streams::Terminal);
        if interactive {
            run.tty = Some(terminal_size());
            run.term = std::env::var("TERM").ok();
        }
        let mut fds: Vec<RawFd> = Vec::new();
        if let Streams::Fds(stdio) = streams {
            fds.extend(stdio);
        }
        fds.extend(directories.iter().map(|d| d.as_raw_fd()));
        fds.extend(files.iter().map(|f| f.as_raw_fd()));
        protocol::send(&self.stream, &Request::Run(run), &fds)
            .context("send to the sekrets gateway")?;
        drop(directories);
        drop(files);
        let Some((reply, mut passed)) = protocol::recv::<Reply>(&self.stream)? else {
            bail!("the sekrets gateway closed the connection");
        };
        match reply {
            Reply::Started { note, .. } => {
                if let Some(note) = note {
                    eprintln!("st sekrets: {note}");
                }
            }
            Reply::Refused { reason, .. } => bail!("refused: {reason}"),
            Reply::Error { message } => bail!("sekrets gateway: {message}"),
            other => bail!("unexpected reply from the gateway: {other:?}"),
        }
        let relay = if interactive {
            let master = passed
                .pop()
                .context("the gateway started a terminal command without its terminal")?;
            Some(spawn_terminal_relay(master)?)
        } else {
            if fds.starts_with(&[0, 1, 2]) {
                forward_interrupts(&self.stream)?;
            }
            None
        };
        let exit = protocol::recv::<Reply>(&self.stream);
        if let Some(relay) = relay {
            relay.finish();
        }
        match exit? {
            Some((Reply::Exited { code, signal }, _)) => {
                Ok(code.unwrap_or_else(|| 128 + signal.unwrap_or(1)))
            }
            Some((Reply::Error { message }, _)) => bail!("sekrets gateway: {message}"),
            Some((other, _)) => bail!("unexpected reply from the gateway: {other:?}"),
            None => bail!("the sekrets gateway closed the connection before the command ended"),
        }
    }
}

/// The working directory and, in a git checkout, its git directories, opened as descriptors the
/// gateway binds into the sandbox. The working directory comes first.
fn checkout_directories(cwd: &Path) -> Result<Vec<OwnedFd>> {
    let open = |path: &Path| -> Result<OwnedFd> {
        Ok(File::open(path)
            .with_context(|| format!("open {}", path.display()))?
            .into())
    };
    let mut paths = vec![cwd.to_path_buf()];
    for level in cwd.ancestors() {
        let entry = level.join(".git");
        if entry.is_dir() {
            paths.push(level.to_path_buf());
            break;
        }
        if entry.is_file() {
            paths.push(level.to_path_buf());
            let text = std::fs::read_to_string(&entry)?;
            if let Some(named) = text.trim().strip_prefix("gitdir:") {
                let git_dir = level.join(named.trim());
                if let Ok(common) = std::fs::read_to_string(git_dir.join("commondir")) {
                    paths.push(git_dir.join(common.trim()));
                } else {
                    paths.push(git_dir);
                }
            }
            break;
        }
    }
    let mut seen = Vec::new();
    let mut fds = Vec::new();
    for path in paths {
        let canonical = std::fs::canonicalize(&path)?;
        if seen.contains(&canonical) {
            continue;
        }
        fds.push(open(&canonical)?);
        seen.push(canonical);
    }
    Ok(fds)
}

fn terminal_size() -> Winsize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_row > 0 {
        Winsize {
            rows: size.ws_row,
            cols: size.ws_col,
        }
    } else {
        Winsize { rows: 24, cols: 80 }
    }
}

/// An interrupt forwarded to the gateway stops the command, as it would a local one.
fn forward_interrupts(stream: &UnixStream) -> Result<()> {
    let stream = stream.try_clone()?;
    let mut signals = SignalPipe::install(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP])?;
    std::thread::spawn(move || {
        while let Some(signal) = signals.next() {
            let _ = protocol::send(&stream, &Request::Signal { signal }, &[]);
        }
    });
    Ok(())
}

struct TerminalRelay {
    restore: Option<libc::termios>,
    master: RawFd,
    output: std::thread::JoinHandle<()>,
    _master: OwnedFd,
}

impl TerminalRelay {
    fn finish(self) {
        let _ = self.output.join();
        if let Some(saved) = self.restore {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        }
        let _ = self.master;
    }
}

fn spawn_terminal_relay(master: OwnedFd) -> Result<TerminalRelay> {
    let master_fd = master.as_raw_fd();
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let restore = if unsafe { libc::tcgetattr(0, &mut saved) } == 0 {
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) };
        Some(saved)
    } else {
        None
    };
    // Keyboard to the command; the thread ends with the process.
    let input_fd = unsafe { libc::dup(master_fd) };
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        let mut stdin = io::stdin().lock();
        loop {
            let Ok(read) = stdin.read(&mut buffer) else {
                return;
            };
            if read == 0 {
                return;
            }
            let written = unsafe { libc::write(input_fd, buffer.as_ptr().cast(), read) };
            if written < 0 {
                return;
            }
        }
    });
    // Resize the command's terminal with ours.
    let mut winch = SignalPipe::install(&[libc::SIGWINCH])?;
    std::thread::spawn(move || {
        while winch.next().is_some() {
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0 {
                unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &size) };
            }
        }
    });
    let output_fd = unsafe { libc::dup(master_fd) };
    let output = std::thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let mut stdout = io::stdout().lock();
        loop {
            let read = unsafe { libc::read(output_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read <= 0 {
                unsafe { libc::close(output_fd) };
                return;
            }
            if stdout.write_all(&buffer[..read as usize]).is_err() {
                return;
            }
            let _ = stdout.flush();
        }
    });
    Ok(TerminalRelay {
        restore,
        master: master_fd,
        output,
        _master: master,
    })
}

/// Signals delivered as bytes on a pipe, read from a thread.
struct SignalPipe {
    read: File,
}

static SIGNAL_WRITE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = SIGNAL_WRITE.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

impl SignalPipe {
    fn install(signals: &[libc::c_int]) -> Result<Self> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        // One pipe at a time: a run relays either a terminal or interrupts, never both.
        SIGNAL_WRITE.store(fds[1], std::sync::atomic::Ordering::SeqCst);
        for signal in signals {
            unsafe { libc::signal(*signal, on_signal as *const () as libc::sighandler_t) };
        }
        Ok(Self {
            read: unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fds[0]) },
        })
    }

    fn next(&mut self) -> Option<i32> {
        let mut byte = [0_u8; 1];
        match self.read.read(&mut byte) {
            Ok(1) => Some(i32::from(byte[0])),
            _ => None,
        }
    }
}
