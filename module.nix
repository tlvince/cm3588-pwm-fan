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
    users.users.nvme-fan = {
      isSystemUser = true;
      group = "nvme-fan";
      description = "CM3588 NVMe fan controller";
    };
    users.groups.nvme-fan = { };

    services.udev.extraRules = ''
      ACTION=="add|change", SUBSYSTEM=="hwmon", ATTR{name}=="pwmfan", ENV{OF_NAME}=="nvme-fan", RUN+="${pkgs.coreutils}/bin/chgrp nvme-fan $sys%p/pwm1", RUN+="${pkgs.coreutils}/bin/chmod 0664 $sys%p/pwm1"
    '';

    systemd.services.cm3588-nvme-fan = {
      description = "CM3588 NVMe fan controller (nvme-fan pwm-fan hwmon)";
      wantedBy = [ "multi-user.target" ];
      wants = [ "systemd-udev-settle.service" ];
      after = [ "systemd-udev-settle.service" ];

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = 5;
        User = "nvme-fan";
        Group = "nvme-fan";
        UMask = "0027";
        # Hardening
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ReadOnlyPaths = "/sys";
        ReadWritePaths = [ "/sys/class/hwmon" ];
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
        RestrictAddressFamilies = [ "AF_UNIX" ];
      };
    };
  };
}
