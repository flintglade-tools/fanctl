//! fanctl: read and control the MSI PRO Z690-A WIFI fan headers through `/dev/nct6687`.
//!
//! The `nct6687_rs` kernel driver returns every fan this program changed to its BIOS curve as
//! soon as the program exits, however it exits. Holding or running a curve therefore lasts
//! exactly as long as the process.

use std::{
    env, fs, fs::File, io, io::IsTerminal, os::fd::AsRawFd, process::ExitCode, thread,
    time::Duration,
};

const DEVICE: &str = "/dev/nct6687";
/// `_IOR('N', 0xa0, [u8; 56])` and `_IO('N', 0xa1)` from the driver.
const IOC_SNAPSHOT: libc::c_ulong = 0x8038_4ea0;
const IOC_SET_DUTY: libc::c_ulong = 0x4ea1;

const FANS: [(&str, &str); 8] = [
    ("cpu", "CPU_FAN1"),
    ("pump", "PUMP_FAN1"),
    ("sys1", "SYS_FAN1"),
    ("sys2", "SYS_FAN2"),
    ("sys3", "SYS_FAN3"),
    ("sys4", "SYS_FAN4"),
    ("sys5", "SYS_FAN5"),
    ("sys6", "SYS_FAN6"),
];
const TEMPS: [&str; 7] = [
    "CPU",
    "System",
    "VRM MOS",
    "PCH",
    "CPU Socket",
    "PCIe x1",
    "M2_1",
];

const USAGE: &str = "\
usage: fanctl                     show temperatures, clocks and fans once
       fanctl watch [SECONDS]     refresh every SECONDS (default 2) until Ctrl-C
       fanctl hold FAN=PCT...     hold fans at fixed duty until Ctrl-C (e.g. fanctl hold sys2=60)
       fanctl curve CONFIG        run temperature curves from CONFIG until stopped

FAN is cpu, pump or sys1..sys6. CPU_FAN1 and PUMP_FAN1 never go below 30 %, and go to 100 %
while the CPU is at 85 °C or more. When fanctl exits, every fan it changed returns to the BIOS curve.";

struct Snapshot {
    temps: [f32; 7],
    rpm: [u16; 8],
    duty: [u8; 8],
    manual: u8,
    held: u8,
}

struct Device(File);

impl Device {
    fn open(write: bool) -> Result<Self, String> {
        File::options()
            .read(true)
            .write(write)
            .open(DEVICE)
            .map(Device)
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => {
                    format!("{DEVICE} is missing: the nct6687_rs driver is not loaded")
                }
                io::ErrorKind::PermissionDenied => {
                    format!("{DEVICE}: permission denied (fan control needs the wheel group)")
                }
                _ => format!("{DEVICE}: {e}"),
            })
    }

    fn snapshot(&self) -> Result<Snapshot, String> {
        let mut raw = [0u8; 56];
        // SAFETY: The driver writes exactly 56 bytes for this request into the buffer we own.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), IOC_SNAPSHOT, raw.as_mut_ptr()) } < 0 {
            return Err(format!("reading sensors: {}", io::Error::last_os_error()));
        }
        Ok(Snapshot {
            temps: std::array::from_fn(|i| {
                i32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap()) as f32 / 1000.0
            }),
            rpm: std::array::from_fn(|i| u16::from_le_bytes([raw[28 + i * 2], raw[29 + i * 2]])),
            duty: std::array::from_fn(|i| raw[44 + i]),
            manual: raw[52],
            held: raw[53],
        })
    }

    /// Returns the duty the driver applied, which can exceed the request (floor or hot CPU).
    fn set(&self, fan: usize, percent: f32) -> Result<u8, String> {
        let raw = (percent.clamp(0.0, 100.0) * 2.55).round() as libc::c_ulong;
        // SAFETY: This request takes its argument by value; no memory is shared.
        let applied = unsafe {
            libc::ioctl(
                self.0.as_raw_fd(),
                IOC_SET_DUTY,
                (fan as libc::c_ulong) << 8 | raw,
            )
        };
        match applied {
            0.. => Ok(applied as u8),
            _ => Err(match io::Error::last_os_error().raw_os_error() {
                Some(libc::EBUSY) => "another fanctl already controls the fans (if it is the curve service: sudo systemctl stop fanctl)".into(),
                _ => format!("setting {}: {}", FANS[fan].0, io::Error::last_os_error()),
            }),
        }
    }
}

fn percent(raw: u8) -> u32 {
    (f32::from(raw) / 2.55).round() as u32
}

