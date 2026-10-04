{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.cm3588-nvme-fan;
in
{
  options.services.cm3588-nvme-fan = {
    enable = lib.mkEnableOption "minimal NVMe-first PWM fan control for the CM3588 NAS";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "cm3588-nvme-fan package to run.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.cm3588-nvme-fan = {
      description = "CM3588 NVMe fan controller (nvme-fan pwm-fan hwmon)";
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = 5;

        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ReadOnlyPaths = "/sys";
        ReadWritePaths = [
          "/sys/class/hwmon"
          "/sys/class/thermal"
        ];
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectControlGroups = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictNamespaces = true;
        LockPersonality = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        MemoryDenyWriteExecute = true;
        CapabilityBoundingSet = "";
        SystemCallArchitectures = "native";
        SystemCallFilter = "@system-service";
        # No sockets are ever opened; AF_UNIX stays in case libc wants one.
        RestrictAddressFamilies = [ "AF_UNIX" ];
      };
    };
  };
}
