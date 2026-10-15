//! Devices in the daemon (D7, D8): state providers, bindings, actions and
//! their approval.
//!
//! The daemon finds device scripts in `devices/*.js` of the room (the
//! proposal of Q5, D6) and in `agents/<id>/devices/*.js` (D21), builds them
//! in the sandbox and checks them. It polls the state sources (airplane
//! mode, the clock, the CPU, memory and network every few seconds, the disk
//! every minute), turns the readings into param values through each binding
//! and sends them as [`Event::DeviceParam`]. So the bindings work both ways.
//! Switching airplane mode in the system settings moves the plane as well as
//! a click on it does.
//!
//! Settings of a device, such as the metric of the system graph, are params
//! the user picks on the device card (`DeviceSetting`). The daemon keeps them
//! in `devices/settings.toml` of the room beside the grants, sends them like
//! bound values and lets the bindings made with `per()` follow them.
//!
//! Actions run only through declared capabilities (D8). A device of the space
//! belongs to the user, who approves its capability once through the
//! capability broker: the same card, the same `DecideCapability` command and
//! the same audit log as the requests of agents. Always allow writes the
//! grant into `devices/grants.toml` of the room. A device an agent made may only
//! bind and use what its agent already holds, and a device that names more
//! does not load. Agents make devices by writing scripts into their
//! directory, which the daemon picks up within a few seconds.
//!
//! The real system is behind [`System`]. Unit tests always run against
//! [`FakeSystem`], so they never switch the radios of the machine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use meerora_protocol::{DeviceActionInfo, DeviceFile, DeviceInfo, Event, RingState, device_files};
use meerora_script::{ActionKind, DeviceDesc, Effect, Limits, Source, build_device};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::capabilities::{DeviceEntry, DeviceRequest};
use crate::daemon::{Daemon, Outgoing};

/// How often the daemon looks for new, changed and removed device scripts.
const SCAN_EVERY: Duration = Duration::from_secs(3);
/// How often airplane mode, the clock, the CPU, memory and network are read.
const FAST_EVERY: Duration = Duration::from_secs(3);
/// How often the disk is read.
const DISK_EVERY: Duration = Duration::from_secs(60);
/// The capabilities a device action may use, with a backend each. Devices
/// never run arbitrary commands.
const BACKENDS: [&str; 1] = ["network.airplane_mode.write"];
/// The grants of the space's devices, in `devices/` of the room.
const GRANTS: &str = "grants.toml";
/// The settings of the devices, beside the grants.
const SETTINGS: &str = "settings.toml";

/// Bytes received and sent so far, and when, for a rate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetSample {
    pub bytes: u64,
    /// Seconds on a clock that only goes forward.
    pub at: f64,
}

/// The system state behind the sources and actions.
pub trait System: Send {
    /// The share of the home filesystem in use, from 0 to 1.
    fn disk_used(&self) -> Result<f64, String>;
    /// Busy and total time of all CPUs since boot, in clock ticks.
    fn cpu_times(&self) -> Result<(u64, u64), String>;
    /// The share of memory that is available, from 0 to 1.
    fn memory_available(&self) -> Result<f64, String>;
    /// Bytes received and sent by every network interface but loopback.
    fn network_bytes(&self) -> Result<NetSample, String>;
    /// True while every radio is off.
    fn airplane_mode(&self) -> Result<bool, String>;
    /// Switches airplane mode. Runs only after the capability check (D8).
    fn set_airplane_mode(&self, on: bool) -> Result<(), String>;
    /// The local time in minutes since midnight.
    fn minutes(&self) -> u32;
}

/// The machine: `statvfs` of the home directory, rfkill for the radios,
/// `/proc` for the CPU, memory and network and the local clock.
#[cfg_attr(test, allow(dead_code))]
pub struct RealSystem {
    pub home: PathBuf,
    /// `/sys/class/rfkill`.
    pub rfkill: PathBuf,
    /// `/proc`.
    pub proc: PathBuf,
}

#[cfg_attr(test, allow(dead_code))]
impl RealSystem {
    pub fn new() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        Self { home, rfkill: PathBuf::from("/sys/class/rfkill"), proc: PathBuf::from("/proc") }
    }

    fn read(&self, name: &str) -> Result<String, String> {
        let path = self.proc.join(name);
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read {} ({e})", path.display()))
    }
}

/// Seconds since the first call, on a clock that only goes forward.
fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

impl System for RealSystem {
    fn disk_used(&self) -> Result<f64, String> {
        disk_used(&self.home)
    }

    fn cpu_times(&self) -> Result<(u64, u64), String> {
        cpu_times(&self.read("stat")?)
    }

    fn memory_available(&self) -> Result<f64, String> {
        memory_available(&self.read("meminfo")?)
    }

    fn network_bytes(&self) -> Result<NetSample, String> {
        let text = self.read("net/dev")?;
        Ok(NetSample { bytes: network_bytes(&text)?, at: monotonic() })
    }

    fn airplane_mode(&self) -> Result<bool, String> {
        radios_off(&self.rfkill)
    }

    fn set_airplane_mode(&self, on: bool) -> Result<(), String> {
        let mut failures = Vec::new();
        for command in airplane_commands(on) {
            match Process::new(command[0]).args(&command[1..]).output() {
                Ok(out) if out.status.success() => return Ok(()),
                Ok(out) => failures.push(format!("{} failed ({})", command.join(" "), String::from_utf8_lossy(&out.stderr).trim())),
                Err(e) => failures.push(format!("{} could not start ({e})", command.join(" "))),
            }
        }
        Err(failures.join(", "))
    }

    fn minutes(&self) -> u32 {
        let now = jiff::Zoned::now();
        now.hour() as u32 * 60 + now.minute() as u32
    }
}

