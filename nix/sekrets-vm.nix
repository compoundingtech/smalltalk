# Proves the sekrets gateway with real Unix users in a VM: the gateway runs as the `sekrets` user
# with its store closed to everyone else; ada calls it from an ssh login session as herself and
# from a seat-like process in her service manager that her st daemon vouches for; robin, another
# person, gets nothing of ada's unless she grants it. The isolation-vm CI job builds this test's
# driver and runs it outside the Nix sandbox with a prebuilt st binary:
#
#   ST_SEKRETS_BINARY   the st binary, built in the flake's dev shell
#
# The VM compiles nothing. KVM is required; there is no emulation fallback.
{ pkgs, pty }:
let
  # A stand-in for gh: it says what it was asked and whether it got a token, never the token.
  fakeGh = pkgs.writeShellScriptBin "gh" ''
    echo "gh $* token=''${GH_TOKEN:+set} home=$HOME"
    if test -f checkout-marker; then
      echo "checkout=$(pwd):$(cat checkout-marker)"
      options=$(${pkgs.util-linux}/bin/findmnt -n -o OPTIONS --target .)
      for flag in ro nosuid nodev; do
        case ",$options," in
          *,$flag,*) ;;
          *) echo "missing-checkout-mount-flag=$flag"; exit 91 ;;
        esac
      done
      echo "checkout-mount-ro-nosuid-nodev"
      if touch forbidden-checkout-write 2>/dev/null; then exit 90; fi
      echo "checkout-readonly"
      cat
    fi
  '';
  # VM-only barrier after the gateway verifies a directory and before bwrap mounts it.
  # No production hook, environment flag or caller-controlled executable is introduced.
  checkedBwrap = pkgs.writeShellScript "sekrets-checked-bwrap" ''
    if test -e /run/st-sekrets/bind-control/watch; then
      touch /run/st-sekrets/bind-control/invoked
    fi
    if test -e /run/st-sekrets/bind-control/pause; then
      touch /run/st-sekrets/bind-control/ready
      ${pkgs.coreutils}/bin/timeout 60 cat /run/st-sekrets/bind-control/resume >/dev/null || {
        echo "VM bind barrier was not released within 60s" >&2
        exit 124
      }
    fi
    exec ${pkgs.bubblewrap}/bin/bwrap "$@"
  '';
  gatewayConfig = pkgs.writeText "gateway.toml" ''
    bwrap = "${checkedBwrap}"
    path = ["/run/current-system/sw/bin"]
    checkout_roots = ["/srv/people"]

    [people]
    "1000" = "person/ada"
    "1001" = "person/robin"

    [programs."1002"]
    principal = "program/ci1/evidence"
    person = "person/ada"
    unit = "ci-evidence-caller.service"
    command = ["gh", "pr", "view", "1"]
    [programs."1002".checkout]
    backing_root = "/var/cache/evidence/work"
    mount_root = "/w"
    [programs."1003"]
    principal = "program/ci1/host-evidence"
    person = "person/ada"
    unit = "ci-host-evidence.service"
    command = ["gh", "pr", "view", "1"]
  '';
