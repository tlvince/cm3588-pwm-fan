# cm3588-pwm-fan

Quiet NVMe cooling for [FriendlyELEC CM3588 NAS](https://www.friendlyelec.com/index.php?route=product/product&product_id=299) (RK3588) on NixOS. [Noctua NF-A8 5V PWM](https://www.noctua.at/en/products/nf-a8-5v-pwm/) on GPIO 4-wire PWM + separate kernel `nvme-fan` + minimal Rust daemon (`cm3588-nvme-fan`)

<div style="display: flex; flex-direction: row;">
<a href="https://github.com/user-attachments/assets/73f91a19-4e1d-47bd-a2a6-1f455fa72a76"><img width="400" alt="fan" src="https://github.com/user-attachments/assets/73f91a19-4e1d-47bd-a2a6-1f455fa72a76" /></a>
<a href="https://github.com/user-attachments/assets/58fb67b7-5c95-447b-ae65-501ee3551a55"><img width="400" alt="gpio" src="https://github.com/user-attachments/assets/58fb67b7-5c95-447b-ae65-501ee3551a55" /></a>
</div>

## Why this exists

- CM3588 Plus (16GB LPDDR5) in [metal case](https://www.amazon.co.uk/FriendlyELEC-Metal-Case-Custom-Made-Only/dp/B0F1LNW7WX). NVMe observed ~50°C idle, up to ~90°C under load
- Stock board fan path is poor for this job:
  - JST-ZH 1.5mm 2-pin port sits over the SoC, not the NVMes
  - Supplied fan is noisy
  - No premium quiet fan uses those pins natively
  - Port does PWM via variable DC (supply chop), fine for DC fans, wrong for true 4-wire control
- Linux 7.3 added in-kernel control ([3bbe67c4](https://github.com/torvalds/linux/commit/3bbe67c4de2aa8b0c042f92ad8c7462e8699cc37)), but bound to the stock `&fan` / SoC `package_thermal`. That cools the SoC, not the SSDs

## Project history

1. Started as “80mm very quiet 5V JST-ZH fan”. Only premium option: Noctua NF-A8 5V + re-crimp to `ZHR-2` (OmniJoin makes it solderless)
2. Compared PWM vs DC at equal price/specs (2200/450 RPM, 32.6 CFM, 17.7 dB(A), 0.75W). DC wins on the ZH header; PWM wins only with logic drive
3. Reviewed controllers: [jgomez](https://github.com/jgomezriesgobancario/cm3588-fan-controller) (raw `pwmchip` for 12V Foxconn on Pin 11 — wiring template only), [vijay](https://github.com/vijaygill/pwm-fan-cm3588) (Python `cooling_device` + NVMe temps — closest behavior), [martabal](https://github.com/martabal/cm3588-fan) (Rust `cooling_device`, CPU-only)
4. Moved to CM3588 GPIO headers (RPi layout, `CON6` in [NAS SDK schematic](https://wiki.friendlyelec.com/wiki/images/1/15/CM3588_NAS_SDK_2309_SCH.PDF)) for true 4-wire control
5. First DTS reused stock `&fan` on `pwm1`. Abandoned: Linux 7.3 `package_thermal` (1s poll, 55°C → state 0-1, 65°C → state 2-MAX) fights any NVMe userspace curve on the same node
6. Final: Noctua as **second** `nvme-fan` on `pwm14`, stock `&fan` untouched. Minimal std-only Rust daemon drives it from hottest NVMe + SoC safety

## Why NF-A8 5V PWM

- Full torque at low speed (no stall/no-start)
- Wide range: 450–2200 RPM spec
- Quieter at low speed (NE-FD + SCD)
- Same box contents as DC (USB adaptor, OmniJoin, extension). PWM-as-2-wire would be full-speed only — hence the GPIO move

## Wiring

No fan rewire. KK-254 4-pin → 4× Dupont females:

```text
pin 2 +5V  <-- yellow +V (constant, not ZH switched)
pin 6 GND  <-- black GND (common first)
pin 7      <-- green tach (GPIO3_B2, internal pull-up)
pin 11     <-- blue PWM (PWM14_M0 / GPIO3_C2, 3.3V, 40000ns = 25kHz)
```

- Tach is open-collector, 2 pulses/rev (~73 Hz max). Internal pull-up is enough over a short wire; add external 4.7–10k only if `fan1_input` jitters
- Airflow: open side → sticker/strut side. Strut side faces down onto drives

## Kernel (NixOS overlay)

`hardware.deviceTree.overlays` with `filter = "*rk3588-friendlyelec-cm3588-nas.dtb"`, `dtsFile`, no kernel rebuild. Needs `irq.h` + `rockchip.h` includes

```dts
&nvme_fan { compatible = "pwm-fan"; #cooling-cells = <2>;
  pwms = <&pwm14 0 40000 0>;
  cooling-levels = <30 50 75 100 150 255>;
  fan-supply = <&vcc_5v0_sys>;
  pinctrl-0 = <&fan_tach_pin>; /* 3 RK_PB2 GPIO + pcfg_pull_up */
  interrupt-parent = <&gpio3>;
  interrupts = <RK_PB2 IRQ_TYPE_EDGE_FALLING>;
  pulses-per-revolution = <2>; };
```

`&pwm14 { pwm14m0_pins, okay }` repeated for self-containment (already okay upstream)

## Measured curve

- Cold start: PWM 23 minimum
- Warm: PWM 20–23 ~175 RPM
- PWM 19: fan stopped

States:

- 0→30 (~234 RPM)
- 1→50 (~448)
- 2→75 (~715)
- 3→100 (~1023)
- 4→150 (~1412)
- 5→255 (~2321)

State 0 spins, never off. Stock DTS used `<0 50 80 120 160 220>`; calibrated `<30 ...>` avoids the stall zone

## Controller

Std-only Rust, sysfs only. No `nvme-cli`, Python, etc

- Discovery: `hwmon` with `name=="pwmfan" + fan1_input + pwm1 + of_node==nvme-fan` (canonical-path fallback). Ambiguous → refuse. Never hard-code `hwmonN`
- Writes `pwm1` to `[30 50 75 100 150 255]`; kernel syncs cooling state. Compares raw PWM so external `49` normalizes to `30`
- NVMe: `nvme` hwmon, Composite preferred, else lowest `tempN_input`. Hottest wins. SoC: `package-thermal` second. Sanity −40…150°C
- Curves: NVMe `<40:0, 40:1, 45:2, 50:3, 55:4, 60:5`; SoC `<55:0, 55:1, 60:2, 65:3, 70:4, 75:5`; `desired=max()`. Up immediate, 2°C down-hysteresis. `poll 5s / settle 5s` (SSD mass is slow; SoC framework already polls 1s)
- Failsafe → state 5 on: 2× 0 RPM, 3× tach errors, any sensor read fail that iteration, all inputs gone, `pwm1` read fail. Write fail → `write_failsafe_or_exit()` → exit for systemd restart. Absent drives = gone

## Results

Before:

- `hwmon10`, PWM30/263 RPM, SoC 51.7°C, NVMe 48.8/44.8/37.8°C
- NVMe state 2, SoC 0, final 2 (PWM75)

After:

- `0→2 (48.8/52.6°C)`
- `2→1 (42.8/49.9°C, 439 RPM)`

Matches ~448 RPM curve; 2→1 below 43°C as expected from 45°C − 2°C
