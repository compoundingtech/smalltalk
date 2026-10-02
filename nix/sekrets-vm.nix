# Proves the sekrets gateway with real Unix users in a VM: the gateway runs as the `sekrets` user
# with its store closed to everyone else; ada calls it from an ssh login session as herself and
# from a seat-like process in her service manager that her st daemon vouches for; bob, another
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
  '';
  gatewayConfig = pkgs.writeText "gateway.toml" ''
    bwrap = "${pkgs.bubblewrap}/bin/bwrap"
    path = ["/run/current-system/sw/bin"]
    checkout_roots = ["/home"]

    [people]
    "1000" = "person/ada"
    "1001" = "person/bob"
  '';
in
pkgs.testers.runNixOSTest {
  name = "sekrets-gateway";
  qemu.forceAccel = true;
  nodes.machine = {
    virtualisation.cores = 2;
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
    users.users.ada = {
      isNormalUser = true;
      uid = 1000;
      linger = true;
    };
    users.users.bob = {
      isNormalUser = true;
      uid = 1001;
    };
    users.users.sekrets = {
      isSystemUser = true;
      group = "sekrets";
    };
    users.groups.sekrets = { };
    services.openssh.enable = true;
    environment.systemPackages = [
      pty
      fakeGh
      pkgs.git
      pkgs.bubblewrap
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
    machine.succeed("systemctl start st-sekrets.service")
    machine.wait_for_file("/run/st-sekrets/gateway.sock")

    machine.succeed("mkdir -p /root/.ssh && ssh-keygen -q -t ed25519 -N ''' -f /root/.ssh/id_ed25519")
    for user in ["ada", "bob"]:
        machine.succeed(
            f"install -d -o {user} -m 0700 /home/{user}/.ssh"
            f" && install -o {user} -m 0600 /root/.ssh/id_ed25519.pub /home/{user}/.ssh/authorized_keys"
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
    assert "denied by rule `gh auth token`" in login("ada", f"{st} sekrets -- gh auth token", succeed=False)

    # No one but the sekrets user reads the store; the token appears in no output or log.
    for user in ["ada", "bob"]:
        login(user, "ls /var/lib/st-sekrets", succeed=False)
        login(user, "cat /var/lib/st-sekrets/sekrets.db", succeed=False)
    assert "example-token" not in login("ada", f"{st} sekrets log --limit 200")

    # Bob has nothing of Ada's until she grants it, and then only what the grant allows.
    assert "owns no profile and has been granted none" in login("bob", f"{st} sekrets -- gh pr list", succeed=False)
    login("ada", f"{st} sekrets grant ada/agent-gh --to person/bob --preset gh-read")
    assert "token=set" in login("bob", f"{st} sekrets -- gh pr view 1")
    assert "no allow rule matches" in login("bob", f"{st} sekrets -- gh pr create --draft", succeed=False)
    login("bob", f"{st} sekrets grant ada/agent-gh --to person/bob --preset gh-pr", succeed=False)

    # A process in the service manager without its daemon's word is refused.
    assert "not identified" in manager(f"{st} sekrets -- gh pr list", succeed=False)

    # Ada's st daemon vouches for her seats once she registers its key from a login session.
    machine.succeed(
        "install -d -o ada -m 0700 /home/ada/.config /home/ada/.config/st3"
        " && echo 'person = \"person/ada\"' > /home/ada/.config/st3/config.toml"
        " && chown ada /home/ada/.config/st3/config.toml"
    )
    machine.succeed(
        "su ada -s /bin/sh -c "
        + shlex.quote(
            "XDG_RUNTIME_DIR=/run/user/1000 systemd-run --user --unit st3-daemon --quiet "
            f"--setenv=PATH=/run/current-system/sw/bin -- {st} up"
        )
    )
    machine.wait_for_file("/run/user/1000/st3.sock")
    assert "can now use the profiles granted to them" in login("ada", f"{st} sekrets enable")
    login(
        "ada",
        f"{st} sekrets grant ada/agent-gh --to 'agent/fleet/example/**' --preset gh-pr --preset git-push --allow 'git status'",
    )

    # A seat: a terminal in its own scope, tagged as st tags a seat's terminal, in a checkout
    # whose git configuration would run a program for anyone who trusted it.
    machine.succeed(
        "su ada -s /bin/sh -c "
        + shlex.quote(
            "cd ~ && git init -q -b main web && cd web"
            " && git -c user.email=ada@example.com -c user.name=Ada commit -q --allow-empty -m first"
            " && git remote add origin https://example.com/web.git"
            " && printf '#!/bin/sh\\necho PWNED >&2\\n' > /home/ada/hook.sh && chmod +x /home/ada/hook.sh"
            " && git config core.fsmonitor /home/ada/hook.sh"
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
            "XDG_RUNTIME_DIR=/run/user/1000 PTY_ROOT=/home/ada/.local/state/st3/pty "
            "systemd-run --user --scope --unit st3-seat-web.scope --quiet "
            "--setenv=PATH=/run/current-system/sw/bin "
            "-- pty run -d --force --id seat-web --cwd /home/ada/web "
            "--tag st3.scope-unit=st3-seat-web.scope --tag st3.subject=agent/fleet/example/web "
            "--env ST_AGENT=agent/fleet/example/web --env XDG_RUNTIME_DIR=/run/user/1000 "
            "--env PATH=/run/current-system/sw/bin:/usr/local/bin "
            f"-- sh -c {shlex.quote(seat + ' > /home/ada/seat.out 2>&1; echo done >> /home/ada/seat.out; sleep 600')}"
        )
    )
    machine.wait_until_succeeds("grep -q '^done' /home/ada/seat.out", timeout=120)
    out = machine.succeed("cat /home/ada/seat.out")
    print(out)
    assert "agent/fleet/example/web, working for person/ada" in out, out
    assert "gh pr create --draft --title Example token=set home=/var/lib/st-sekrets/profiles/ada/agent-gh/home" in out, out
    assert "no allow rule matches `gh auth status`" in out, out
    assert "profile ada/gh is not agent/fleet/example/web's" in out, out
    assert "PWNED" not in out, out
    # gh pr create and git status ran; gh auth status and ada's own profile were refused.
    assert out.count("exit=0") == 2, out

    # Every call, refusal and change is a claim on the profile, recorded by ada's daemon.
    def recorded(_):
        history = machine.succeed(
            "su ada -s /bin/sh -c "
            + shlex.quote(f"XDG_RUNTIME_DIR=/run/user/1000 {st} subject history sekret/machine/ada/agent-gh --json")
        )
        return "sekret.called" in history and "sekret.refused" in history and "agent/fleet/example/web" in history
    retry(recorded, timeout_seconds=120)
  '';
}