in
pkgs.testers.runNixOSTest {
  name = "sekrets-gateway";
  qemu.forceAccel = true;
  nodes.machine = {
    virtualisation.cores = 2;
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
    # Homes outside /home: the gateway serves the checkout roots its configuration names.
    users.users.ada = {
      isNormalUser = true;
      uid = 1000;
      home = "/srv/people/ada";
      linger = true;
    };
    users.users.robin = {
      isNormalUser = true;
      uid = 1001;
      home = "/srv/people/robin";
    };
    users.users.sekrets = {
      isSystemUser = true;
      group = "sekrets";
    };
    users.groups.sekrets = { };
    users.users.evidence-caller = {
      isSystemUser = true;
      uid = 1002;
      group = "evidence-caller";
    };
    users.groups.evidence-caller = { };
    users.users.host-evidence-caller = {
      isSystemUser = true;
      uid = 1003;
      group = "host-evidence-caller";
    };
    users.groups.host-evidence-caller = { };
    services.openssh.enable = true;
    environment.systemPackages = [
      pty
      fakeGh
      pkgs.git
      pkgs.bubblewrap
      pkgs.acl
    ];
    environment.etc."st-sekrets/gateway.toml".source = gatewayConfig;
    systemd.services.st-sekrets = {
      description = "st sekrets gateway";
      serviceConfig = {
        User = "sekrets";
        Group = "sekrets";
        ExecStart = "/usr/local/libexec/st-sekrets sekrets serve --config /etc/st-sekrets/gateway.toml";
        RuntimeDirectory = "st-sekrets";
        RuntimeDirectoryMode = "0755";
        StateDirectory = "st-sekrets";
        StateDirectoryMode = "0700";
        UMask = "0077";
        NoNewPrivileges = true;
      };
    };
  };
  testScript = ''
    import os
    import shlex

    st = "/usr/local/bin/st"
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("sshd.service")
    machine.wait_for_unit("default.target", "ada")
    machine.copy_from_host(os.environ["ST_SEKRETS_BINARY"], "/usr/local/libexec/st-sekrets")
    machine.succeed(
        "chown root:root /usr/local/libexec/st-sekrets && chmod 0755 /usr/local/libexec/st-sekrets"
        " && mkdir -p /usr/local/bin && ln -sf /usr/local/libexec/st-sekrets /usr/local/bin/st"
    )
    # As `st sekrets setup` does: the gateway may pass through each home, never read it.
    machine.succeed("setfacl -m u:sekrets:x /srv/people/ada /srv/people/robin")
    machine.succeed("systemctl start st-sekrets.service")
    machine.wait_for_file("/run/st-sekrets/gateway.sock")

    machine.succeed("mkdir -p /root/.ssh && ssh-keygen -q -t ed25519 -N ''' -f /root/.ssh/id_ed25519")
    for user in ["ada", "robin"]:
        machine.succeed(
            f"install -d -o {user} -m 0700 /srv/people/{user}/.ssh"
            f" && install -o {user} -m 0600 /root/.ssh/id_ed25519.pub /srv/people/{user}/.ssh/authorized_keys"
        )

    def login(user, command, succeed=True):
        """Run as a person from an ssh login session, which logind puts in a session scope."""
        ssh = (
            "ssh -o StrictHostKeyChecking=no -o BatchMode=yes -i /root/.ssh/id_ed25519 "
            f"{user}@localhost {shlex.quote(command)} 2>&1"
        )
        return machine.succeed(ssh) if succeed else machine.fail(ssh)

    def manager(command, succeed=True):
        """Run as ada inside her service manager, where seats run, with no attestation."""
        run = (
            "su ada -s /bin/sh -c "
            + shlex.quote(
                "XDG_RUNTIME_DIR=/run/user/1000 systemd-run --user --wait --pipe --collect --quiet "
                f"--setenv=PATH=/run/current-system/sw/bin -- {command}"
            )
            + " 2>&1"
        )
        return machine.succeed(run) if succeed else machine.fail(run)

    # The kernel tells a login session from the service manager.
    assert "person/ada, from a login session" in login("ada", f"{st} sekrets whoami")
    assert "unidentified process of person/ada" in manager(f"{st} sekrets whoami")
    machine.fail("su nobody -s /bin/sh -c '/usr/local/bin/st sekrets whoami'")

    # Ada's profiles: her own, and one for her agents with a token she puts in.
    login("ada", f"{st} sekrets profile create ada/gh --preset everything --preset no-credential-printing --default")
    login("ada", f"{st} sekrets profile create ada/agent-gh --preset gh-pr --preset git-push --allow 'git status'")
    login("ada", f"printf example-token | {st} sekrets put GH_TOKEN --profile ada/agent-gh")
    assert "gh pr list token= home=/var/lib/st-sekrets/profiles/ada/gh/home" in login(
        "ada", f"{st} sekrets -- gh pr list"
    )
    assert "token=set" in login("ada", f"{st} sekrets --profile ada/agent-gh -- gh pr view 1")

    # A dedicated system UID + exact root-created unit, BEFORE any st daemon exists.
    # It receives a command grant, never Ada's person authority or profile management.
    machine.succeed("install -d -m 0755 /var/cache/evidence /w && install -d -o evidence-caller -m 0700 /var/cache/evidence/work && install -d -o evidence-caller -m 0755 /var/cache/evidence/work/smalltalk && printf private-runner-checkout > /var/cache/evidence/work/smalltalk/checkout-marker && setfacl -m u:sekrets:x /var/cache/evidence/work")
    login("ada", f"{st} sekrets grant ada/agent-gh --to program/ci1/evidence --allow 'gh pr view'")
    def program(command, unit="ci-evidence-caller.service", user="evidence-caller", succeed=True,
                cwd="/w/smalltalk", backing="/var/cache/evidence/work"):
        call = (f"systemd-run --quiet --wait --pipe --collect --unit {unit} "
                f"--property=User={user} --property=WorkingDirectory={cwd} "
                f"--property=BindPaths={backing}:/w -- /bin/sh -c "
                + shlex.quote(f"({command}) </dev/null") + " 2>&1")
        # --pipe otherwise inherits the driver's open stdin. The fake gh intentionally
        # reads to EOF; explicit pipelines inside the group still provide their payload.
        # Bound the call, but never mistake a timeout for an expected refusal.
        status, output = machine.execute("timeout --kill-after=5 120 " + call)
        assert status not in (124, 137), f"program call timed out: {output}"
        assert (status == 0) if succeed else (status != 0), output
        return output
    assert "run-only system service" in program(f"{st} sekrets whoami")
    with subtest("Program first command binds its authorized private checkout"):
        assert "token=set" in program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 1")
    output = program(f"printf 'structured-stdin-only' | {st} sekrets --profile ada/agent-gh -- gh pr view 1")
    assert "checkout=/w/smalltalk:private-runner-checkout" in output
    assert "checkout-readonly" in output and "structured-stdin-only" in output
    assert "checkout-mount-ro-nosuid-nodev" in output
    machine.fail("test -e /var/cache/evidence/work/smalltalk/forbidden-checkout-write")
    machine.fail("test -e /w/smalltalk") # the gateway host has only the empty mount anchor

    with subtest("Program bind retains the verified inode across caller path replacement"):
        machine.succeed("install -d -o sekrets -m 0700 /run/st-sekrets/bind-control && mkfifo /run/st-sekrets/bind-control/resume && chown sekrets /run/st-sekrets/bind-control/resume && touch /run/st-sekrets/bind-control/pause && install -o evidence-caller -m 0600 /dev/null /var/cache/evidence/swap-output")
        swap_command = (f"{st} sekrets --profile ada/agent-gh -- gh pr view 1 </dev/null > /var/cache/evidence/swap-output 2>&1; "
                        "result=$?; printf '\\nexit=%s\\n' \"$result\" >> /var/cache/evidence/swap-output; exit \"$result\"")
        machine.succeed("systemd-run --quiet --no-block --unit ci-evidence-caller.service "
                        "--property=Type=oneshot --property=RemainAfterExit=yes "
                        "--property=User=evidence-caller --property=WorkingDirectory=/w/smalltalk "
                        "--property=BindPaths=/var/cache/evidence/work:/w -- /bin/sh -c " + shlex.quote(swap_command))
        machine.wait_for_file("/run/st-sekrets/bind-control/ready")
        # The caller namespace sees a replacement at /w/smalltalk. The verified descriptor
        # still names the original directory, now renamed beneath the same backing root.
        machine.succeed("mv /var/cache/evidence/work/smalltalk /var/cache/evidence/work/verified-original && install -d -o evidence-caller -m 0755 /var/cache/evidence/work/smalltalk && printf swapped-directory > /var/cache/evidence/work/smalltalk/checkout-marker && timeout 10 sh -c 'printf resume > /run/st-sekrets/bind-control/resume'")
        machine.wait_for_unit("ci-evidence-caller.service")
        assert machine.succeed("systemctl show --value -p ExecMainStatus ci-evidence-caller.service").strip() == "0"
        swapped = machine.succeed("cat /var/cache/evidence/swap-output")
        assert "checkout=/w/smalltalk:private-runner-checkout" in swapped
        assert "checkout-readonly" in swapped and "exit=0" in swapped
        assert "checkout-mount-ro-nosuid-nodev" in swapped
        assert "swapped-directory" not in swapped
        machine.succeed("systemctl stop ci-evidence-caller.service && rm -rf /run/st-sekrets/bind-control /var/cache/evidence/work/smalltalk && mv /var/cache/evidence/work/verified-original /var/cache/evidence/work/smalltalk")
    program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 1", cwd="/tmp", succeed=False)
    program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 1", cwd="/w/missing", succeed=False)
    machine.succeed("install -d -m 0755 /var/cache/evidence/other/smalltalk && ln -s /var/cache/evidence/other /var/cache/evidence/work/escape")
    program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 1", cwd="/w/escape/smalltalk", succeed=False)
    with subtest("Program refuses a private checkout with the wrong inode before bwrap"):
        machine.succeed("install -d -o sekrets -m 0700 /run/st-sekrets/bind-control && touch /run/st-sekrets/bind-control/watch")
        assert "passed checkout does not match the configured runner backing directory" in program(
            f"{st} sekrets --profile ada/agent-gh -- gh pr view 1", backing="/var/cache/evidence/other", succeed=False)
        machine.fail("test -e /run/st-sekrets/bind-control/invoked")
        machine.succeed("rm -rf /run/st-sekrets/bind-control")
    # Reachable private mount but wrong physical-root owner: refusal comes from the gateway,
    # rather than a client that cannot enter its cwd.
    machine.succeed("chown robin /var/cache/evidence/work && chmod 0755 /var/cache/evidence/work")
    assert "runner checkout root has the wrong owner" in program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 1", succeed=False)
    machine.succeed("chown evidence-caller /var/cache/evidence/work && chmod 0700 /var/cache/evidence/work && setfacl -m u:sekrets:x /var/cache/evidence/work")
    # A Program with no private-mount mapping must refuse an out-of-roots cwd too;
    # the ordinary person's profile-HOME fallback must not apply.
    login("ada", f"{st} sekrets grant ada/agent-gh --to program/ci1/host-evidence --allow 'gh pr view'")
    assert "not under a checkout root" in program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 1", unit="ci-host-evidence.service", user="host-evidence-caller", cwd="/tmp", succeed=False)
    assert "program/ci1/evidence" in login("ada", f"{st} sekrets log --limit 200")
    program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 2", succeed=False)
    program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 1", unit="other-evidence.service", succeed=False)
    program(f"{st} sekrets --profile ada/agent-gh -- gh pr view 1", user="robin", succeed=False)
    program(f"{st} sekrets profile policy ada/agent-gh --preset everything", succeed=False)
    program(f"{st} sekrets grant ada/agent-gh --to person/robin --preset everything", succeed=False)
    program(f"printf refused | {st} sekrets put KEY --profile ada/agent-gh", succeed=False)
    program(f"{st} sekrets log", succeed=False)
    program("cat /var/lib/st-sekrets/sekrets.db", succeed=False)
    program("ls /var/lib/st-sekrets/profiles/ada/agent-gh/home", succeed=False)
    signing_home = "/var/lib/st-sekrets/profiles/ada/agent-gh/home"
    signing_key = f"{signing_home}/signing-key"
    machine.succeed(f"printf throwaway > {signing_key} && chown sekrets:sekrets {signing_key} && chmod 0600 {signing_key}")
    program(f"cat {signing_key}", succeed=False)
    assert "denied by rule `gh auth token`" in login("ada", f"{st} sekrets -- gh auth token", succeed=False)

    # No one but the sekrets user reads the store; the token appears in no output or log.
    for user in ["ada", "robin"]:
        login(user, "ls /var/lib/st-sekrets", succeed=False)
        login(user, "cat /var/lib/st-sekrets/sekrets.db", succeed=False)
    assert "example-token" not in login("ada", f"{st} sekrets log --limit 200")

    # Robin has nothing of Ada's until she grants it, and then only what the grant allows.
    assert "owns no profile and has been granted none" in login("robin", f"{st} sekrets -- gh pr list", succeed=False)
    login("ada", f"{st} sekrets grant ada/agent-gh --to person/robin --preset gh-read")
    assert "token=set" in login("robin", f"{st} sekrets -- gh pr view 1")
    assert "no allow rule matches" in login("robin", f"{st} sekrets -- gh pr create --draft", succeed=False)
    login("robin", f"{st} sekrets grant ada/agent-gh --to person/robin --preset gh-pr", succeed=False)

    # A process in the service manager without its daemon's word is refused.
    assert "not identified" in manager(f"{st} sekrets -- gh pr list", succeed=False)

    # Ada's st daemon vouches for her seats once she registers its key from a login session.
    machine.succeed(
        "install -d -o ada -m 0700 /srv/people/ada/.config /srv/people/ada/.config/st3"
        " && echo 'person = \"person/ada\"' > /srv/people/ada/.config/st3/config.toml"
        " && chown ada /srv/people/ada/.config/st3/config.toml"
    )
    machine.succeed(
        "su ada -s /bin/sh -c "
        + shlex.quote(
            "XDG_RUNTIME_DIR=/run/user/1000 systemd-run --user --unit st3-daemon --quiet "
            f"--setenv=PATH=/run/current-system/sw/bin -- {st} up"
        )
    )
    machine.wait_for_file("/run/user/1000/st3.sock")
    # The socket file appears while the daemon is still starting ("the API is not ready"), so the
    # file is not readiness. Ready means a daemon API read answers: `st clients` is read-only and
    # fails until then.
    machine.wait_until_succeeds(
        "ssh -o StrictHostKeyChecking=no -o BatchMode=yes -i /root/.ssh/id_ed25519 "
        f"ada@localhost {shlex.quote(f'{st} clients')} >/dev/null 2>&1",
        timeout=120,
    )
    assert "can now use the profiles granted to them" in login("ada", f"{st} sekrets enable")
    login(
        "ada",
        f"{st} sekrets grant ada/agent-gh --to 'agent/fleet/fixture-example/**' --preset gh-pr --preset git-push --allow 'git status'",
    )

    # A seat: a terminal in its own scope, tagged as st tags a seat's terminal, in a checkout
    # whose git configuration would run a program for anyone who trusted it.
    machine.succeed(
        "su ada -s /bin/sh -c "
        + shlex.quote(
            "cd ~ && git init -q -b main web && cd web"
            " && git -c user.email=ada@example.com -c user.name=Ada commit -q --allow-empty -m first"
            " && git remote add origin https://example.com/web.git"
            " && printf '#!/bin/sh\\necho PWNED >&2\\n' > /srv/people/ada/hook.sh && chmod +x /srv/people/ada/hook.sh"
            " && git config core.fsmonitor /srv/people/ada/hook.sh"
        )
    )
    seat = (
        f"{st} sekrets whoami; "
        f"{st} sekrets -- gh pr create --draft --title Example; echo exit=$?; "
        f"{st} sekrets -- gh auth status; echo exit=$?; "
        f"{st} sekrets --profile ada/gh -- gh pr list; echo exit=$?; "
        f"{st} sekrets -- git status --porcelain; echo exit=$?"
    )
    machine.succeed(
        "su ada -s /bin/sh -c "
        + shlex.quote(
            "XDG_RUNTIME_DIR=/run/user/1000 PTY_ROOT=/srv/people/ada/.local/state/st3/pty "
            "systemd-run --user --scope --unit st3-seat-web.scope --quiet "
            "--setenv=PATH=/run/current-system/sw/bin "
            "-- pty run -d --force --id seat-web --cwd /srv/people/ada/web "
            "--tag st3.scope-unit=st3-seat-web.scope --tag st3.subject=agent/fleet/fixture-example/web "
            "--env ST_AGENT=agent/fleet/fixture-example/web --env XDG_RUNTIME_DIR=/run/user/1000 "
            "--env PATH=/run/current-system/sw/bin:/usr/local/bin "
            f"-- sh -c {shlex.quote('{ ' + seat + '; } > /srv/people/ada/seat.out 2>&1; echo done >> /srv/people/ada/seat.out; sleep 600')}"
        )
    )
    machine.wait_until_succeeds("grep -q '^done' /srv/people/ada/seat.out", timeout=120)
    out = machine.succeed("cat /srv/people/ada/seat.out")
    print(out)
    print(machine.succeed("journalctl -u st-sekrets.service --no-pager -n 40 2>&1 || true"))
    print(machine.succeed("journalctl _UID=1000 --no-pager -n 60 -g 'sekrets|attest|st3:' 2>&1 || true"))
    assert "agent/fleet/fixture-example/web, working for person/ada" in out, out
    assert "gh pr create --draft --title Example token=set home=/var/lib/st-sekrets/profiles/ada/agent-gh/home" in out, out
    assert "no allow rule matches `gh auth status`" in out, out
    assert "profile ada/gh is not agent/fleet/fixture-example/web's" in out, out
    assert "PWNED" not in out, out
    # gh pr create and git status ran; gh auth status and ada's own profile were refused.
    assert out.count("exit=0") == 2, out

    # Every call, refusal and change is a claim on the profile, recorded by ada's daemon.
    # Every call, its exit and every refusal is in the gateway's log, which ada reads; ada's
    # daemon records them as local observations that age out and go to OpenTelemetry.
    log = login("ada", f"{st} sekrets log --limit 200")
    print(log)
    assert "call" in log and "agent/fleet/fixture-example/web" in log, log
    assert "refused" in log and "no allow rule matches" in log, log
    assert "example-token" not in log, log
  '';
}