fn read_number(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// CPU package temperature, hottest core, average clock and any active performance caps.
fn cpu_line() -> String {
    let mut package = None;
    let mut hottest = None::<u64>;
    for hwmon in fs::read_dir("/sys/class/hwmon")
        .into_iter()
        .flatten()
        .flatten()
    {
        let dir = hwmon.path();
        if fs::read_to_string(dir.join("name")).is_ok_and(|n| n.trim() == "coretemp") {
            for i in 1..64 {
                let (Ok(label), Some(value)) = (
                    fs::read_to_string(dir.join(format!("temp{i}_label"))),
                    read_number(&dir.join(format!("temp{i}_input")).to_string_lossy()),
                ) else {
                    continue;
                };
                if label.starts_with("Package") {
                    package = Some(value)
                } else {
                    hottest = hottest.max(Some(value))
                }
            }
        }
    }
    let clocks: Vec<u64> = (0..256)
        .map_while(|cpu| {
            read_number(&format!(
                "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq"
            ))
        })
        .collect();
    let mut line = match package {
        Some(t) => format!("CPU {:.0} °C", t as f32 / 1000.0),
        None => "CPU temperature unavailable".into(),
    };
    if let Some(t) = hottest {
        line += &format!(" (hottest core {:.0} °C)", t as f32 / 1000.0);
    }
    if !clocks.is_empty() {
        line += &format!(
            " · {} MHz average",
            clocks.iter().sum::<u64>() / clocks.len() as u64 / 1000
        );
    }
    if let Some(cap) =
        read_number("/sys/devices/system/cpu/intel_pstate/max_perf_pct").filter(|&c| c < 100)
    {
        line += &format!(" · capped at {cap} % of max speed");
    }
    if read_number("/sys/devices/system/cpu/intel_pstate/no_turbo") == Some(1) {
        line += " · turbo off";
    }
    line
}

fn report(snapshot: &Snapshot) -> String {
    let mut out = cpu_line() + "\n";
    let temps: Vec<String> = TEMPS
        .iter()
        .zip(snapshot.temps)
        .map(|(name, t)| format!("{name} {t:.1}"))
        .collect();
    out += &format!("Board sensors (°C): {}\n", temps.join(" · "));
    out += "fan   header       RPM  duty  control\n";
    for (i, (name, header)) in FANS.iter().enumerate() {
        let control = if snapshot.held & 1 << i != 0 {
            "fanctl"
        } else if snapshot.manual & 1 << i != 0 {
            "manual (set outside fanctl)"
        } else {
            "BIOS curve"
        };
        out += &format!(
            "{name:<5} {header:<10} {:>5}  {:>3}%  {control}\n",
            snapshot.rpm[i],
            percent(snapshot.duty[i])
        );
    }
    out
}

fn fan_index(name: &str) -> Result<usize, String> {
    FANS.iter()
        .position(|(n, _)| *n == name)
        .ok_or_else(|| format!("unknown fan '{name}' (use cpu, pump or sys1..sys6)"))
}

fn number(text: &str, what: &str) -> Result<f32, String> {
    text.parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("invalid {what} '{text}'"))
}

fn watch(seconds: f32) -> Result<(), String> {
    let device = Device::open(false)?;
    let clear = io::stdout().is_terminal();
    loop {
        let text = report(&device.snapshot()?);
        if clear {
            print!("\x1b[H\x1b[2J")
        }
        println!("{text}");
        thread::sleep(Duration::from_secs_f32(seconds));
    }
}

fn hold(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err(USAGE.into());
    }
    let mut targets = Vec::new();
    for arg in args {
        let (fan, pct) = arg
            .split_once('=')
            .ok_or_else(|| format!("expected FAN=PCT, got '{arg}'"))?;
        let pct = number(pct.trim_end_matches('%'), "percentage")?;
        if !(0.0..=100.0).contains(&pct) {
            return Err(format!("{arg}: percentage must be 0–100"));
        }
        targets.push((fan_index(fan)?, pct));
    }
    let device = Device::open(true)?;
    for &(fan, pct) in &targets {
        let applied = percent(device.set(fan, pct)?);
        let note = if applied as f32 > pct + 0.5 {
            " (raised by the driver: 30 % floor or CPU ≥ 85 °C)"
        } else {
            ""
        };
        println!("{} held at {applied} %{note}", FANS[fan].0);
    }
    println!("Ctrl-C returns these fans to the BIOS curve.\n");
    loop {
        thread::sleep(Duration::from_secs(2));
        // Re-sending lets the driver apply its hot-CPU override if the temperature rises.
        for &(fan, pct) in &targets {
            device.set(fan, pct)?;
        }
        let snapshot = device.snapshot()?;
        let fans: Vec<String> = targets
            .iter()
            .map(|&(fan, _)| {
                format!(
                    "{} {} RPM @ {} %",
                    FANS[fan].0,
                    snapshot.rpm[fan],
                    percent(snapshot.duty[fan])
                )
            })
            .collect();
        println!("CPU {:.1} °C · {}", snapshot.temps[0], fans.join(" · "));
    }
}

/// Temperature-to-duty points, linear in between and flat beyond the ends.
struct Curve {
    fan: usize,
    points: Vec<(f32, f32)>,
}

