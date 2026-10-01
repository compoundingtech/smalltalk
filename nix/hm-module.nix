{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkEnableOption mkIf mkOption types;
  cfg = config.services.smalltalk;
  # Copy st3 itself: a symlinkJoin-only executable resolves current_exe() back to the
  # original package, whose bin/pty would still win when seats prepend that directory.
  effectivePackage =
    if cfg.ptyPackage == null then
      cfg.package
    else
      pkgs.symlinkJoin {
        name = "st3-with-pty";
        paths = [ cfg.package ];
        postBuild = ''
          rm "$out/bin/st3" "$out/bin/pty" "$out/bin/st"
          cp ${cfg.package}/bin/st3 "$out/bin/st3"
          ln -s st3 "$out/bin/st"
          ln -s ${cfg.ptyPackage}/bin/pty "$out/bin/pty"
        '';
      };
  # Keep the real executable with the daemon's per-user state: current_exe()
  # resolves store symlinks, but seats must watch one replaceable path across deploys.
  binDir = "${cfg.stateDir}/bin";
  executable = "${binDir}/st3";
  environment = cfg.environment // {
    # The Linux daemon asks the user manager to move PTY servers into their own scopes via busctl.
    PATH = binDir + ":" + lib.makeBinPath (
      lib.optional (cfg.ptyPackage != null) cfg.ptyPackage
      ++ [ effectivePackage ]
      ++ lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.systemd
    ) + ":/usr/local/bin:/usr/bin:/bin";
    XDG_CONFIG_HOME = config.xdg.configHome;
    XDG_STATE_HOME = config.xdg.stateHome;
  };
  upArgs =
    [ "up" "--state-dir" cfg.stateDir ]
    ++ lib.optionals (cfg.node != null) [ "--node" cfg.node ]
    ++ lib.optionals (cfg.ptyRoot != null) [ "--pty-root" cfg.ptyRoot ]
    ++ lib.optionals (cfg.socket != null) [ "--socket" cfg.socket ]
    ++ lib.optionals (cfg.clientGatewaySocket != null) [ "--client-gateway-socket" cfg.clientGatewaySocket ]
    ++ lib.optionals (cfg.ptyPackage != null) [ "--pty-binary" "${cfg.ptyPackage}/bin/pty" ]
    ++ cfg.extraArgs;
  # systemd's command line parser is not a shell; escape its own substitutions too.
  systemdArg = arg:
    "\"${lib.replaceStrings [ "\\" "\"" "$" "%" ] [ "\\\\" "\\\"" "$$" "%%" ] arg}\"";
  applyCommands =
    map (file: [ "agents" "apply" "${file}" "--as" cfg.person ]) cfg.declarations.seats
    ++ map (file: [ "missions" "publish" "${file}" "--as" cfg.person ]) cfg.declarations.missions;
  applyScript = pkgs.writeShellScript "smalltalk-apply" (
    ''set -eu
''
    + lib.concatMapStringsSep "\n" (args:
      lib.escapeShellArgs ([ executable "--daemon-wait" "30" ]
        ++ lib.optionals (cfg.socket != null) [ "--endpoint" cfg.socket ]
        ++ args)
    ) applyCommands
    + "\n"
  );
  hasDeclarations = applyCommands != [ ];
