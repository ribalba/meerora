//! A private D-Bus session bus per desktop realm, so single-instance apps
//! such as Nautilus, Dolphin or Text Editor find their running instance on
//! the agent's desktop and open their window in the tile, instead of
//! handing the launch to an instance on the host.
//!
//! The bus has no service activation. A session bus with the stock
//! configuration would start a second dconf-service, gvfsd, keyring or
//! document portal for every desktop, which then write the same files as
//! the instances of the host session. Without activation the bus only
//! carries messages between the apps of its realm, and nothing on it runs
//! outside the sandbox of agent apps. What apps lose this way is listed in
//! the README (notifications, portals, saving GSettings, gvfs, the secret
//! service, accessibility).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long the daemon may take to create its socket.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// A running `dbus-daemon`. It ends when this is dropped.
pub(crate) struct Bus {
    child: Child,
    /// The listening socket.
    pub socket: PathBuf,
}

impl Bus {
    /// Starts a daemon that listens on `socket`, with its configuration in
    /// `config`, and waits until the socket exists.
    pub fn start(socket: &Path, config: &Path) -> io::Result<Bus> {
        let _ = std::fs::remove_file(socket);
        std::fs::write(config, config_text(socket))?;
        let mut cmd = Command::new("dbus-daemon");
        cmd.arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile", "--nosyslog"])
            .env_clear()
            .envs(std::env::vars_os().filter(|(k, _)| k == "PATH" || k == "LANG"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::launch::own_group(&mut cmd);
        let child = cmd.spawn()?;
        let mut bus = Bus { child, socket: socket.to_path_buf() };
        let deadline = Instant::now() + START_TIMEOUT;
        while !socket.exists() {
            if let Ok(Some(status)) = bus.child.try_wait() {
                return Err(io::Error::other(format!("dbus-daemon {status}")));
            }
            if Instant::now() > deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "dbus-daemon did not create its socket"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(bus)
    }

    /// `DBUS_SESSION_BUS_ADDRESS` for apps that see the socket at `socket`.
    pub fn address(socket: &Path) -> String {
        format!("unix:path={}", escape_address(&socket.to_string_lossy()))
    }

    /// The daemon still runs.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        crate::launch::stop(&mut self.child, Duration::from_millis(200));
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A session bus configuration that listens on `socket`, lets every
/// connection own and call any name, and has no service directories, so
/// nothing is ever activated.
fn config_text(socket: &Path) -> String {
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        escape_xml(&escape_address(&socket.to_string_lossy()))
    )
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// D-Bus address values escape every byte outside `[-0-9A-Za-z_/.\*]` as `%xx`.
fn escape_address(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_/.\\*".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_config_are_escaped() {
        assert_eq!(Bus::address(Path::new("/run/user/1000/a b,c")), "unix:path=/run/user/1000/a%20b%2cc");
        let text = config_text(Path::new("/tmp/x&y"));
        assert!(text.contains("<listen>unix:path=/tmp/x%26y</listen>"), "{text}");
        assert!(!text.contains("servicedir"), "no service activation");
    }

    #[test]
    fn a_bus_starts_and_stops() {
        if !crate::launch::installed("dbus-daemon") {
            eprintln!("skipped: dbus-daemon is not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("meerora-bus-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("bus");
        let mut bus = Bus::start(&socket, &dir.join("bus.conf")).expect("bus");
        assert!(socket.exists() && bus.alive());
        drop(bus);
        assert!(!socket.exists(), "the socket goes with the bus");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