/// The commands that switch every radio off (`on` is airplane mode on) or
/// back on, in the order they are tried.
pub fn airplane_commands(on: bool) -> [Vec<&'static str>; 2] {
    let (rfkill, nmcli) = if on { ("block", "off") } else { ("unblock", "on") };
    [vec!["rfkill", rfkill, "all"], vec!["nmcli", "radio", "all", nmcli]]
}

/// Like `df`, the used blocks over the used and the available ones.
pub fn disk_used(path: &Path) -> Result<f64, String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: statvfs only writes into the struct we pass, and `c` is a valid C string.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return Err(format!("cannot read the filesystem of {} ({})", path.display(), std::io::Error::last_os_error()));
    }
    let used = stat.f_blocks.saturating_sub(stat.f_bfree) as f64;
    let total = used + stat.f_bavail as f64;
    if total <= 0.0 {
        return Err(format!("the filesystem of {} reports no blocks", path.display()));
    }
    Ok(used / total)
}

/// Busy and total ticks of the `cpu` line of `/proc/stat`. Idle and iowait
/// count as idle. Guest time is already part of user time, so it is left out.
pub fn cpu_times(stat: &str) -> Result<(u64, u64), String> {
    let line = stat.lines().find(|l| l.starts_with("cpu ")).ok_or("/proc/stat has no cpu line")?;
    let fields: Vec<u64> = line.split_whitespace().skip(1).take(8).map(|f| f.parse().unwrap_or(0)).collect();
    if fields.len() < 4 {
        return Err(format!("/proc/stat has a short cpu line: {line}"));
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Ok((total.saturating_sub(idle), total))
}

/// MemAvailable over MemTotal of `/proc/meminfo`, from 0 to 1.
pub fn memory_available(meminfo: &str) -> Result<f64, String> {
    let field = |name: &str| -> Option<f64> {
        let line = meminfo.lines().find(|l| l.starts_with(name))?;
        line[name.len()..].split_whitespace().next()?.parse().ok()
    };
    let total = field("MemTotal:").filter(|t| *t > 0.0).ok_or("/proc/meminfo has no MemTotal")?;
    let available = field("MemAvailable:").ok_or("/proc/meminfo has no MemAvailable")?;
    Ok((available / total).clamp(0.0, 1.0))
}

/// Received plus sent bytes of every interface in `/proc/net/dev` but loopback.
pub fn network_bytes(dev: &str) -> Result<u64, String> {
    let mut total = 0u64;
    let mut interfaces = 0;
    for line in dev.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        if name.trim() == "lo" {
            continue;
        }
        let fields: Vec<u64> = rest.split_whitespace().map(|f| f.parse().unwrap_or(0)).collect();
        // Receive bytes come first, transmit bytes are the ninth field.
        if fields.len() >= 9 {
            total = total.saturating_add(fields[0]).saturating_add(fields[8]);
            interfaces += 1;
        }
    }
    if interfaces == 0 {
        return Err("/proc/net/dev lists no network interface but loopback".into());
    }
    Ok(total)
}

/// Airplane mode as rfkill sees it. True when the computer has radios and
/// every one of them is blocked.
pub fn radios_off(rfkill: &Path) -> Result<bool, String> {
    let entries = std::fs::read_dir(rfkill).map_err(|e| format!("cannot read {} ({e})", rfkill.display()))?;
    let flag = |dir: &Path, name: &str| std::fs::read_to_string(dir.join(name)).is_ok_and(|s| s.trim() == "1");
    let mut radios = 0;
    let mut blocked = 0;
    for dir in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
        if !dir.join("soft").exists() {
            continue;
        }
        radios += 1;
        if flag(&dir, "soft") || flag(&dir, "hard") {
            blocked += 1;
        }
    }
    if radios == 0 {
        return Err("this computer has no radios that rfkill knows".into());
    }
    Ok(blocked == radios)
}

/// A system for tests. Clones share the state, so a test keeps one to
/// change the state behind the daemon's back.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeSystem {
    pub state: Arc<Mutex<FakeState>>,
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub struct FakeState {
    pub disk: f64,
    pub airplane: bool,
    pub minutes: u32,
    /// Busy and total CPU ticks since boot.
    pub cpu: (u64, u64),
    pub memory: f64,
    pub network: NetSample,
    /// Every `set_airplane_mode` call.
    pub writes: Vec<bool>,
    /// Makes the next write fail with this message.
    pub fail: Option<String>,
}

#[cfg(test)]
impl System for FakeSystem {
    fn disk_used(&self) -> Result<f64, String> {
        Ok(self.state.lock().unwrap_or_else(|e| e.into_inner()).disk)
    }

    fn cpu_times(&self) -> Result<(u64, u64), String> {
        Ok(self.state.lock().unwrap_or_else(|e| e.into_inner()).cpu)
    }

    fn memory_available(&self) -> Result<f64, String> {
        Ok(self.state.lock().unwrap_or_else(|e| e.into_inner()).memory)
    }

    fn network_bytes(&self) -> Result<NetSample, String> {
        Ok(self.state.lock().unwrap_or_else(|e| e.into_inner()).network)
    }

    fn airplane_mode(&self) -> Result<bool, String> {
        Ok(self.state.lock().unwrap_or_else(|e| e.into_inner()).airplane)
    }

    fn set_airplane_mode(&self, on: bool) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = state.fail.take() {
            return Err(e);
        }
        state.writes.push(on);
        state.airplane = on;
        Ok(())
    }

    fn minutes(&self) -> u32 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).minutes
    }
}

/// What the user allowed one device of the space, in `space/devices/grants.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct GrantEntry {
    #[serde(default)]
    granted: Vec<String>,
}