impl Curve {
    fn duty_at(&self, temp: f32) -> f32 {
        let first = self.points[0];
        if temp <= first.0 {
            return first.1;
        }
        for pair in self.points.windows(2) {
            let ((t0, d0), (t1, d1)) = (pair[0], pair[1]);
            if temp <= t1 {
                return d0 + (d1 - d0) * (temp - t0) / (t1 - t0);
            }
        }
        self.points[self.points.len() - 1].1
    }
}

fn parse_curves(text: &str) -> Result<Vec<Curve>, String> {
    let mut curves: Vec<Curve> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut words = line.split_whitespace();
        let Some(fan) = words.next() else { continue };
        let at = |e: String| format!("line {}: {e}", n + 1);
        let fan = fan_index(fan).map_err(at)?;
        if curves.iter().any(|c| c.fan == fan) {
            return Err(at(format!("{} has two curves", FANS[fan].0)));
        }
        let mut points = Vec::new();
        for word in words {
            let (t, d) = word
                .split_once(':')
                .ok_or_else(|| at(format!("expected TEMP:PCT, got '{word}'")))?;
            let (t, d) = (
                number(t, "temperature").map_err(at)?,
                number(d.trim_end_matches('%'), "percentage").map_err(at)?,
            );
            if !(0.0..=100.0).contains(&d) {
                return Err(at(format!("{word}: percentage must be 0–100")));
            }
            if points.last().is_some_and(|&(last, _)| t <= last) {
                return Err(at("temperatures must increase from left to right".into()));
            }
            points.push((t, d));
        }
        if points.is_empty() {
            return Err(at(format!("{} has no TEMP:PCT points", FANS[fan].0)));
        }
        curves.push(Curve { fan, points });
    }
    if curves.is_empty() {
        return Err("the config has no curves; fans stay on the BIOS curve".into());
    }
    Ok(curves)
}

/// Follows the curves once per second. Duty rises immediately and falls at most 2 % per second,
/// so short load spikes do not make the fans pulse.
fn curve(path: &str) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let curves = parse_curves(&text)?;
    let device = Device::open(true)?;
    let mut current: Vec<f32> = vec![0.0; curves.len()];
    let mut last_report = None;
    loop {
        let temp = device.snapshot()?.temps[0];
        let mut applied = Vec::new();
        for (curve, current) in curves.iter().zip(&mut current) {
            let target = curve.duty_at(temp);
            *current = if target >= *current {
                target
            } else {
                (*current - 2.0).max(target)
            };
            applied.push(format!(
                "{} {} %",
                FANS[curve.fan].0,
                percent(device.set(curve.fan, *current)?)
            ));
        }
        let line = applied.join(" · ");
        if last_report.as_ref() != Some(&line) {
            println!("CPU {temp:.1} °C → {line}");
            last_report = Some(line);
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Ctrl-Z would stop the process with the device still open, leaving the fans at the held duty
/// while nothing re-applies the driver's hot-CPU override. Holding and curves ignore it.
fn ignore_terminal_stop() {
    // SAFETY: Setting a signal's disposition to SIG_IGN installs no handler code.
    unsafe { libc::signal(libc::SIGTSTP, libc::SIG_IGN) };
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None => {
            print!("{}", report(&Device::open(false)?.snapshot()?));
            Ok(())
        }
        Some("watch") => watch(match args.get(1) {
            Some(s) => number(s, "interval")
                .ok()
                .filter(|s| (0.5..=3600.0).contains(s))
                .ok_or("interval must be 0.5–3600 s")?,
            None => 2.0,
        }),
        Some("hold") => {
            ignore_terminal_stop();
            hold(&args[1..])
        }
        Some("curve") if args.len() == 2 => {
            ignore_terminal_stop();
            curve(&args[1])
        }
        _ => Err(USAGE.into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_interpolates_and_clamps() {
        let curves = parse_curves("cpu 40:30 60:50 80:100  # comment\n").unwrap();
        let c = &curves[0];
        assert_eq!(c.fan, 0);
        assert_eq!(c.duty_at(20.0), 30.0);
        assert_eq!(c.duty_at(50.0), 40.0);
        assert_eq!(c.duty_at(70.0), 75.0);
        assert_eq!(c.duty_at(95.0), 100.0);
    }

    #[test]
    fn curve_config_rejects_mistakes() {
        assert!(parse_curves("").is_err());
        assert!(parse_curves("cpu 60:30 40:50").is_err());
        assert!(parse_curves("cpu 40:130").is_err());
        assert!(parse_curves("gpu 40:30").is_err());
        assert!(parse_curves("cpu 40:30\ncpu 50:40").is_err());
        assert!(parse_curves("sys2").is_err());
    }

    #[test]
    fn ioctl_numbers_match_the_driver() {
        // _IOC(dir, type, nr, size) = dir << 30 | size << 16 | type << 8 | nr
        assert_eq!(
            IOC_SNAPSHOT,
            2 << 30 | 56 << 16 | (b'N' as libc::c_ulong) << 8 | 0xa0
        );
        assert_eq!(IOC_SET_DUTY, (b'N' as libc::c_ulong) << 8 | 0xa1);
    }
}
