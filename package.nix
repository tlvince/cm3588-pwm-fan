{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "cm3588-nvme-fan";
  version = "0.1.0";

  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  meta = with lib; {
    description = "Minimal NVMe-first PWM fan controller for the CM3588 NAS (nvme-fan pwm-fan hwmon)";
    license = licenses.mit;
    platforms = [ "aarch64-linux" ];
    mainProgram = "cm3588-nvme-fan";
  };
}
