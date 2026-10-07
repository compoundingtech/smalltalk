//! The gateway end to end, in this process: a real socket, the real sandbox and real git, with
//! the kernel's word about the caller's cgroup stood in for. The isolation VM proves the kernel
//! part with real Unix users and login sessions.

use std::fs;
use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use smallclaims::fleet::MemberKey;

use super::client::{Connection, Streams};
use super::gateway::{Gateway, GatewayConfig, Kernel};
use super::identity::{self, STATEMENT_VERSION, Statement};
use super::policy::Policy;
use super::protocol::{Attestation, CallerView, Request, RunRequest};

struct FakeKernel {
    cgroup: Mutex<String>,
}

struct SharedKernel(Arc<FakeKernel>);

impl Kernel for SharedKernel {
    fn cgroup(&self, _pid: i32) -> Option<String> {
        Some(self.0.cgroup.lock().unwrap().clone())
    }
    fn start(&self, pid: i32) -> Option<u64> {
        identity::process_start(pid)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    socket: PathBuf,
    store: PathBuf,
    tools: PathBuf,
    checkouts: PathBuf,
    kernel: Arc<FakeKernel>,
    uid: u32,
}

/// The sandbox needs bubblewrap and user namespaces; without them these tests have nothing to
/// prove here, and the isolation VM still runs them.
fn sandbox_available() -> bool {
    Path::new("/usr/bin/bwrap").exists()
        && Command::new("/usr/bin/bwrap")
            .args(["--ro-bind", "/", "/", "--unshare-pid", "true"])
            .status()
            .is_ok_and(|status| status.success())
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let uid = unsafe { libc::getuid() };
        let store = root.join("store");
        fs::create_dir(&store).unwrap();
        fs::set_permissions(&store, fs::Permissions::from_mode(0o700)).unwrap();
        let tools = root.join("tools");
        fs::create_dir(&tools).unwrap();
        fs::set_permissions(&tools, fs::Permissions::from_mode(0o755)).unwrap();
        let checkouts = root.join("checkouts");
        fs::create_dir(&checkouts).unwrap();
        let run = root.join("run");
        fs::create_dir(&run).unwrap();
        let socket = run.join("gateway.sock");
        let config: GatewayConfig = toml::from_str(&format!(
            "socket = {socket:?}\nstore = {store:?}\npath = [{tools:?}, \"/usr/bin\", \"/bin\"]\n\
             checkout_roots = [{checkouts:?}]\n[people]\n\"{uid}\" = \"person/ada\"\n",
        ))
        .unwrap();
        let kernel = Arc::new(FakeKernel {
            cgroup: Mutex::new(String::new()),
        });
        let gateway = Arc::new(
            Gateway::new(
                config,
                uid,
                None,
                Box::new(SharedKernel(Arc::clone(&kernel))),
            )
            .unwrap(),
        );
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || gateway.serve_listener(listener));
        let fixture = Self {
            _dir: dir,
            root,
            socket,
            store,
            tools,
            checkouts,
            kernel,
            uid,
        };
        fixture.as_person();
        fixture
    }

    fn as_person(&self) {
        *self.kernel.cgroup.lock().unwrap() =
            format!("/user.slice/user-{}.slice/session-1.scope", self.uid);
    }

    fn seat_cgroup(&self) -> String {
        format!(
            "/user.slice/user-{0}.slice/user@{0}.service/app.slice/st3-seat-1.scope",
            self.uid
        )
    }

    fn as_seat(&self) {
        *self.kernel.cgroup.lock().unwrap() = self.seat_cgroup();
    }

    fn tool(&self, name: &str, script: &str) {
        let path = self.tools.join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn connect(&self) -> Connection {
        Connection::open(&self.socket).unwrap()
    }

    fn manage(&self, request: Request) -> anyhow::Result<serde_json::Value> {
        self.connect().manage(&request)
    }

    /// A connection that brings an attestation for `agent`, signed by `key`.
    fn seat(&self, key: &MemberKey, agent: &str) -> Connection {
        self.as_seat();
        let mut connection = self.connect();
        let pid = std::process::id() as i32;
        let statement = Statement {
            version: STATEMENT_VERSION,
            node: "host/example".into(),
            person: "person/ada".into(),
            agent: agent.into(),
            revision: None,
            cgroup: self.seat_cgroup(),
            pid,
            pid_start: identity::process_start(pid).unwrap(),
            nonce: connection.nonce.clone(),
            issued_at_unix_ms: super::store::now_unix_ms(),
        };
        let text = serde_json::to_string(&statement).unwrap();
        let signature = key.sign(&identity::signing_message(&text));
        connection
            .hello(Some(Attestation {
                statement: text,
                signature,
            }))
            .unwrap();
        connection
    }

    fn checkout(&self, name: &str) -> PathBuf {
        let path = self.checkouts.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

struct Output {
    result: anyhow::Result<i32>,
    stdout: String,
    stderr: String,
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn run(connection: Connection, profile: Option<&str>, argv: &[&str], cwd: &Path) -> Output {
    run_request(
        connection,
        RunRequest {
            profile: profile.map(str::to_owned),
            argv: argv.iter().map(|word| (*word).to_owned()).collect(),
            ..RunRequest::default()
        },
        cwd,
    )
}

fn run_request(connection: Connection, request: RunRequest, cwd: &Path) -> Output {
    let stdin: OwnedFd = fs::File::open("/dev/null").unwrap().into();
    let (out_read, out_write) = pipe();
    let (err_read, err_write) = pipe();
    let result = connection.run_in(
        request,
        cwd,
        Streams::Fds([
            stdin.as_raw_fd(),
            out_write.as_raw_fd(),
            err_write.as_raw_fd(),
        ]),
    );
    drop((out_write, err_write));
    let mut stdout = String::new();
    fs::File::from(out_read)
        .read_to_string(&mut stdout)
        .unwrap();
    let mut stderr = String::new();
    fs::File::from(err_read)
        .read_to_string(&mut stderr)
        .unwrap();
    Output {
        result,
        stdout,
        stderr,
    }
}

fn policy(presets: &[&str], allow: &[&str]) -> Policy {
    Policy::build(
        &presets.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>(),
        &allow.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>(),
        &[],
    )
    .unwrap()
}

fn refusal(output: &Output) -> String {
    match &output.result {
        Err(error) => format!("{error:#}"),
        Ok(code) => panic!("ran with exit {code}: {}{}", output.stdout, output.stderr),
    }
}

#[test]
fn a_person_runs_their_profile_without_seeing_the_store_or_other_profiles() {
    if !sandbox_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture
        .manage(Request::ProfileCreate {
            profile: "ada/gh".into(),
            description: None,
            policy: policy(&["everything", "no-credential-printing"], &[]),
            default: true,
        })
        .unwrap();
    fixture
        .manage(Request::ProfileCreate {
            profile: "ada/other".into(),
            description: None,
            policy: policy(&["everything"], &[]),
            default: false,
        })
        .unwrap();
    fixture
        .manage(Request::Put {
            profile: "ada/gh".into(),
            name: "EXAMPLE_TOKEN".into(),
            value: "example-value".into(),
        })
        .unwrap();
    let other_home = fixture.store.join("profiles/ada/other/home");
    fs::write(other_home.join("secret"), "other-profile-secret").unwrap();
    fixture.tool(
        "probe",
        "echo \"token=$EXAMPLE_TOKEN home=$HOME\"\n\
         touch \"$HOME/written\" && echo home-writable\n\
         cat \"$1\" 2>/dev/null && echo STORE-LEAK || echo store-hidden\n\
         cat \"$2\" 2>/dev/null && echo OTHER-LEAK || echo other-hidden\n\
         pwd",
    );
    let cwd = fixture.checkout("web");
    let output = run(
        fixture.connect(),
        None,
        &[
            "probe",
            fixture.store.join("sekrets.db").to_str().unwrap(),
            other_home.join("secret").to_str().unwrap(),
        ],
        &cwd,
    );
    assert_eq!(output.result.as_ref().unwrap(), &0, "{}", output.stderr);
    let home = fixture.store.join("profiles/ada/gh/home");
    assert!(
        output
            .stdout
            .contains(&format!("token=example-value home={}", home.display())),
        "{}",
        output.stdout
    );
    assert!(output.stdout.contains("home-writable"));
    assert!(output.stdout.contains("store-hidden"), "{}", output.stdout);
    assert!(output.stdout.contains("other-hidden"), "{}", output.stdout);
    assert!(output.stdout.contains(cwd.to_str().unwrap()));
    assert!(home.join("written").exists());

    // From a directory the gateway does not serve, the command runs in the profile's home.
    fixture.tool("where", "pwd");
    let elsewhere = run(fixture.connect(), None, &["where"], &fixture.root);
    assert_eq!(
        elsewhere.result.as_ref().unwrap(),
        &0,
        "{}",
        elsewhere.stderr
    );
    assert_eq!(elsewhere.stdout.trim(), home.to_str().unwrap());

    fixture.tool("gh", "echo \"gh $*\"");
    let refused = run(fixture.connect(), None, &["gh", "auth", "token"], &cwd);
    assert!(
        refusal(&refused).contains("denied by rule `gh auth token`"),
        "{}",
        refusal(&refused)
    );
    let missing = run(fixture.connect(), None, &["no-such-tool"], &cwd);
    assert!(refusal(&missing).contains("not on the gateway's path"));
    let path = run(fixture.connect(), None, &["/bin/sh", "-c", "id"], &cwd);
    assert!(refusal(&path).contains("name a command, not a path"));

    let log = fixture
        .manage(Request::Log {
            after: 0,
            limit: 100,
            wait_ms: 0,
        })
        .unwrap();
    let entries = log.as_array().unwrap();
    let call = entries
        .iter()
        .find(|entry| entry["event"] == "call")
        .expect("the call is logged");
    assert_eq!(call["actor"], "person/ada");
    assert_eq!(call["detail"]["argv"][0], "probe");
    let exited = entries
        .iter()
        .find(|entry| entry["event"] == "exited")
        .expect("the exit is logged");
    assert_eq!(exited["detail"]["call"], call["seq"]);
    assert_eq!(exited["detail"]["code"], 0);
    assert!(
        entries
            .iter()
            .any(|entry| entry["event"] == "refused" && entry["detail"]["argv"][1] == "auth")
    );
    assert!(
        !serde_json::to_string(&log)
            .unwrap()
            .contains("example-value"),
        "the log never holds a value"
    );
}

#[test]
fn a_seat_uses_only_what_it_was_granted_and_only_with_its_daemons_word() {
    if !sandbox_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.tool("gh", "echo \"gh $*\"");
    fixture
        .manage(Request::ProfileCreate {
            profile: "ada/agent-gh".into(),
            description: None,
            policy: policy(&["gh-pr", "no-credential-printing"], &[]),
            default: false,
        })
        .unwrap();
    let (key, _) = MemberKey::generate().unwrap();
    let cwd = fixture.checkout("web");

    // Without the daemon's word a process in the service manager is nobody.
    fixture.as_seat();
    let anonymous = fixture.connect();
    assert!(matches!(anonymous.caller, CallerView::Unidentified { .. }));
    let refused = run(anonymous, None, &["gh", "pr", "list"], &cwd);
    assert!(
        refusal(&refused).contains("not identified"),
        "{}",
        refusal(&refused)
    );

    // An attestation counts only with a registered key.
    let unregistered = fixture.seat(&key, "agent/fleet/fixture-web/builder");
    assert!(
        matches!(&unregistered.caller, CallerView::Unidentified { reason, .. } if reason.contains("registered no daemon key")),
        "{:?}",
        unregistered.caller
    );
    // Only a person registers it.
    assert!(
        fixture
            .seat(&key, "agent/fleet/fixture-web/builder")
            .manage(&Request::Register {
                node: "host/example".into(),
                key: key.public().into(),
            })
            .is_err()
    );
    fixture.as_person();
    fixture
        .manage(Request::Register {
            node: "host/example".into(),
            key: key.public().into(),
        })
        .unwrap();
    let seat = fixture.seat(&key, "agent/fleet/fixture-web/builder");
    assert_eq!(
        seat.caller,
        CallerView::Agent {
            agent: "agent/fleet/fixture-web/builder".into(),
            person: "person/ada".into()
        }
    );
    // A seat does not get its person's profiles.
    let refused = run(seat, None, &["gh", "pr", "list"], &cwd);
    assert!(
        refusal(&refused).contains("owns no profile and has been granted none"),
        "{}",
        refusal(&refused)
    );
    // Another key's signature is no one's word.
    let (forged, _) = MemberKey::generate().unwrap();
    let forged = fixture.seat(&forged, "agent/fleet/fixture-web/builder");
    assert!(matches!(forged.caller, CallerView::Unidentified { .. }));

    // A grant to an agent lists subcommands, never a whole command.
    fixture.as_person();
    let broad = fixture.manage(Request::GrantAdd {
        profile: "ada/agent-gh".into(),
        to: "agent/fleet/fixture-web/**".into(),
        policy: policy(&[], &["gh"]),
        until_unix_ms: None,
    });
    assert!(format!("{:#}", broad.unwrap_err()).contains("allows a whole command"));
    fixture
        .manage(Request::GrantAdd {
            profile: "ada/agent-gh".into(),
            to: "agent/fleet/fixture-web/**".into(),
            policy: policy(&["gh-pr"], &[]),
            until_unix_ms: None,
        })
        .unwrap();
    let output = run(
        fixture.seat(&key, "agent/fleet/fixture-web/builder"),
        None,
        &["gh", "pr", "create", "--draft", "--title", "Example"],
        &cwd,
    );
    assert_eq!(output.result.as_ref().unwrap(), &0, "{}", output.stderr);
    assert_eq!(output.stdout, "gh pr create --draft --title Example\n");
    for (argv, reason) in [
        (
            &["gh", "pr", "create", "--template", "x"][..],
            "option `--template` denied",
        ),
        (&["gh", "auth", "status"][..], "no allow rule matches"),
        (
            &["gh", "-R", "other/repo", "pr", "list"][..],
            "no allow rule matches",
        ),
    ] {
        let refused = run(
            fixture.seat(&key, "agent/fleet/fixture-web/builder"),
            None,
            argv,
            &cwd,
        );
        assert!(
            refusal(&refused).contains(reason),
            "{argv:?}: {}",
            refusal(&refused)
        );
    }
    // The grant covers its pattern only.
    let outside = run(
        fixture.seat(&key, "agent/fleet/fixture-app/builder"),
        None,
        &["gh", "pr", "list"],
        &cwd,
    );
    assert!(refusal(&outside).contains("granted none"));
    // A seat changes nothing.
    assert!(
        fixture
            .seat(&key, "agent/fleet/fixture-web/builder")
            .manage(&Request::PolicySet {
                profile: "ada/agent-gh".into(),
                policy: policy(&["everything"], &[]),
            })
            .is_err()
    );

    // A lock stops every use until the person lifts it.
    fixture.as_person();
    fixture
        .manage(Request::Lock {
            person: None,
            reason: Some("drill".into()),
        })
        .unwrap();
    let locked = run(
        fixture.seat(&key, "agent/fleet/fixture-web/builder"),
        None,
        &["gh", "pr", "list"],
        &cwd,
    );
    assert!(refusal(&locked).contains("locked for everyone by person/ada: drill"));
    fixture.as_person();
    fixture.manage(Request::Unlock { person: None }).unwrap();
    let unlocked = run(
        fixture.seat(&key, "agent/fleet/fixture-web/builder"),
        None,
        &["gh", "pr", "list"],
        &cwd,
    );
    assert_eq!(unlocked.result.unwrap(), 0);
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", cwd)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn git_in_a_checkout_runs_none_of_the_checkouts_programs() {
    if !sandbox_available() || !Path::new("/usr/bin/git").exists() {
        return;
    }
    let fixture = Fixture::new();
    fixture
        .manage(Request::ProfileCreate {
            profile: "ada/gh".into(),
            description: None,
            policy: policy(&[], &["git status", "git remote", "git log"]),
            default: true,
        })
        .unwrap();
    let repo = fixture.checkout("web");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "ada@example.com"]);
    git(&repo, &["config", "user.name", "Ada"]);
    git(
        &repo,
        &["remote", "add", "origin", "https://example.com/web.git"],
    );
    fs::write(repo.join("file.txt"), "one\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let worktree = fixture.checkouts.join("web-worktree");
    git(
        &repo,
        &["worktree", "add", "-q", worktree.to_str().unwrap()],
    );
    let script = |name: &str, body: &str| {
        let path = fixture.root.join(name);
        fs::write(&path, format!("#!/bin/sh\necho PWNED >&2\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let filter = script("filter.sh", "exec cat");
    let hook = script("hook.sh", "exit 0");
    // Every way the checkout's own configuration could run a program.
    git(&repo, &["config", "core.fsmonitor", &hook]);
    git(&repo, &["config", "filter.evil.clean", &filter]);
    git(&repo, &["config", "core.pager", &filter]);
    git(&repo, &["config", "pager.log", &filter]);
    fs::write(repo.join(".gitattributes"), "* filter=evil\n").unwrap();
    fs::write(worktree.join(".gitattributes"), "* filter=evil\n").unwrap();
    fs::write(repo.join("file.txt"), "two\n").unwrap();
    fs::write(worktree.join("file.txt"), "two\n").unwrap();
    fs::write(
        repo.join(".git/hooks/post-index-change"),
        format!("#!/bin/sh\n{hook}\n"),
    )
    .unwrap();
    fs::set_permissions(
        repo.join(".git/hooks/post-index-change"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    for cwd in [&repo, &worktree] {
        let status = run(
            fixture.connect(),
            None,
            &["git", "status", "--porcelain"],
            cwd,
        );
        assert_eq!(status.result.as_ref().unwrap(), &0, "{}", status.stderr);
        assert!(!status.stderr.contains("PWNED"), "{}", status.stderr);
        assert!(!status.stdout.contains("PWNED"), "{}", status.stdout);
        let remote = run(fixture.connect(), None, &["git", "remote", "-v"], cwd);
        assert!(
            remote.stdout.contains("https://example.com/web.git"),
            "{} {}",
            remote.stdout,
            remote.stderr
        );
        let log = run(fixture.connect(), None, &["git", "log", "--oneline"], cwd);
        assert!(
            log.stdout.contains("first"),
            "{} {}",
            log.stdout,
            log.stderr
        );
        assert!(!log.stderr.contains("PWNED"));
    }
    // A .git the gateway cannot sanitize, because it is a link or names a directory outside the
    // passed checkout, is refused before anything runs.
    let linked = fixture.checkout("linked");
    fs::rename(repo.join(".git"), linked.join("real-git")).unwrap();
    std::os::unix::fs::symlink("real-git", linked.join(".git")).unwrap();
    let refused = run(fixture.connect(), None, &["git", "status"], &linked);
    assert!(
        refusal(&refused).contains("is a symbolic link"),
        "{}",
        refusal(&refused)
    );
    // A gitdir outside every checkout root is never bound.
    let elsewhere = fixture.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::rename(linked.join("real-git"), elsewhere.join("real-git")).unwrap();
    let outside = fixture.checkout("outside");
    let gitfile = |target: &Path| {
        fs::write(
            outside.join(".git"),
            format!("gitdir: {}\n", target.display()),
        )
        .unwrap()
    };
    gitfile(&elsewhere.join("real-git"));
    let refused = run(fixture.connect(), None, &["git", "status"], &outside);
    assert!(
        refusal(&refused).contains("is not under a checkout root"),
        "{}",
        refusal(&refused)
    );
    // A caller that passes the checkout but not the git directory its gitdir file names gets
    // nothing run: the gateway cannot sanitize what it was not given.
    fs::rename(elsewhere.join("real-git"), linked.join("real-git")).unwrap();
    gitfile(&linked.join("real-git"));
    {
        use super::protocol::{self, Reply};
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(&fixture.socket).unwrap();
        protocol::send(&stream, &Request::Hello { attestation: None }, &[]).unwrap();
        let _ = protocol::recv::<Reply>(&stream).unwrap().unwrap();
        let null: OwnedFd = fs::File::open("/dev/null").unwrap().into();
        let directory: OwnedFd = fs::File::open(&outside).unwrap().into();
        protocol::send(
            &stream,
            &Request::Run(RunRequest {
                argv: vec!["git".into(), "status".into()],
                directories: 1,
                ..RunRequest::default()
            }),
            &[
                null.as_raw_fd(),
                null.as_raw_fd(),
                null.as_raw_fd(),
                directory.as_raw_fd(),
            ],
        )
        .unwrap();
        let (reply, _) = protocol::recv::<Reply>(&stream).unwrap().unwrap();
        let Reply::Refused { reason, .. } = reply else {
            panic!("ran: {reply:?}");
        };
        assert!(
            reason.contains("is not a directory inside the checkout"),
            "{reason}"
        );
    }
    fs::remove_file(linked.join(".git")).unwrap();
    fs::rename(linked.join("real-git"), repo.join(".git")).unwrap();

    // The checkout itself is untouched and still runs its own programs for its owner.
    let own = Command::new("git")
        .current_dir(&repo)
        .args(["config", "core.fsmonitor"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&own.stdout).trim(), hook);
}

#[test]
fn a_terminal_command_gets_a_controlling_terminal_whose_side_the_caller_holds() {
    if !sandbox_available() {
        return;
    }
    use super::protocol::{self, Reply, Winsize};
    use std::os::unix::net::UnixStream;
    let fixture = Fixture::new();
    fixture
        .manage(Request::ProfileCreate {
            profile: "ada/gh".into(),
            description: None,
            policy: policy(&["everything"], &[]),
            default: true,
        })
        .unwrap();
    fixture.tool(
        "login",
        "stty size\n[ -t 0 ] && echo has-terminal\necho via-dev-tty > /dev/tty\nread answer\necho \"answered $answer as $TERM\"",
    );
    let cwd = fixture.checkout("web");
    let directory: OwnedFd = fs::File::open(&cwd).unwrap().into();
    let stream = UnixStream::connect(&fixture.socket).unwrap();
    protocol::send(&stream, &Request::Hello { attestation: None }, &[]).unwrap();
    let _ = protocol::recv::<Reply>(&stream).unwrap().unwrap();
    protocol::send(
        &stream,
        &Request::Run(RunRequest {
            argv: vec!["login".into()],
            tty: Some(Winsize {
                rows: 33,
                cols: 101,
            }),
            term: Some("../../etc/passwd".into()),
            directories: 1,
            ..RunRequest::default()
        }),
        &[directory.as_raw_fd()],
    )
    .unwrap();
    let (started, mut fds) = protocol::recv::<Reply>(&stream).unwrap().unwrap();
    assert!(matches!(started, Reply::Started { .. }), "{started:?}");
    let master = fs::File::from(fds.pop().expect("the terminal's controlling side"));
    use std::io::Write as _;
    (&master).write_all(b"yes\n").unwrap();
    let mut output = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match (&master).read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => output.extend_from_slice(&buffer[..read]),
        }
    }
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("33 101"), "{output}");
    assert!(output.contains("has-terminal"), "{output}");
    assert!(output.contains("via-dev-tty"), "{output}");
    assert!(
        output.contains("answered yes as xterm-256color"),
        "{output}"
    );
    let (exited, _) = protocol::recv::<Reply>(&stream).unwrap().unwrap();
    assert!(
        matches!(exited, Reply::Exited { code: Some(0), .. }),
        "{exited:?}"
    );
}

#[test]
fn owners_manage_their_profiles_and_only_they_log_them_in() {
    if !sandbox_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.tool("gh", "echo \"gh $*\"");
    let cwd = fixture.checkout("web");
    fixture
        .manage(Request::ProfileCreate {
            profile: "ada/agent-gh".into(),
            description: Some("agents".into()),
            policy: policy(&["gh-read"], &[]),
            default: false,
        })
        .unwrap();
    for (name, value) in [("GH_TOKEN", "example-one"), ("EXTRA", "example-two")] {
        fixture
            .manage(Request::Put {
                profile: "ada/agent-gh".into(),
                name: name.into(),
                value: value.into(),
            })
            .unwrap();
    }
    fixture
        .manage(Request::Unset {
            profile: "ada/agent-gh".into(),
            name: "EXTRA".into(),
        })
        .unwrap();
    let shown = fixture
        .manage(Request::ProfileShow {
            profile: "ada/agent-gh".into(),
        })
        .unwrap();
    assert_eq!(shown[0]["profile"]["env"], serde_json::json!(["GH_TOKEN"]));
    assert!(
        !shown.to_string().contains("example-one"),
        "values never come back out"
    );
    let listed = fixture.manage(Request::ProfileList).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // A policy change applies to the next call.
    let refused = run(
        fixture.connect(),
        Some("ada/agent-gh"),
        &["gh", "pr", "create"],
        &cwd,
    );
    assert!(refusal(&refused).contains("no allow rule matches"));
    fixture
        .manage(Request::PolicySet {
            profile: "ada/agent-gh".into(),
            policy: policy(&["gh-pr"], &[]),
        })
        .unwrap();
    let created = run(
        fixture.connect(),
        Some("ada/agent-gh"),
        &["gh", "pr", "create"],
        &cwd,
    );
    assert_eq!(created.result.unwrap(), 0);

    // A login is the owner's, whatever the policy says; nobody else's.
    let login = |connection: Connection| {
        run_request(
            connection,
            RunRequest {
                profile: Some("ada/agent-gh".into()),
                argv: vec!["gh".into(), "auth".into(), "login".into()],
                login: true,
                ..RunRequest::default()
            },
            &cwd,
        )
    };
    let owned = login(fixture.connect());
    assert_eq!(owned.result.as_ref().unwrap(), &0, "{}", owned.stderr);
    assert_eq!(owned.stdout, "gh auth login\n");
    let (key, _) = MemberKey::generate().unwrap();
    fixture
        .manage(Request::Register {
            node: "host/example".into(),
            key: key.public().into(),
        })
        .unwrap();
    let grant = fixture
        .manage(Request::GrantAdd {
            profile: "ada/agent-gh".into(),
            to: "agent/fleet/fixture-web/**".into(),
            policy: policy(&["gh-read"], &[]),
            until_unix_ms: None,
        })
        .unwrap();
    let seat_login = login(fixture.seat(&key, "agent/fleet/fixture-web/builder"));
    assert!(
        refusal(&seat_login).contains("only its owner logs it in"),
        "{}",
        refusal(&seat_login)
    );

    // The owner sees and removes the grants on their profiles.
    fixture.as_person();
    let grants = fixture.manage(Request::GrantList).unwrap();
    assert_eq!(grants.as_array().unwrap().len(), 1);
    fixture
        .manage(Request::GrantRemove {
            grant: grant["id"].as_str().unwrap().into(),
        })
        .unwrap();
    assert!(
        fixture
            .manage(Request::GrantList)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );

    fixture
        .manage(Request::ProfileRemove {
            profile: "ada/agent-gh".into(),
        })
        .unwrap();
    assert!(
        fixture
            .manage(Request::ProfileList)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!fixture.store.join("profiles/ada/agent-gh").exists());
}

#[test]
fn a_seat_passes_the_files_gh_reads_and_names_no_file_of_the_profile() {
    if !sandbox_available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.tool(
        "gh",
        "echo \"gh $*\"\nprev=\nfor a; do [ \"$prev\" = --body-file ] && cat \"$a\"; prev=$a; done",
    );
    fixture
        .manage(Request::ProfileCreate {
            profile: "ada/agent-gh".into(),
            description: None,
            policy: policy(&["gh-agent"], &[]),
            default: false,
        })
        .unwrap();
    let (key, _) = MemberKey::generate().unwrap();
    fixture
        .manage(Request::Register {
            node: "host/example".into(),
            key: key.public().into(),
        })
        .unwrap();
    fixture
        .manage(Request::GrantAdd {
            profile: "ada/agent-gh".into(),
            to: "agent/fleet/fixture-web/**".into(),
            policy: policy(&["gh-agent"], &[]),
            until_unix_ms: None,
        })
        .unwrap();
    let cwd = fixture.checkout("web");
    // A body written anywhere the caller can read, outside the checkout.
    let notes = fixture.root.join("notes.md");
    fs::write(&notes, "the body\n").unwrap();
    let edited = run(
        fixture.seat(&key, "agent/fleet/fixture-web/builder"),
        None,
        &[
            "gh",
            "pr",
            "edit",
            "7",
            "--body-file",
            notes.to_str().unwrap(),
        ],
        &cwd,
    );
    assert_eq!(edited.result.as_ref().unwrap(), &0, "{}", edited.stderr);
    assert!(
        edited.stdout.contains("--body-file /dev/fd/"),
        "{}",
        edited.stdout
    );
    assert!(edited.stdout.ends_with("the body\n"), "{}", edited.stdout);
    // A path the command itself would open is refused: a raw request names the profile's login.
    {
        use super::protocol::{self, Reply};
        use std::os::unix::net::UnixStream;
        // As the owner, whose own use the profile's policy governs too.
        fixture.as_person();
        let stream = UnixStream::connect(&fixture.socket).unwrap();
        protocol::send(&stream, &Request::Hello { attestation: None }, &[]).unwrap();
        let _ = protocol::recv::<Reply>(&stream).unwrap().unwrap();
        let null: OwnedFd = fs::File::open("/dev/null").unwrap().into();
        let directory: OwnedFd = fs::File::open(&cwd).unwrap().into();
        protocol::send(
            &stream,
            &Request::Run(RunRequest {
                profile: Some("ada/agent-gh".into()),
                argv: [
                    "gh",
                    "pr",
                    "edit",
                    "7",
                    "--body-file",
                    ".config/gh/hosts.yml",
                ]
                .map(str::to_owned)
                .to_vec(),
                directories: 1,
                ..RunRequest::default()
            }),
            &[
                null.as_raw_fd(),
                null.as_raw_fd(),
                null.as_raw_fd(),
                directory.as_raw_fd(),
            ],
        )
        .unwrap();
        let (reply, _) = protocol::recv::<Reply>(&stream).unwrap().unwrap();
        let Reply::Refused { reason, .. } = reply else {
            panic!("ran: {reply:?}");
        };
        assert!(reason.contains("may read only"), "{reason}");
    }
}