/// A device script and what came of it.
struct Loaded {
    file: DeviceFile,
    modified: Option<SystemTime>,
    /// The start of the SHA-256 of the script, see [`DeviceInfo::revision`].
    revision: String,
    desc: Result<DeviceDesc, String>,
}

/// The daemon's devices: scripts, readings and what the clients know.
pub(crate) struct DeviceHub {
    data_dir: PathBuf,
    /// The folder of the room whose devices are loaded, see `meerora_protocol::space_dir`.
    space: PathBuf,
    system: Box<dyn System>,
    loaded: BTreeMap<String, Loaded>,
    readings: BTreeMap<Source, Value>,
    /// The previous CPU ticks and network bytes, for the next share and rate.
    last_cpu: Option<(u64, u64)>,
    last_net: Option<NetSample>,
    /// What the user picked on the device cards, by device and setting.
    settings: BTreeMap<String, BTreeMap<String, Value>>,
    next_scan: Instant,
    next_fast: Instant,
    next_disk: Instant,
    /// The device list the clients have, see [`Event::Devices`].
    sent: Vec<DeviceInfo>,
    /// The param values the clients have, by device and param.
    values: BTreeMap<(String, String), Value>,
    grants: BTreeMap<String, GrantEntry>,
    /// The last load problem reported to the agent that made a device.
    reported: BTreeMap<String, Option<String>>,
}

impl DeviceHub {
    pub fn new(data_dir: &Path) -> Self {
        #[cfg(not(test))]
        let system: Box<dyn System> = Box::new(RealSystem::new());
        // Tests never touch the radios of the machine.
        #[cfg(test)]
        let system: Box<dyn System> = Box::new(FakeSystem::default());
        let now = Instant::now();
        let mut hub = Self {
            data_dir: data_dir.to_path_buf(),
            space: meerora_protocol::space_dir(data_dir),
            system,
            loaded: BTreeMap::new(),
            readings: BTreeMap::new(),
            last_cpu: None,
            last_net: None,
            settings: BTreeMap::new(),
            next_scan: now,
            next_fast: now,
            next_disk: now,
            sent: Vec::new(),
            values: BTreeMap::new(),
            grants: BTreeMap::new(),
            reported: BTreeMap::new(),
        };
        hub.grants = hub.read_grants();
        hub.settings = hub.read_settings();
        hub
    }

    /// Replaces the system, for tests.
    #[cfg(test)]
    pub fn set_system(&mut self, system: Box<dyn System>) {
        self.system = system;
        self.next_fast = Instant::now();
        self.next_disk = Instant::now();
    }

    /// Makes the next tick scan the scripts and read every source, for tests.
    #[cfg(test)]
    pub fn due_now(&mut self) {
        let now = Instant::now();
        (self.next_scan, self.next_fast, self.next_disk) = (now, now, now);
    }

    /// Looks for device scripts again on the next tick.
    pub fn rescan_soon(&mut self) {
        self.next_scan = Instant::now();
    }

    /// The next time the hub has something to do.
    pub fn next_due(&self) -> Instant {
        self.next_scan.min(self.next_fast).min(self.next_disk)
    }

    fn grants_path(&self) -> PathBuf {
        self.space.join("devices").join(GRANTS)
    }

    /// The grant file as the user finds it below the data dir.
    fn grants_name(&self) -> String {
        let path = self.grants_path();
        path.strip_prefix(&self.data_dir).unwrap_or(&path).display().to_string()
    }

    fn settings_path(&self) -> PathBuf {
        self.space.join("devices").join(SETTINGS)
    }

