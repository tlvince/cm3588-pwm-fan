//! cm3588-nvme-fan — minimal NVMe-first fan controller for the CM3588 NAS.
//!
//! Hardware contract: controls ONLY the fan described by the DTS overlay
//! (six states, `0..=5`; state 0 is PWM 30 and still spins, so zero RPM at any
//! state is abnormal). The single device the daemon discovers and drives is the
//! fan **hwmon** device: `name == "pwmfan"`, DT node `nvme-fan` (function-
//! named, not brand-named), with `fan1_input` (tach) and `pwm1`.
//!
//! Why hwmon/`pwm1` instead of `cooling_device*/cur_state`: the thermal core
//! does not set a physical-device parent for cooling devices, so
//! `cooling_device*/device` cannot reliably identify `nvme-fan`. The
//! hwmon's `device` link does belong to the `pwm-fan` platform device, and the
//! `pwm-fan` driver (`pwm_fan_write`) updates its internal cooling state after
//! every `pwm1` write — so writing the calibrated levels below keeps the
//! cooling-state abstraction intact while provably touching only the right fan.
//!
//! Policy: the hottest NVMe composite temperature drives an aggressive curve,
//! SoC `package-thermal` drives a laxer curve; `desired = max(both)` with
//! ~2 C of down-hysteresis. Tachometer stall -> demand state 5 and log.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Configuration: all thresholds live here. Millidegrees Celsius, states 1..=5
// are reached when the temperature is >= the corresponding threshold.
// ---------------------------------------------------------------------------

struct Config {
    nvme_thresholds_mdec: [i32; 5],
    soc_thresholds_mdec: [i32; 5],
    hysteresis_mdec: i32,
    poll_secs: u64,
    settle_secs: u64,
}

const CONFIG: Config = Config {
    // <42 -> 0, 42 -> 1, 45 -> 2, 50 -> 3, 55 -> 4, 60 -> 5
    nvme_thresholds_mdec: [42_000, 45_000, 50_000, 55_000, 60_000],
    // <55 -> 0, 55 -> 1, 60 -> 2, 65 -> 3, 70 -> 4, 75 -> 5
    soc_thresholds_mdec: [55_000, 60_000, 65_000, 70_000, 75_000],
    hysteresis_mdec: 2_000,
    poll_secs: 5,
    settle_secs: 5,
};

/// Calibrated PWM for each state 0..=5. Must match `cooling-levels` in the DTS
/// overlay (state 0 is PWM 30, still spinning — never fan-off).
const PWM_LEVELS: [u32; 6] = [30, 50, 75, 100, 150, 255];

/// Failsafe is always the highest available state; derived from PWM_LEVELS so
/// the two cannot drift apart if states are added or removed.
const FAILSAFE_STATE: u32 = (PWM_LEVELS.len() - 1) as u32;

/// DT node name of the fan (function-named on purpose, not after a brand).
/// The hwmon device is matched by this exact node name, never by index.
const DT_NODE_NAME: &str = "nvme-fan";

/// Deliberately broad plausibility range; anything outside is treated as an
/// unavailable sensor, never fed to the policy.
const TEMP_SANE_MIN_MDEC: i32 = -40_000;
const TEMP_SANE_MAX_MDEC: i32 = 150_000;

/// Consecutive 0-RPM samples before the fan is declared stalled.
const TACH_ZERO_SAMPLES: u32 = 2;
/// Consecutive `fan1_input` read errors before the tach is declared faulty.
/// (Errors are sampled every poll interval, so this is ~15 s of blindness.)
const TACH_ERR_SAMPLES: u32 = 3;

// ---------------------------------------------------------------------------
// Small sysfs helpers
// ---------------------------------------------------------------------------

fn read_trimmed(path: &Path) -> std::io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

fn hwmon_devices() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hwmon") else {
        return out;
    };
    for ent in entries.flatten() {
        if ent.file_name().to_string_lossy().starts_with("hwmon") {
            out.push(ent.path());
        }
    }
    out.sort();
    out
}