in
{
  options.services.smalltalk = {
    enable = mkEnableOption "the smalltalk user daemon";
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.st3;
      description = "The st3 package.";
    };
    person = mkOption {
      type = types.str;
      description = "The person identity used by st3 commands, for example person/ada.";
    };
    node = mkOption { type = types.nullOr types.str; default = null; description = "Daemon node name; null uses the daemon default."; };
    stateDir = mkOption {
      type = types.str;
      default = "${config.xdg.stateHome}/st3";
      description = "Persistent daemon state directory.";
    };
    ptyRoot = mkOption { type = types.nullOr types.str; default = null; description = "PTY registry root; null uses the daemon default."; };
    socket = mkOption { type = types.nullOr types.str; default = null; description = "API socket; null uses the daemon default."; };
    clientGatewaySocket = mkOption { type = types.nullOr types.str; default = null; description = "Client gateway socket; null uses the daemon default."; };
    ptyPackage = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "PTY package used for both the daemon and spawned seats; null uses the bundled PTY.";
    };
    environment = mkOption {
      type = types.attrsOf types.str;
      default = { };
      description = "Additional daemon and declaration-service environment variables.";
    };
    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Additional arguments passed to st3 up.";
    };
    declarations = {
      seats = mkOption { type = types.listOf types.path; default = [ ]; description = "Seat KDL files to apply."; };
      missions = mkOption { type = types.listOf types.path; default = [ ]; description = "Mission KDL files to publish."; };
    };
    declarationsApply.enable = mkOption {
      type = types.bool;
      default = true;
      description = "Apply declared KDL after the daemon starts; omissions do not delete prior declarations.";
    };
    # TODO: Fleet and replication integration belongs to docs/st3/replication.md.
  };

  config = mkIf cfg.enable {
    home.packages = [ effectivePackage ];
    home.activation.smalltalkBinary = lib.hm.dag.entryBetween
      [ (if pkgs.stdenv.hostPlatform.isLinux then "reloadSystemd" else "setupLaunchAgents") ]
      [ "writeBoundary" ] ''
        if [[ -n "''${DRY_RUN_CMD:-}" ]]; then
          verboseEcho "Would install the smalltalk executable at ${executable}"
        else
          (
            set -eu
            binDir=${lib.escapeShellArg binDir}
            source=${lib.escapeShellArg "${effectivePackage}/bin/st3"}
            target=${lib.escapeShellArg executable}
            ${pkgs.coreutils}/bin/mkdir -p "$binDir"
            # Preserve the inode on no-op activation; replace symlinks even if bytes match.
            if [[ -L "$target" ]] || ! ${pkgs.diffutils}/bin/cmp -s "$source" "$target"; then
              temporary=$(${pkgs.coreutils}/bin/mktemp "$binDir/.st3.XXXXXX")
              trap '${pkgs.coreutils}/bin/rm -f "$temporary"' EXIT
              ${pkgs.coreutils}/bin/cp "$source" "$temporary"
              ${pkgs.coreutils}/bin/chmod 755 "$temporary"
              ${pkgs.coreutils}/bin/mv -f "$temporary" "$target"
            fi
            ${pkgs.coreutils}/bin/ln -sfn st3 "$binDir/st"
          )
        fi
      '';
    xdg.configFile."st3/config.toml".text =
      "person = ${builtins.toJSON cfg.person}\n"
      + lib.optionalString (cfg.node != null) "node = ${builtins.toJSON cfg.node}\n"
      + "state_dir = ${builtins.toJSON cfg.stateDir}\n"
      + lib.optionalString (cfg.ptyRoot != null) "pty_root = ${builtins.toJSON cfg.ptyRoot}\n"
      + lib.optionalString (cfg.socket != null) "socket = ${builtins.toJSON cfg.socket}\n"
      + lib.optionalString (cfg.clientGatewaySocket != null) "client_gateway_socket = ${builtins.toJSON cfg.clientGatewaySocket}\n";

    systemd.user.services = mkIf pkgs.stdenv.hostPlatform.isLinux ({
      smalltalk = {
        Unit = {
          Description = "st claims graph daemon";
          After = [ "network.target" ];
          # ExecStart is stable; sd-switch still needs to restart on a new build.
          X-Restart-Triggers = [ effectivePackage ];
        };
        Service = {
          Type = "simple";
          ExecStart = lib.concatStringsSep " " (map systemdArg ([ executable ] ++ upArgs));
          Environment = lib.mapAttrsToList (name: value: "${name}=${value}") (environment // { MALLOC_ARENA_MAX = "2"; });
          Restart = "on-failure";
          RestartSec = "5s";
          Nice = 0;
          # The daemon answers every command and attach, ahead of harness builds at the default 100.
          CPUWeight = 1000;
          IOWeight = 1000;
          KillMode = "control-group";
          MemoryMax = "1024M";
        };
        Install.WantedBy = [ "default.target" ];
      };
    } // lib.optionalAttrs (cfg.declarationsApply.enable && hasDeclarations) {
      smalltalk-apply = {
        Unit = {
          Description = "Apply smalltalk KDL declarations";
          After = [ "smalltalk.service" ];
          Requires = [ "smalltalk.service" ];
        };
        Service = {
          Type = "oneshot";
          ExecStart = toString applyScript;
          Restart = "on-failure";
          RestartSec = "5s";
          Environment = lib.mapAttrsToList (name: value: "${name}=${value}") environment;
        };
        Install.WantedBy = [ "default.target" ];
      };
    });

    home.activation.smalltalkLogs = mkIf pkgs.stdenv.hostPlatform.isDarwin (
      lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        $DRY_RUN_CMD mkdir -p ${lib.escapeShellArg "${cfg.stateDir}/logs"}
      ''
    );
    launchd.agents = mkIf pkgs.stdenv.hostPlatform.isDarwin ({
      smalltalk = {
        enable = true;
        config = {
          ProgramArguments = [ executable ] ++ upArgs;
          RunAtLoad = true;
          KeepAlive.SuccessfulExit = false;
          ProcessType = "Interactive";
          SoftResourceLimits.NumberOfFiles = 8192;
          StandardOutPath = "${cfg.stateDir}/logs/st3.stdout.log";
          StandardErrorPath = "${cfg.stateDir}/logs/st3.stderr.log";
          # Home Manager reloads changed plists; retain a build reference even
          # though ProgramArguments now points at the stable executable.
          EnvironmentVariables = environment // { SMALLTALK_PACKAGE = toString effectivePackage; };
        };
      };
    } // lib.optionalAttrs (cfg.declarationsApply.enable && hasDeclarations) {
      smalltalk-apply = {
        enable = true;
        config = {
          ProgramArguments = [ (toString applyScript) ];
          RunAtLoad = true;
          KeepAlive.SuccessfulExit = false;
          EnvironmentVariables = environment;
          StandardOutPath = "${cfg.stateDir}/logs/st3-apply.stdout.log";
          StandardErrorPath = "${cfg.stateDir}/logs/st3-apply.stderr.log";
        };
      };
    });
  };
}