    fn read_settings(&self) -> BTreeMap<String, BTreeMap<String, Value>> {
        let path = self.settings_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                warn!("ignoring {} because it does not parse ({e})", path.display());
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        }
    }

    fn write_settings(&self) -> Result<(), String> {
        let path = self.settings_path();
        let body = toml::to_string(&self.settings).map_err(|e| e.to_string())?;
        let text = format!(
            "# What the devices of the space show (D7). meerora writes this file when you pick\n\
             # a setting on a device card, such as the metric of the system graph.\n\n{body}"
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path)).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The current values of the settings of a device, defaults included.
    fn setting_values(&self, id: &str, desc: &DeviceDesc) -> BTreeMap<String, Value> {
        let stored = self.settings.get(id);
        desc.settings
            .iter()
            .filter_map(|s| {
                let def = desc.param(&s.param)?;
                let value = stored.and_then(|m| m.get(&s.param)).filter(|v| def.accepts(v)).cloned().unwrap_or_else(|| def.default_value());
                Some((s.param.clone(), value))
            })
            .collect()
    }

    fn read_grants(&self) -> BTreeMap<String, GrantEntry> {
        let path = self.grants_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                warn!("ignoring {} because it does not parse ({e})", path.display());
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        }
    }

    fn write_grants(&self) -> Result<(), String> {
        let path = self.grants_path();
        let body = toml::to_string(&self.grants).map_err(|e| e.to_string())?;
        let text = format!(
            "# What you allowed the devices of the space (D8). meerora writes this file when you\n\
             # choose Always allow, and every change is logged in audit.jsonl.\n\n{body}"
        );
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path)).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn space_grants(&self, device: &str) -> Vec<String> {
        self.grants.get(device).map(|g| g.granted.clone()).unwrap_or_default()
    }

    /// Builds new and changed scripts and drops removed ones. Returns true
    /// when something changed. A switch to another room brings its grants
    /// and settings.
    fn scan(&mut self) -> bool {
        let mut changed = false;
        let space = meerora_protocol::space_dir(&self.data_dir);
        if space != self.space {
            info!("the space is now {}", space.display());
            self.space = space;
            self.grants = self.read_grants();
            self.settings = self.read_settings();
            changed = true;
        }
        let files = device_files(&self.data_dir);
        let before = self.loaded.len();
        self.loaded.retain(|id, _| files.iter().any(|f| f.id == *id));
        changed |= self.loaded.len() != before;
        for file in files {
            let modified = std::fs::metadata(&file.path).and_then(|m| m.modified()).ok();
            if self.loaded.get(&file.id).is_some_and(|l| l.modified.is_some() && l.modified == modified && l.file == file) {
                continue;
            }
            let (revision, desc) = build(&self.data_dir, &file);
            if let Err(e) = &desc {
                info!("device {} does not build ({e})", file.id);
            }
            self.loaded.insert(file.id.clone(), Loaded { file, modified, revision, desc });
            changed = true;
        }
        changed
    }

    /// Reads the sources that are due. Returns true when a reading changed.
    fn poll(&mut self, now: Instant, force: bool) -> bool {
        let mut readings = Vec::new();
        if force || now >= self.next_fast {
            self.next_fast = now + FAST_EVERY;
            readings.push((Source::Clock, Ok(Value::from(self.system.minutes()))));
            readings.push((Source::AirplaneMode, self.system.airplane_mode().map(Value::Bool)));
            if let Some(share) = self.cpu_share() {
                readings.push((Source::Cpu, share.map(Value::from)));
            }
            readings.push((Source::Memory, self.system.memory_available().map(Value::from)));
            if let Some(rate) = self.network_rate() {
                readings.push((Source::Network, rate.map(Value::from)));
            }
        }
        if force || now >= self.next_disk {
            self.next_disk = now + DISK_EVERY;
            readings.push((Source::Disk, self.system.disk_used().map(Value::from)));
        }
        let mut changed = false;
        for (source, reading) in readings {
            match reading {
                Ok(value) => changed |= self.readings.insert(source, value.clone()).as_ref() != Some(&value),
                Err(e) => {
                    if self.readings.remove(&source).is_some() {
                        warn!("{source} cannot be read any more ({e})");
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// The share of CPU time in use since the last reading. The first
    /// reading gives the share since boot. None keeps the last reading,
    /// when the counters did not move since.
    fn cpu_share(&mut self) -> Option<Result<f64, String>> {
        let (busy, total) = match self.system.cpu_times() {
            Ok(times) => times,
            Err(e) => return Some(Err(e)),
        };
        if self.last_cpu.is_some_and(|(_, t)| t == total) {
            return None;
        }
        // Counters that went back (they never should) count from boot again.
        let (b0, t0) = self.last_cpu.filter(|(b, t)| busy >= *b && total > *t).unwrap_or((0, 0));
        self.last_cpu = Some((busy, total));
        (total > t0).then(|| Ok((busy.saturating_sub(b0) as f64 / (total - t0) as f64).clamp(0.0, 1.0)))
    }

    /// Bytes per second since the last reading. None for the first reading,
    /// which has nothing to compare with.
    fn network_rate(&mut self) -> Option<Result<f64, String>> {
        let now = match self.system.network_bytes() {
            Ok(now) => now,
            Err(e) => return Some(Err(e)),
        };
        let before = self.last_net.replace(now)?;
        let seconds = now.at - before.at;
        if seconds <= 0.0 || now.bytes < before.bytes {
            // A counter that went back (an interface went away) starts over.
            return None;
        }
        Some(Ok((now.bytes - before.bytes) as f64 / seconds))
    }
}

/// Builds a device script in the sandbox. Returns its revision too.
fn build(data_dir: &Path, file: &DeviceFile) -> (String, Result<DeviceDesc, String>) {
    use sha2::{Digest, Sha256};
    let name = file.path.strip_prefix(data_dir).unwrap_or(&file.path).display().to_string();
    let source = match std::fs::read_to_string(&file.path) {
        Ok(source) => source,
        Err(e) => return (String::new(), Err(format!("cannot read {name} ({e})"))),
    };
    let revision = Sha256::digest(source.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    (revision, build_device(&source, &name, &Limits::default()).map_err(|e| e.to_string()))
}

/// Why a built device may not load, if it may not. `owner` is the name of
/// the agent that made it and what that agent holds.
fn problem(name: &str, desc: &DeviceDesc, stations: &[String], owner: Option<(&str, &[String])>) -> Option<String> {
    if !good_name(name) {
        return Some(format!("{name}.js is not a device name. Use lowercase letters, digits, - and _, for example paper_plane.js."));
    }
    if let Some(station) = &desc.place.station
        && !stations.contains(station)
    {
        return Some(format!("{station:?} is not a station. The stations are {}.", stations.join(", ")));
    }
    let unsupported: Vec<String> =
        desc.actions.iter().filter_map(|a| a.capability()).filter(|c| !BACKENDS.contains(c)).map(String::from).collect();
    if !unsupported.is_empty() {
        return Some(format!(
            "meerora has no device backend for {}. Device actions can use {} so far.",
            list(&unsupported),
            BACKENDS.join(", ")
        ));
    }
    if let Some((who, held)) = owner {
        // An agent's device binds and uses only what its agent holds (D8).
        let missing: Vec<String> = desc.capabilities().into_iter().filter(|c| !held.contains(c)).collect();
        if !missing.is_empty() {
            return Some(format!(
                "{who} does not hold {}, so the device cannot use it. Ask for it with request_capability first.",
                list(&missing)
            ));
        }
    }
    None
}

/// One line per device for `meerora-agentd check`, and the problems of
/// those that would not load. `agents` gives the capabilities the owners hold in
/// their `agent.toml`.
pub fn check(data_dir: &Path, stations: &[String], agents: &[meerora_protocol::AgentDir]) -> (Vec<String>, Vec<String>) {
    let (mut lines, mut problems) = (Vec::new(), Vec::new());
    for file in device_files(data_dir) {
        let name = file.id.rsplit('/').next().unwrap_or(&file.id).to_string();
        let problem = match build(data_dir, &file).1 {
            Err(e) => Some(format!("does not build ({e})")),
            Ok(desc) => {
                let owner = file.owner.as_ref().map(|agent| {
                    let dir = agents.iter().find(|a| a.id == *agent);
                    let who = dir.map_or(agent.clone(), |a| a.config.name.clone());
                    (who, dir.map(|a| a.config.capabilities.granted.clone()).unwrap_or_default())
                });
                let problem = problem(&name, &desc, stations, owner.as_ref().map(|(who, held)| (who.as_str(), held.as_slice())));
                if problem.is_none() {
                    let needs = desc.capabilities();
                    let needs = if needs.is_empty() { String::new() } else { format!(", needs {}", needs.join(" ")) };
                    lines.push(format!("{:<9} {} ({}){needs}", "device", file.id, desc.name));
                }
                problem
            }
        };
        if let Some(problem) = problem {
            lines.push(format!("{:<9} {} does not load", "device", file.id));
            problems.push(format!("the device {} does not load: {problem}", file.id));
        }
    }
    (lines, problems)
}

/// A device id from a file name: lowercase letters, digits, `-` and `_`.
fn good_name(name: &str) -> bool {
    name.len() <= 40
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// "a", "a and b", "a, b and c".
fn list(items: &[String]) -> String {
    match items.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

impl Daemon {
    /// Loads the device scripts at the start, without telling anyone.
    pub(crate) fn load_devices(&mut self) {
        self.devices.scan();
        self.devices.poll(Instant::now(), true);
        self.devices.sent = self.device_infos();
        self.devices.values = self.device_values();
        for info in &self.devices.sent {
            self.devices.reported.insert(info.id.clone(), info.error.clone());
        }
    }

    /// Picks up script changes, reads the sources that are due and sends
    /// what changed. Runs on every tick of the daemon.
    pub(crate) fn tick_devices(&mut self, now: Instant) {
        if now >= self.devices.next_scan {
            self.devices.next_scan = now + SCAN_EVERY;
            self.devices.scan();
        }
        self.devices.poll(now, false);
        self.send_device_changes();
    }

    /// How long until the devices need the next tick.
    pub(crate) fn device_wait(&self, now: Instant) -> Duration {
        self.devices.next_due().saturating_duration_since(now)
    }

    /// Sends the device list when it changed, tells agents about their
    /// devices, and sends the param values that changed.
    fn send_device_changes(&mut self) {
        let infos = self.device_infos();
        if infos != self.devices.sent {
            for info in &infos {
                let before = self.devices.reported.insert(info.id.clone(), info.error.clone());
                if before.as_ref() != Some(&info.error) {
                    self.tell_owner(info);
                }
            }
            self.devices.reported.retain(|id, _| infos.iter().any(|i| i.id == *id));
            let gone: Vec<String> = self
                .caps
                .devices
                .iter()
                .filter(|r| !infos.iter().any(|i| i.id == r.device && i.error.is_none()))
                .map(|r| r.id.clone())
                .collect();
            for id in gone {
                self.close_device_request(&id, "the device is gone");
            }
            self.devices.sent = infos.clone();
            self.out.push(Outgoing::All(Event::Devices { devices: infos }));
        }
        let values = self.device_values();
        for ((device, name), value) in &values {
            if self.devices.values.get(&(device.clone(), name.clone())) != Some(value) {
                self.out.push(Outgoing::All(Event::DeviceParam { device: device.clone(), name: name.clone(), value: value.clone() }));
            }
        }
        self.devices.values = values;
    }

    /// A note in the chat of the agent that made the device. It says that the
    /// device loaded, or why not.
    fn tell_owner(&mut self, info: &DeviceInfo) {
        let Some(agent) = info.owner.clone() else {
            if let Some(e) = &info.error {
                warn!("the device {} of the space did not load ({e})", info.id);
            }
            return;
        };
        let file = info.script.rsplit('/').next().unwrap_or(&info.script).to_string();
        let text = match &info.error {
            Some(e) => format!("Your device {file} did not load. {e}"),
            None => format!("Your device {} ({file}) is in the space now.", info.name),
        };
        info!("{agent}: {text}");
        self.post_system(&agent, text);
    }

    /// Every device with what it holds, or why it did not load.
    pub(crate) fn device_infos(&self) -> Vec<DeviceInfo> {
        self.devices
            .loaded
            .values()
            .map(|loaded| {
                let file = &loaded.file;
                let script = file.path.strip_prefix(&self.devices.data_dir).unwrap_or(&file.path).display().to_string();
                let name = file.id.rsplit('/').next().unwrap_or(&file.id).to_string();
                let mut info = DeviceInfo {
                    id: file.id.clone(),
                    name: name.clone(),
                    owner: file.owner.clone(),
                    script,
                    revision: loaded.revision.clone(),
                    actions: Vec::new(),
                    needs: Vec::new(),
                    granted: Vec::new(),
                    error: None,
                };
                let desc = match &loaded.desc {
                    Ok(desc) => desc,
                    Err(e) => {
                        info.error = Some(e.clone());
                        return info;
                    }
                };
                info.name = desc.name.clone();
                info.needs = desc.capabilities();
                info.actions = desc
                    .actions
                    .iter()
                    .map(|a| DeviceActionInfo {
                        id: a.id.clone(),
                        label: a.label.clone(),
                        capability: a.capability().map(String::from),
                        opens: match &a.kind {
                            ActionKind::Opens(view) => Some(view.clone()),
                            ActionKind::Capability { .. } => None,
                        },
                    })
                    .collect();
                info.granted = match &file.owner {
                    None => self.devices.space_grants(&file.id),
                    Some(agent) => {
                        let held = self.granted(agent);
                        info.needs.iter().filter(|c| held.contains(c)).cloned().collect()
                    }
                };
                info.error = self.device_problem(&name, file, desc);
                info
            })
            .collect()
    }

    /// Why a built device may not load, if it may not.
    fn device_problem(&self, name: &str, file: &DeviceFile, desc: &DeviceDesc) -> Option<String> {
        let owner = file.owner.as_ref().map(|agent| {
            let who = self.agents.get(agent).map_or(agent.clone(), |a| a.dir.config.name.clone());
            (who, self.granted(agent))
        });
        problem(name, desc, &self.opts.stations, owner.as_ref().map(|(who, held)| (who.as_str(), held.as_slice())))
    }

    /// The reading of a source now. The system sources are polled, the
    /// sources of agents come from the engine.
    fn reading(&self, source: &Source) -> Option<Value> {
        match source {
            Source::Disk | Source::AirplaneMode | Source::Clock | Source::Cpu | Source::Memory | Source::Network => {
                self.devices.readings.get(source).cloned()
            }
            Source::AgentParam { agent, name } => self.engine.agent(agent)?.params.get(name).cloned(),
            Source::AgentRing { agent } => Some(Value::from(match self.engine.agent(agent)?.ring {
                RingState::Working => "working",
                RingState::NeedsYou => "needs_you",
                RingState::Resting => "resting",
            })),
        }
    }

    /// The value of every setting and every bound param of every loaded
    /// device. A param whose source has no reading, or whose value does not
    /// fit, is left out. Bindings made with `per()` hold only while their
    /// setting has their value.
    fn device_values(&self) -> BTreeMap<(String, String), Value> {
        let mut values = BTreeMap::new();
        for info in self.devices.sent.iter().filter(|i| i.error.is_none()) {
            let Some(Ok(desc)) = self.devices.loaded.get(&info.id).map(|l| &l.desc) else { continue };
            let settings = self.devices.setting_values(&info.id, desc);
            for (name, value) in &settings {
                values.insert((info.id.clone(), name.clone()), value.clone());
            }
            for binding in desc.active_bindings(&settings) {
                let Some(value) = self.reading(&binding.source).and_then(|r| binding.apply(&r)) else { continue };
                if desc.param(&binding.param).is_some_and(|def| def.accepts(&value)) {
                    values.insert((info.id.clone(), binding.param.clone()), value);
                }
            }
        }
        values
    }

    /// The device list, its param values and the open requests of devices, for the greeting.
    pub(crate) fn device_greeting(&self) -> Vec<Event> {
        let mut events = Vec::new();
        if !self.devices.sent.is_empty() {
            events.push(Event::Devices { devices: self.devices.sent.clone() });
        }
        events.extend(self.devices.values.iter().map(|((device, name), value)| Event::DeviceParam {
            device: device.clone(),
            name: name.clone(),
            value: value.clone(),
        }));
        events.extend(self.caps.devices.iter().map(device_request_event));
        events
    }

    /// `Command::DeviceAction` from the user, who clicked an action on a device card.
    pub(crate) fn device_action(&mut self, client: u64, device: &str, action: &str) {
        let answer = |daemon: &mut Daemon, ok: bool, message: String| {
            if !ok {
                info!("action {action} of the device {device} refused ({message})");
            }
            daemon.out.push(Outgoing::To(client, Event::DeviceActionResult { device: device.into(), action: action.into(), ok, message }));
        };
        let Some(info) = self.devices.sent.iter().find(|i| i.id == device).cloned() else {
            return answer(self, false, format!("There is no device {device:?}."));
        };
        if let Some(e) = &info.error {
            return answer(self, false, format!("{} did not load. {e}", info.name));
        }
        let Some(Ok(desc)) = self.devices.loaded.get(device).map(|l| &l.desc) else {
            return answer(self, false, format!("{} did not load.", info.name));
        };
        let Some(act) = desc.actions.iter().find(|a| a.id == action).cloned() else {
            return answer(self, false, format!("{} has no action {action:?}.", info.name));
        };
        let capability = match &act.kind {
            ActionKind::Opens(view) => return answer(self, true, format!("The app opens the {view}.")),
            ActionKind::Capability { capability, .. } => capability.clone(),
        };
        match &info.owner {
            Some(agent) => {
                // An agent's device uses only what its agent holds right now (D8).
                if self.granted(agent).contains(&capability) {
                    let (ok, message) = self.run_device_action(&info, &act.kind, "user");
                    answer(self, ok, message);
                } else {
                    let who = self.agents.get(agent).map_or(agent.clone(), |a| a.dir.config.name.clone());
                    answer(self, false, format!("{who} does not hold {capability} any more, so the device cannot use it."));
                }
            }
            None if self.devices.space_grants(device).contains(&capability) => {
                let (ok, message) = self.run_device_action(&info, &act.kind, "user");
                answer(self, ok, message);
            }
            None => self.request_device_capability(client, &info, &act.id, &act.label, &capability),
        }
    }

    /// `Command::DeviceSetting` from the user, who picked a value of a
    /// setting on a device card. It is kept in `devices/settings.toml` of the
    /// room, and the bindings that follow it switch at once.
    pub(crate) fn device_setting(&mut self, client: u64, device: &str, name: &str, value: Value) {
        let refuse = |daemon: &mut Daemon, message: String| {
            info!("setting {name} of the device {device} refused ({message})");
            daemon.out.push(Outgoing::To(
                client,
                Event::DeviceActionResult { device: device.into(), action: format!("setting.{name}"), ok: false, message },
            ));
        };
        let Some(Ok(desc)) = self.devices.loaded.get(device).map(|l| &l.desc) else {
            return refuse(self, format!("There is no device {device:?}."));
        };
        if desc.setting(name).is_none() {
            return refuse(self, format!("{} has no setting {name:?}.", desc.name));
        }
        if !desc.param(name).is_some_and(|def| def.accepts(&value)) {
            return refuse(self, format!("{value} is not a value of the setting {name} of {}.", desc.name));
        }
        let default = desc.param(name).map(|d| d.default_value());
        let entry = self.devices.settings.entry(device.to_string()).or_default();
        if Some(&value) == default.as_ref() {
            entry.remove(name);
        } else {
            entry.insert(name.to_string(), value.clone());
        }
        if entry.is_empty() {
            self.devices.settings.remove(device);
        }
        info!("the device {device} shows {name} = {value} now");
        if let Err(e) = self.devices.write_settings() {
            warn!("cannot save the setting of {device} ({e}), so it holds until the daemon stops");
        }
        self.send_device_changes();
    }

    /// Asks the user, through the broker, for the capability of an action
    /// of a space device. The action runs once the user allows it.
    fn request_device_capability(&mut self, client: u64, info: &DeviceInfo, action: &str, label: &str, capability: &str) {
        if let Some(open) = self.caps.devices.iter_mut().find(|r| r.device == info.id && r.capability == capability) {
            // Clicking again while the card is open does not queue the action twice.
            let waiter = (client, action.to_string());
            if !open.waiters.contains(&waiter) {
                open.waiters.push(waiter);
            }
            return;
        }
        let id = self.next_id("cap");
        info!("the device {} asks for {capability} ({id})", info.id);
        let request = DeviceRequest {
            id: id.clone(),
            device: info.id.clone(),
            capability: capability.into(),
            action: label.into(),
            waiters: vec![(client, action.to_string())],
        };
        self.caps.log_device(DeviceEntry {
            request: Some(&id),
            note: Some(label),
            ..DeviceEntry::new(&info.id, capability, "request", &info.id)
        });
        self.out.push(Outgoing::All(device_request_event(&request)));
        self.caps.devices.push(request);
    }

    /// `Command::DecideCapability` for a request of a device. Returns false
    /// when `id` is not one, so the broker looks among the agents' requests.
    pub(crate) fn decide_device_capability(&mut self, id: &str, approve: bool, remember: bool) -> bool {
        let Some(index) = self.caps.devices.iter().position(|r| r.id == id) else { return false };
        let request = self.caps.devices.remove(index);
        let decision = match (approve, remember) {
            (false, _) => "deny",
            (true, false) => "allow_once",
            (true, true) => "always_allow",
        };
        info!("the user decided {decision} for {} of the device {} ({id})", request.capability, request.device);
        let mut note = None;
        if approve && remember {
            let entry = self.devices.grants.entry(request.device.clone()).or_default();
            if !entry.granted.contains(&request.capability) {
                entry.granted.push(request.capability.clone());
            }
            if let Err(e) = self.devices.write_grants() {
                warn!("cannot save the grant of {} ({e}), so it holds once", request.device);
                note = Some(format!("{} could not be written ({e}), so it holds once", self.devices.grants_name()));
                self.devices.grants = self.devices.read_grants();
            }
        }
        self.caps.log_device(DeviceEntry {
            decision: Some(decision),
            request: Some(id),
            note: note.as_deref(),
            ..DeviceEntry::new(&request.device, &request.capability, "decision", "user")
        });
        let granted = self.devices.space_grants(&request.device);
        self.out.push(Outgoing::All(Event::DeviceCapabilitiesChanged { device: request.device.clone(), granted }));
        let info = self.devices.sent.iter().find(|i| i.id == request.device).cloned();
        let desc = self.devices.loaded.get(&request.device).and_then(|l| l.desc.as_ref().ok()).cloned();
        for (client, action) in &request.waiters {
            let kind = desc.as_ref().and_then(|d| d.actions.iter().find(|a| a.id == *action)).map(|a| a.kind.clone());
            let (ok, message) = match (&info, kind) {
                (Some(info), Some(kind)) if approve => self.run_device_action(info, &kind, "user"),
                _ if approve => (false, "The device is gone.".to_string()),
                _ => (false, format!("You did not allow {}, so nothing changed.", request.capability)),
            };
            self.out.push(Outgoing::To(
                *client,
                Event::DeviceActionResult { device: request.device.clone(), action: action.clone(), ok, message },
            ));
        }
        self.send_device_changes();
        true
    }

    /// Denies an open device request that cannot be decided any more.
    fn close_device_request(&mut self, id: &str, why: &str) {
        let Some(index) = self.caps.devices.iter().position(|r| r.id == id) else { return };
        let request = self.caps.devices.remove(index);
        self.caps.log_device(DeviceEntry {
            decision: Some("deny"),
            request: Some(id),
            note: Some(why),
            ..DeviceEntry::new(&request.device, &request.capability, "decision", "meerora")
        });
        for (client, action) in &request.waiters {
            self.out.push(Outgoing::To(
                *client,
                Event::DeviceActionResult {
                    device: request.device.clone(),
                    action: action.clone(),
                    ok: false,
                    message: format!("The request for {} was closed, because {why}.", request.capability),
                },
            ));
        }
        let granted = self.devices.space_grants(&request.device);
        self.out.push(Outgoing::All(Event::DeviceCapabilitiesChanged { device: request.device, granted }));
    }

    /// `Command::RevokeDeviceCapability` from the user. It takes a grant away
    /// from a device of the space.
    pub(crate) fn revoke_device_capability(&mut self, device: &str, capability: &str) {
        let held = self.devices.grants.get(device).is_some_and(|g| g.granted.iter().any(|c| c == capability));
        if held {
            if let Some(entry) = self.devices.grants.get_mut(device) {
                entry.granted.retain(|c| c != capability);
                if entry.granted.is_empty() {
                    self.devices.grants.remove(device);
                }
            }
            let note = self.devices.write_grants().err().map(|e| format!("{} could not be written ({e})", self.devices.grants_name()));
            info!("the user revoked {capability} from the device {device}");
            self.caps.log_device(DeviceEntry { note: note.as_deref(), ..DeviceEntry::new(device, capability, "revoke", "user") });
        } else {
            info!("the device {device} does not hold {capability}, nothing to revoke");
        }
        let granted = self.devices.space_grants(device);
        self.out.push(Outgoing::All(Event::DeviceCapabilitiesChanged { device: device.into(), granted }));
        self.send_device_changes();
    }

    /// Runs a capability action after the check, then reads the state again
    /// so the device follows at once. Returns the answer for the card.
    fn run_device_action(&mut self, info: &DeviceInfo, kind: &ActionKind, by: &str) -> (bool, String) {
        let ActionKind::Capability { capability, effect } = kind else { return (true, String::new()) };
        let result = match capability.as_str() {
            "network.airplane_mode.write" => {
                let on = match effect {
                    Effect::On => Ok(true),
                    Effect::Off => Ok(false),
                    Effect::Toggle => self.devices.system.airplane_mode().map(|now| !now),
                };
                on.and_then(|on| self.devices.system.set_airplane_mode(on).map(|()| on))
                    .map(|on| format!("Airplane mode is {}.", if on { "on" } else { "off" }))
            }
            other => Err(format!("meerora has no device backend for {other}")),
        };
        let note = match &result {
            Ok(message) => message.clone(),
            Err(e) => format!("failed: {e}"),
        };
        self.caps.log_device(DeviceEntry {
            note: Some(&note),
            decision: Some(effect.name()),
            ..DeviceEntry::new(&info.id, capability, "use", by)
        });
        self.devices.poll(Instant::now(), true);
        self.send_device_changes();
        match result {
            Ok(message) => (true, message),
            Err(e) => (false, format!("{} could not do that ({e}).", info.name)),
        }
    }
}

fn device_request_event(r: &DeviceRequest) -> Event {
    Event::DeviceCapabilityRequest {
        id: r.id.clone(),
        device: r.device.clone(),
        capability: r.capability.clone(),
        action: r.action.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::TempDir;

    #[test]
    fn radios_are_read_from_rfkill() {
        let dir = TempDir::new("rfkill");
        assert!(radios_off(&dir.0).unwrap_err().contains("no radios"));
        let radio = |name: &str, soft: &str, hard: &str| {
            let d = dir.0.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("soft"), soft).unwrap();
            std::fs::write(d.join("hard"), hard).unwrap();
        };
        radio("rfkill0", "0\n", "0\n");
        radio("rfkill1", "1\n", "0\n");
        assert_eq!(radios_off(&dir.0), Ok(false), "the wifi is still on");
        radio("rfkill0", "0\n", "1\n");
        assert_eq!(radios_off(&dir.0), Ok(true), "a hard block counts too");
    }

    #[test]
    fn the_real_write_commands() {
        assert_eq!(airplane_commands(true), [vec!["rfkill", "block", "all"], vec!["nmcli", "radio", "all", "off"]]);
        assert_eq!(airplane_commands(false), [vec!["rfkill", "unblock", "all"], vec!["nmcli", "radio", "all", "on"]]);
    }

    #[test]
    fn the_disk_of_this_machine_is_read() {
        let used = disk_used(Path::new("/")).unwrap();
        assert!((0.0..=1.0).contains(&used), "{used}");
        assert!(disk_used(Path::new("/does/not/exist")).is_err());
    }

    #[test]
    fn proc_files_are_read() {
        let stat = "cpu  100 5 50 800 20 3 2 0 0 0\ncpu0 50 2 25 400 10 1 1 0 0 0\nintr 1\n";
        assert_eq!(cpu_times(stat), Ok((160, 980)), "idle and iowait are idle");
        assert!(cpu_times("intr 1\n").is_err());
        let meminfo = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(memory_available(meminfo), Ok(0.25));
        assert!(memory_available("MemFree: 1 kB\n").is_err());
        let dev = "Inter-|   Receive                                                |  Transmit\n \
                   face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
                   lo: 5000 10 0 0 0 0 0 0 5000 10 0 0 0 0 0 0\n  \
                   wlan0: 1200 3 0 0 0 0 0 0 300 2 0 0 0 0 0 0\n  \
                   eth0: 100 1 0 0 0 0 0 0 50 1 0 0 0 0 0 0\n";
        assert_eq!(network_bytes(dev), Ok(1650), "loopback does not count");
        assert!(network_bytes("a\nb\n    lo: 1 0 0 0 0 0 0 0 1 0 0 0 0 0 0 0\n").is_err());
    }

    #[test]
    fn this_machine_has_cpu_memory_and_network_readings() {
        let real = RealSystem::new();
        let (busy, total) = real.cpu_times().unwrap();
        assert!(busy <= total && total > 0);
        let free = real.memory_available().unwrap();
        assert!((0.0..=1.0).contains(&free), "{free}");
        // A machine without a network interface has no reading, which the hub reports.
        if let Ok(sample) = real.network_bytes() {
            assert!(sample.at >= 0.0);
        }
    }

    #[test]
    fn device_names() {
        for good in ["airplane", "paper_plane", "lamp-2"] {
            assert!(good_name(good), "{good}");
        }
        for bad in ["", "Plane", "my plane", "-x", &"x".repeat(41)] {
            assert!(!good_name(bad), "{bad}");
        }
    }
}