fn fmt_temp(mdec: i32) -> String {
    let sign = if mdec < 0 { "-" } else { "" };
    let a = mdec.unsigned_abs();
    format!("{}{}.{} C", sign, a / 1000, (a % 1000) / 100)
}

fn fmt_opt_temp(v: Option<i32>) -> String {
    match v {
        Some(t) => fmt_temp(t),
        None => "unavailable".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Fan discovery: the nvme-fan hwmon is the single device we find and control.
//
// Primary: <hwmon>/device/of_node resolves to the DT node itself (exact).
// Fallback: a '/'-separated component of the canonical backing-device path
// equals it (covers kernels/layouts where of_node is not exposed). Both are
// exact matches, so a hypothetical `nvme-fan2` node can never collide.
// ---------------------------------------------------------------------------

/// True when the device's `of_node` symlink resolves to the DT node
/// `nvme-fan`. Uses the documented `of_node` relationship rather than
/// depending on a readable `of_node/name` property file.
fn of_node_matches(hwmon: &Path) -> bool {
    let Ok(node) = fs::canonicalize(hwmon.join("device/of_node")) else {
        return false;
    };
    node.file_name().and_then(|n| n.to_str()) == Some(DT_NODE_NAME)
}

/// True when a component of the backing-device path equals the DT node name
/// exactly.
fn device_path_matches_node(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == DT_NODE_NAME)
}

fn discover_fan_hwmon() -> Result<PathBuf, String> {
    let mut pwmfan_total = 0;
    let mut with_tach = 0;
    let mut with_pwm = 0;
    let mut matches = Vec::new();
    for hw in hwmon_devices() {
        if read_trimmed(&hw.join("name")).unwrap_or_default() != "pwmfan" {
            continue;
        }
        pwmfan_total += 1;
        let has_fan = hw.join("fan1_input").exists();
        let has_pwm = hw.join("pwm1").exists();
        if has_fan {
            with_tach += 1;
        }
        if has_pwm {
            with_pwm += 1;
        }
        // The stock CM3588 fan has no tachometer, so requiring fan1_input
        // already excludes it; the DT check below pins the exact device.
        if !has_fan || !has_pwm {
            continue;
        }
        let of_match = of_node_matches(&hw);
        let path_match = fs::canonicalize(hw.join("device"))
            .map(|p| device_path_matches_node(&p))
            .unwrap_or(false);
        if of_match || path_match {
            matches.push(hw);
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(format!(
            "no pwmfan hwmon with tach+pwm1 matches DT node \"{}\" \
             ({} pwmfan hwmon(s) total, {} with fan1_input, {} with pwm1); \
             is the DTS overlay applied and the tachometer wired? refusing to guess",
            DT_NODE_NAME, pwmfan_total, with_tach, with_pwm
        )),
        _ => {
            let list: Vec<String> = matches.iter().map(|p| p.display().to_string()).collect();
            Err(format!(
                "multiple pwmfan hwmons match DT node \"{}\": {}; refusing to guess",
                DT_NODE_NAME,
                list.join(", ")
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Temperature discovery: NVMe hwmons (primary) + package-thermal (safety).
// ---------------------------------------------------------------------------

struct NvmeSensor {
    /// e.g. "nvme0" for display; best effort, never used for control decisions.
    name: String,
    temp_input: PathBuf,
}

fn is_nvme_ctrl(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("nvme") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Best-effort display name for an NVMe hwmon (nearest `nvmeN` ancestor of its
/// backing device, else the hwmon dir name). Display only.
fn nvme_display_name(hwmon: &Path) -> String {
    if let Ok(dev) = fs::canonicalize(hwmon.join("device")) {
        for anc in dev.ancestors() {
            if let Some(s) = anc.file_name().and_then(|s| s.to_str()) {
                if is_nvme_ctrl(s) {
                    return s.to_string();
                }
            }
        }
    }
    hwmon
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("nvme?")
        .to_string()
}

fn discover_nvme_sensors() -> Vec<NvmeSensor> {
    let mut out = Vec::new();
    for hw in hwmon_devices() {
        if read_trimmed(&hw.join("name")).unwrap_or_default() != "nvme" {
            continue;
        }
        // Collect tempN_input files, sorted by N.
        let mut inputs: Vec<(u32, PathBuf)> = Vec::new();
        let Ok(entries) = fs::read_dir(&hw) else {
            continue;
        };
        for ent in entries.flatten() {
            let fname = ent.file_name().to_string_lossy().into_owned();
            if let Some(rest) = fname
                .strip_prefix("temp")
                .and_then(|s| s.strip_suffix("_input"))
            {
                if let Ok(n) = rest.parse::<u32>() {
                    inputs.push((n, ent.path()));
                }
            }
        }
        inputs.sort();
        if inputs.is_empty() {
            eprintln!(
                "warning: NVMe hwmon {} has no temp*_input files",
                hw.display()
            );
            continue;
        }
        // Prefer the Composite temperature; otherwise temp1_input; otherwise
        // the lowest-index input.
        let mut chosen: Option<PathBuf> = None;
        for (n, p) in &inputs {
            let label = hw.join(format!("temp{}_label", n));
            if read_trimmed(&label).unwrap_or_default() == "Composite" {
                chosen = Some(p.clone());
                break;
            }
        }
        if chosen.is_none() {
            for (n, p) in &inputs {
                if *n == 1 {
                    chosen = Some(p.clone());
                    break;
                }
            }
        }
        if chosen.is_none() {
            chosen = inputs.first().map(|(_, p)| p.clone());
        }
        out.push(NvmeSensor {
            name: nvme_display_name(&hw),
            temp_input: chosen.expect("inputs non-empty"),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn find_package_thermal() -> Option<PathBuf> {
    let mut zones = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/thermal") else {
        return None;
    };
    for ent in entries.flatten() {
        if ent.file_name().to_string_lossy().starts_with("thermal_zone") {
            zones.push(ent.path());
        }
    }
    zones.sort();
    for z in zones {
        if read_trimmed(&z.join("type")).unwrap_or_default() == "package-thermal" {
            return Some(z);
        }
    }
    None
}

fn read_temp_millidegrees(path: &Path) -> Result<i32, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let t: i32 = raw.trim().parse().map_err(|e| {
        format!(
            "cannot parse temperature in {} ({:?}): {}",
            path.display(),
            raw.trim(),
            e
        )
    })?;
    if !(TEMP_SANE_MIN_MDEC..=TEMP_SANE_MAX_MDEC).contains(&t) {
        return Err(format!(
            "implausible temperature {} in {} (expected -40..150 C); treating as unavailable",
            fmt_temp(t),
            path.display()
        ));
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// Policy curves + hysteresis
// ---------------------------------------------------------------------------

fn state_for(temp_mdec: i32, thresholds: &[i32; 5]) -> u32 {
    let mut s = 0;
    for t in thresholds.iter() {
        if temp_mdec >= *t {
            s += 1;
        } else {
            break;
        }
    }
    s
}

fn nvme_state(temp_mdec: i32) -> u32 {
    state_for(temp_mdec, &CONFIG.nvme_thresholds_mdec)
}

fn soc_state(temp_mdec: i32) -> u32 {
    state_for(temp_mdec, &CONFIG.soc_thresholds_mdec)
}

fn state_for_down(temp_mdec: i32, thresholds: &[i32; 5]) -> u32 {
    let mut s = 0;
    for t in thresholds.iter() {
        if temp_mdec >= *t - CONFIG.hysteresis_mdec {
            s += 1;
        } else {
            break;
        }
    }
    s
}

/// Raw (step-up) request: max of the two curves.
/// Failsafe: no sensors at all -> maximum state.
fn requested_state_up(hottest_nvme: Option<i32>, package: Option<i32>) -> u32 {
    let mut req = 0;
    let mut any = false;
    if let Some(t) = hottest_nvme {
        req = req.max(nvme_state(t));
        any = true;
    }
    if let Some(t) = package {
        req = req.max(soc_state(t));
        any = true;
    }
    if any {
        req
    } else {
        FAILSAFE_STATE
    }
}

fn requested_state_down(hottest_nvme: Option<i32>, package: Option<i32>) -> u32 {
    let mut req = 0;
    let mut any = false;
    if let Some(t) = hottest_nvme {
        req = req.max(state_for_down(t, &CONFIG.nvme_thresholds_mdec));
        any = true;
    }
    if let Some(t) = package {
        req = req.max(state_for_down(t, &CONFIG.soc_thresholds_mdec));
        any = true;
    }
    if any {
        req
    } else {
        FAILSAFE_STATE
    }
}

/// Step up immediately when a threshold is crossed; step down only once every
/// contributing temperature sits ~2 C below its threshold.
fn apply_hysteresis(current: u32, hottest_nvme: Option<i32>, package: Option<i32>) -> u32 {
    let up = requested_state_up(hottest_nvme, package);
    if up > current {
        return up;
    }
    let down = requested_state_down(hottest_nvme, package);
    if down < current {
        return down;
    }
    current
}

// ---------------------------------------------------------------------------
// Fan actuation (pwm1) + tachometer
// ---------------------------------------------------------------------------

fn read_pwm1(hwmon: &Path) -> Result<u32, String> {
    let path = hwmon.join("pwm1");
    let raw = read_trimmed(&path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let v: u32 = raw.parse().map_err(|e| {
        format!(
            "cannot parse PWM in {} ({:?}): {}",
            path.display(),
            raw,
            e
        )
    })?;
    if v > 255 {
        return Err(format!("implausible PWM {} in {}", v, path.display()));
    }
    Ok(v)
}

/// Map a raw PWM value back to a state: largest index whose level is <= pwm.
/// Exact levels map exactly (30->0 … 255->5); anything else rounds down for
/// hysteresis bookkeeping.
fn state_from_pwm(pwm: u32) -> u32 {
    let mut s = 0;
    for (i, lvl) in PWM_LEVELS.iter().enumerate() {
        if pwm >= *lvl {
            s = i as u32;
        } else {
            break;
        }
    }
    s
}

fn write_state_pwm(hwmon: &Path, state: u32) -> Result<(), String> {
    let pwm = PWM_LEVELS
        .get(state as usize)
        .ok_or_else(|| format!("invalid fan state {}", state))?;
    let path = hwmon.join("pwm1");
    fs::write(&path, format!("{}\n", pwm))
        .map_err(|e| format!("cannot write {}: {}", path.display(), e))
}

/// Demand the failsafe PWM; if the actuator itself is gone, exit so systemd
/// restarts us and discovery picks up the new path.
fn write_failsafe_or_exit(hwmon: &Path) {
    if let Err(e) = write_state_pwm(hwmon, FAILSAFE_STATE) {
        eprintln!("error: failed to write failsafe PWM: {}", e);
        std::process::exit(1);
    }
}

fn read_rpm(hwmon: &Path) -> Result<u32, String> {
    let path = hwmon.join("fan1_input");
    let raw = read_trimmed(&path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    raw.parse::<u32>().map_err(|e| {
        format!(
            "cannot parse RPM in {} ({:?}): {}",
            path.display(),
            raw,
            e
        )
    })
}

/// Tachometer fault tracking, fed by the every-iteration RPM sample.
struct TachMonitor {
    zero_streak: u32,
    err_streak: u32,
    /// Pinned at failsafe until a nonzero RPM is observed.
    fault: bool,
}

/// Observe one periodic RPM sample. A stall mid-state (requested state
/// unchanged for hours, fan then unplugged) is caught here, not only after a
/// state change. Silent while already faulted to avoid journal spam.
fn observe_tach_sample(mon: &mut TachMonitor, sample: Result<u32, String>) {
    match sample {
        Ok(0) => {
            mon.zero_streak += 1;
            mon.err_streak = 0;
            if mon.zero_streak >= TACH_ZERO_SAMPLES && !mon.fault {
                mon.fault = true;
                eprintln!(
                    "error: fan RPM is 0 for {} consecutive samples \
                     (possible stall/disconnect); demanding state {}",
                    mon.zero_streak, FAILSAFE_STATE
                );
            } else if !mon.fault {
                eprintln!(
                    "warning: fan RPM is 0 (sample {} of {}); watching",
                    mon.zero_streak, TACH_ZERO_SAMPLES
                );
            }
        }
        Ok(rpm) => {
            mon.zero_streak = 0;
            mon.err_streak = 0;
            if mon.fault {
                mon.fault = false;
                println!("fan recovered ({} RPM); resuming normal control", rpm);
            }
        }
        Err(e) => {
            mon.zero_streak = 0;
            mon.err_streak += 1;
            if mon.fault {
                return;
            }
            if mon.err_streak >= TACH_ERR_SAMPLES {
                mon.fault = true;
                eprintln!(
                    "error: cannot read fan1_input persistently \
                     ({} consecutive errors, last: {}); demanding state {}",
                    mon.err_streak, e, FAILSAFE_STATE
                );
            } else {
                eprintln!(
                    "warning: cannot read fan1_input ({}); {}/{} consecutive errors",
                    e, mon.err_streak, TACH_ERR_SAMPLES
                );
            }
        }
    }
}

/// After a state change, allow RPM to settle. Zero RPM at any state is abnormal
/// (state 0 still spins at PWM 30): demand state 5, wait, re-read. Records the
/// outcome in the tach monitor so the periodic path agrees.
fn settle_and_check_stall(hwmon: &Path, requested: u32, mon: &mut TachMonitor) {
    thread::sleep(Duration::from_secs(CONFIG.settle_secs));
    match read_rpm(hwmon) {
        Ok(0) => {
            eprintln!(
                "error: fan RPM is 0 after requesting state {} \
                 (possible stall/disconnect); demanding state {}",
                requested, FAILSAFE_STATE
            );
            if requested != FAILSAFE_STATE {
                write_failsafe_or_exit(hwmon);
            }
            thread::sleep(Duration::from_secs(CONFIG.settle_secs));
            match read_rpm(hwmon) {
                Ok(0) => {
                    eprintln!(
                        "error: fan still stalled/disconnected (fan1_input=0); \
                         remaining at state {}",
                        FAILSAFE_STATE
                    );
                    mon.zero_streak = TACH_ZERO_SAMPLES;
                    mon.err_streak = 0;
                    mon.fault = true;
                }
                Ok(rpm) => {
                    mon.zero_streak = 0;
                    mon.err_streak = 0;
                    mon.fault = false;
                    println!("fan tachometer recovered: {} RPM", rpm);
                }
                Err(e) => {
                    eprintln!(
                        "warning: cannot re-read fan1_input ({}); holding state {}",
                        e, FAILSAFE_STATE
                    );
                    mon.zero_streak = 0;
                    mon.err_streak = TACH_ERR_SAMPLES;
                    mon.fault = true;
                }
            }
        }
        Ok(_) => {
            mon.zero_streak = 0;
            mon.err_streak = 0;
        }
        Err(e) => eprintln!(
            "warning: cannot read fan1_input after state change ({}); \
             periodic monitor will retry",
            e
        ),
    }
}

// ---------------------------------------------------------------------------
// Modes: --once (dry run) and the control loop
// ---------------------------------------------------------------------------

fn run_once(hwmon: &Path) {
    // Dry run: never writes pwm1.
    let pwm = read_pwm1(hwmon).ok();
    let current = pwm.map(state_from_pwm);
    let rpm = read_rpm(hwmon).ok();
    let sensors = discover_nvme_sensors();
    let pkg_zone = find_package_thermal();

    let mut temp_fault = false;
    let mut hottest: Option<i32> = None;
    let mut per_drive: Vec<(String, Option<i32>)> = Vec::new();
    for s in &sensors {
        let t = match read_temp_millidegrees(&s.temp_input) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("warning: {}: {}", s.temp_input.display(), e);
                temp_fault = true;
                None
            }
        };
        if let Some(v) = t {
            hottest = Some(hottest.map_or(v, |h| h.max(v)));
        }
        per_drive.push((s.name.clone(), t));
    }
    let package = match pkg_zone.as_deref() {
        Some(z) => match read_temp_millidegrees(&z.join("temp")) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("warning: {}/temp: {}", z.display(), e);
                temp_fault = true;
                None
            }
        },
        None => None,
    };

    let nvme_req = hottest.map(nvme_state);
    let soc_req = package.map(soc_state);
    // Show what the daemon would actually choose, hysteresis included. An
    // unreadable pwm1 or a failed temperature read forces failsafe, same as
    // live.
    let final_state = match current {
        Some(c) if !temp_fault => apply_hysteresis(c, hottest, package),
        _ => FAILSAFE_STATE,
    };

    println!("Fan hwmon (nvme-fan):    {}", hwmon.display());
    match (pwm, current) {
        (Some(p), Some(c)) => println!("Current PWM/state:     {} (state {})", p, c),
        _ => println!("Current PWM/state:     unavailable"),
    }
    match rpm {
        Some(r) => println!("Current RPM:           {}", r),
        None => println!("Current RPM:           unavailable"),
    }
    match package {
        Some(t) => println!("package-thermal:       {}", fmt_temp(t)),
        None => println!("package-thermal:       unavailable"),
    }
    if per_drive.is_empty() {
        println!("NVMe:                  no sensors found");
    } else {
        for (name, t) in &per_drive {
            match t {
                Some(v) => println!("NVMe {:<16} {}", name, fmt_temp(*v)),
                None => println!("NVMe {:<16} unavailable", name),
            }
        }
    }
    match hottest {
        Some(t) => println!("hottest NVMe:          {}", fmt_temp(t)),
        None => println!("hottest NVMe:          unavailable"),
    }
    match nvme_req {
        Some(s) => println!("NVMe requested state:  {}", s),
        None => println!("NVMe requested state:  unavailable"),
    }
    match soc_req {
        Some(s) => println!("SoC requested state:   {}", s),
        None => println!("SoC requested state:   unavailable"),
    }
    println!("final state:           {} (PWM {})", final_state, PWM_LEVELS[final_state as usize]);
    println!("(dry run: pwm1 not modified)");
}

fn run_loop(hwmon: &Path) {
    println!("cm3588-nvme-fan starting: fan hwmon={}", hwmon.display());
    let mut mon = TachMonitor {
        zero_streak: 0,
        err_streak: 0,
        fault: false,
    };

    loop {
        // Re-enumerate each pass: cheap, and tolerates NVMe hot-plug/removal.
        let sensors = discover_nvme_sensors();
        if sensors.is_empty() {
            eprintln!("warning: no NVMe hwmon sensors found; falling back to SoC-only control");
        }
        let pkg_zone = find_package_thermal();
        if pkg_zone.is_none() {
            eprintln!("warning: package-thermal zone not found");
        }

        // Any previously visible sensor that fails to read latches a fault
        // for this iteration: losing sight of the hottest drive must never
        // reduce cooling. (Drives that vanish entirely are simply no longer
        // enumerated.) No last-value caching — just fail safe.
        let mut temp_fault = false;
        let mut hottest: Option<i32> = None;
        for s in &sensors {
            match read_temp_millidegrees(&s.temp_input) {
                Ok(t) => hottest = Some(hottest.map_or(t, |h| h.max(t))),
                Err(e) => {
                    eprintln!("warning: {}: {}", s.temp_input.display(), e);
                    temp_fault = true;
                }
            }
        }
        let package: Option<i32> = match pkg_zone.as_deref() {
            Some(z) => match read_temp_millidegrees(&z.join("temp")) {
                Ok(t) => Some(t),
                Err(e) => {
                    eprintln!("warning: {}/temp: {}", z.display(), e);
                    temp_fault = true;
                    None
                }
            },
            None => None,
        };
        if hottest.is_none() && package.is_none() {
            eprintln!(
                "warning: all temperature sensors unavailable; failsafe state {}",
                FAILSAFE_STATE
            );
        }

        // Current PWM comes from the device we can prove we own. On actuator
        // read error, demand failsafe and skip policy for this iteration: a
        // cool heatsink must never turn into state 0 when the actuator's
        // condition is unknown.
        let current_pwm = match read_pwm1(hwmon) {
            Ok(pwm) => pwm,
            Err(e) => {
                eprintln!(
                    "error: {}; demanding failsafe state {}",
                    e, FAILSAFE_STATE
                );
                write_failsafe_or_exit(hwmon);
                thread::sleep(Duration::from_secs(CONFIG.poll_secs));
                continue;
            }
        };
        let current = state_from_pwm(current_pwm);

        // Tachometer is sampled every iteration, whether or not the state
        // changes: a stall mid-state must still be caught. A latched fault
        // is cleared by the sampler itself on the next nonzero RPM.
        observe_tach_sample(&mut mon, read_rpm(hwmon));

        let desired = if mon.fault || temp_fault {
            FAILSAFE_STATE
        } else {
            apply_hysteresis(current, hottest, package)
        };

        let desired_pwm = PWM_LEVELS[desired as usize];
        // Compare raw PWM, not derived states: the daemon is authoritative
        // over its fan, so PWM 49 still gets corrected to the calibrated
        // state-0 value of 30 rather than being mistaken for "already 0".
        if current_pwm != desired_pwm {
            println!(
                "fan state {} -> {} (PWM {} -> {}; hottest NVMe: {}, SoC: {})",
                current,
                desired,
                current_pwm,
                desired_pwm,
                fmt_opt_temp(hottest),
                fmt_opt_temp(package)
            );
            match write_state_pwm(hwmon, desired) {
                Ok(()) => {
                    // Settle already slept; sample temperatures again
                    // immediately instead of waiting out another poll interval.
                    settle_and_check_stall(hwmon, desired, &mut mon);
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "error: failed to set fan state {}: {}; attempting failsafe state {}",
                        desired, e, FAILSAFE_STATE
                    );
                    // If the actuator is gone entirely (e.g. re-enumerated
                    // under a new hwmonN), exit so systemd restarts us and
                    // discovery picks up the new path.
                    if desired != FAILSAFE_STATE {
                        write_failsafe_or_exit(hwmon);
                    } else {
                        // The failed write already was the failsafe attempt.
                        std::process::exit(1);
                    }
                }
            }
        }

        thread::sleep(Duration::from_secs(CONFIG.poll_secs));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: cm3588-nvme-fan [--once|--dry-run] [--help]");
        println!("  --once, --dry-run   print discovery, temperatures and requested");
        println!("                      state; do not write pwm1");
        return;
    }
    let mut once = false;
    for a in &args {
        if a == "--once" || a == "--dry-run" {
            once = true;
        } else {
            eprintln!("error: unknown argument '{}'", a);
            eprintln!("usage: cm3588-nvme-fan [--once|--dry-run] [--help]");
            std::process::exit(2);
        }
    }

    let hwmon = match discover_fan_hwmon() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("error: cannot identify nvme-fan device: {}", e);
            std::process::exit(1);
        }
    };

    if once {
        run_once(&hwmon);
    } else {
        run_loop(&hwmon);
    }
}
