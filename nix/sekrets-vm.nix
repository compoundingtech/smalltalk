# Proves the sekrets gateway with real Unix users in a VM: the gateway runs as the `sekrets` user
# with its store closed to everyone else; ada calls it from an ssh login session as herself and
# from a seat-like process in her service manager that her st daemon vouches for; robin, another
# person, gets nothing of ada's unless she grants it. The isolation-vm CI job builds this test's
# driver and runs it outside the Nix sandbox with a prebuilt st binary:
#
#   ST_BINARY           the st binary, built in the flake's dev shell
#   ST_SEKRETS_BINARY   the sekrets binary, built beside it
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
    checkout_roots = ["/srv/people"]

    [people]
    "1000" = "person/ada"
    "1001" = "person/robin"
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
        ExecStart = "/usr/local/libexec/sekrets serve --config /etc/st-sekrets/gateway.toml";
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
    sk = "/usr/local/bin/sekrets"
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("sshd.service")
    machine.wait_for_unit("default.target", "ada")
    machine.succeed("mkdir -p /usr/local/bin /usr/local/libexec")
    machine.copy_from_host(os.environ["ST_BINARY"], "/usr/local/bin/st")
    machine.copy_from_host(os.environ["ST_SEKRETS_BINARY"], "/usr/local/libexec/sekrets")
    machine.succeed(
        "chown root:root /usr/local/libexec/sekrets /usr/local/bin/st"
        " && chmod 0755 /usr/local/libexec/sekrets /usr/local/bin/st"
        " && ln -sf /usr/local/libexec/sekrets /usr/local/bin/sekrets"
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
    assert "person/ada, from a login session" in login("ada", f"{sk} whoami")
    assert "unidentified process of person/ada" in manager(f"{sk} whoami")
    machine.fail("su nobody -s /bin/sh -c '/usr/local/bin/st sekrets whoami'")

    # Ada's profiles: her own, and one for her agents with a token she puts in.
    login("ada", f"{sk} profile create ada/gh --preset everything --preset no-credential-printing --default")
    login("ada", f"{sk} profile create ada/agent-gh --preset gh-pr --preset git-push --allow 'git status'")
    login("ada", f"printf example-token | {sk} put GH_TOKEN --profile ada/agent-gh")
    assert "gh pr list token= home=/var/lib/st-sekrets/profiles/ada/gh/home" in login(
        "ada", f"{sk} -- gh pr list"
    )
    assert "token=set" in login("ada", f"{sk} --profile ada/agent-gh -- gh pr view 1")
    assert "denied by rule `gh auth token`" in login("ada", f"{sk} -- gh auth token", succeed=False)

    # No one but the sekrets user reads the store; the token appears in no output or log.
    for user in ["ada", "robin"]:
        login(user, "ls /var/lib/st-sekrets", succeed=False)
        login(user, "cat /var/lib/st-sekrets/sekrets.db", succeed=False)
    assert "example-token" not in login("ada", f"{sk} log --limit 200")

    # Robin has nothing of Ada's until she grants it, and then only what the grant allows.
    assert "owns no profile and has been granted none" in login("robin", f"{sk} -- gh pr list", succeed=False)
    login("ada", f"{sk} grant ada/agent-gh --to person/robin --preset gh-read")
    assert "token=set" in login("robin", f"{sk} -- gh pr view 1")
    assert "no allow rule matches" in login("robin", f"{sk} -- gh pr create --draft", succeed=False)
    login("robin", f"{sk} grant ada/agent-gh --to person/robin --preset gh-pr", succeed=False)

    # A process in the service manager without its daemon's word is refused.
    assert "not identified" in manager(f"{sk} -- gh pr list", succeed=False)

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
    assert "can now use the profiles granted to them" in login("ada", f"{sk} enable")
    login(
        "ada",
        f"{sk} grant ada/agent-gh --to 'agent/fleet/fixture-example/**' --preset gh-pr --preset git-push --allow 'git status'",
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
        f"{sk} whoami; "
        f"{sk} -- gh pr create --draft --title Example; echo exit=$?; "
        f"{sk} -- gh auth status; echo exit=$?; "
        f"{sk} --profile ada/gh -- gh pr list; echo exit=$?; "
        f"{sk} -- git status --porcelain; echo exit=$?"
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

    # Every call, its exit and every refusal is in the gateway's log, which ada reads; ada's
    # daemon records them as local observations that age out and go to OpenTelemetry.
    log = login("ada", f"{sk} log --limit 200")
    print(log)
    assert "call" in log and "agent/fleet/fixture-example/web" in log, log
    assert "refused" in log and "no allow rule matches" in log, log
    assert "example-token" not in log, log

    # Adopting gh, step 1: every seat's gh runs through sekrets with the agent profile; ada's own
    # shell keeps the real gh. Running it again changes nothing.
    adopt = f"{sk} adopt gh --yes --bin-dir /srv/people/ada/.local/bin"
    first = login("ada", adopt)
    print(first)
    assert "done  agent grant" in first and "done  gh shim" in first, first
    again = login("ada", adopt)
    print(again)
    assert "done" not in again and "todo" not in again, again
    assert "/usr/local/libexec/sekrets" in login("ada", "cat /srv/people/ada/.local/bin/gh")
    shell = login("ada", "PATH=/srv/people/ada/.local/bin:$PATH gh pr list")
    assert "home=/srv/people/ada" in shell, shell
    machine.succeed(
        "su ada -s /bin/sh -c "
        + shlex.quote(
            "XDG_RUNTIME_DIR=/run/user/1000 PTY_ROOT=/srv/people/ada/.local/state/st3/pty "
            "systemd-run --user --scope --unit st3-seat-adopt.scope --quiet "
            "--setenv=PATH=/run/current-system/sw/bin "
            "-- pty run -d --force --id seat-adopt --cwd /srv/people/ada/web "
            "--tag st3.scope-unit=st3-seat-adopt.scope --tag st3.subject=agent/fleet/fixture-example/adopter "
            "--env ST_AGENT=agent/fleet/fixture-example/adopter --env XDG_RUNTIME_DIR=/run/user/1000 "
            "--env PATH=/srv/people/ada/.local/bin:/run/current-system/sw/bin:/usr/local/bin "
            "-- sh -c " + shlex.quote("{ gh pr view 1; gh auth token; echo exit=$?; } > /srv/people/ada/adopt.out 2>&1; echo done >> /srv/people/ada/adopt.out; sleep 600")
        )
    )
    machine.wait_until_succeeds("grep -q '^done' /srv/people/ada/adopt.out", timeout=120)
    seat_out = machine.succeed("cat /srv/people/ada/adopt.out")
    print(seat_out)
    assert "gh pr view 1 token=set home=/var/lib/st-sekrets/profiles/ada/agent-gh/home" in seat_out, seat_out
    assert "no allow rule matches `gh auth token`" in seat_out, seat_out
    assert "done  gh shim" not in login("ada", adopt)
    undone = login("ada", f"{sk} unadopt gh --bin-dir /srv/people/ada/.local/bin")
    assert "removed" in undone, undone
    machine.fail("test -e /srv/people/ada/.local/bin/gh")
  '';
}
