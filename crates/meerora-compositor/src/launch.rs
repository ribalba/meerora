//! Starting programs on an agent desktop. Every program talks to its
//! desktop's own Wayland socket, X display and session bus only, renders in
//! software and dies with the app.
//!
//! Programs the user starts run as normal programs with the user's
//! permissions. Programs an agent starts need the capability `apps.launch`
//! (D8), which the caller checks, and run in bubblewrap (D16) with the
//! sandbox code of meerora-runtime. The sandbox shows:
//! - the system read-only, as for agent runs
//! - the agent directory read-write, as `$HOME` and the working directory
//! - the agent realm's own Wayland socket, X socket and session bus, never
//!   the ones of the user's apps
//! - a private `/tmp`, `/dev` and `/proc`, and no network unless asked for
//! - an allowlist of environment variables, and nothing of the user's home

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use meerora_runtime::sandbox;

/// GTK and Mesa render in software, because the compositor has no dmabuf
/// and no GPU path.
const SOFTWARE: [(&str, &str); 2] = [("GSK_RENDERER", "cairo"), ("LIBGL_ALWAYS_SOFTWARE", "1")];

/// Toolkits prefer Wayland and fall back to the desktop's X display.
const WAYLAND_FIRST: [(&str, &str); 5] = [
    ("XDG_SESSION_TYPE", "wayland"),
    ("GDK_BACKEND", "wayland,x11"),
    ("QT_QPA_PLATFORM", "wayland;xcb"),
    ("MOZ_ENABLE_WAYLAND", "1"),
    ("ELECTRON_OZONE_PLATFORM_HINT", "auto"),
];

/// Wayland only, for a desktop without an X display.
const WAYLAND_ONLY: [(&str, &str); 5] = [
    ("XDG_SESSION_TYPE", "wayland"),
    ("GDK_BACKEND", "wayland"),
    ("QT_QPA_PLATFORM", "wayland"),
    ("MOZ_ENABLE_WAYLAND", "1"),
    ("ELECTRON_OZONE_PLATFORM_HINT", "wayland"),
];

/// Toolkits use X11, for [`X11::Force`].
const X11_ONLY: [(&str, &str); 6] = [
    ("XDG_SESSION_TYPE", "x11"),
    ("GDK_BACKEND", "x11"),
    ("QT_QPA_PLATFORM", "xcb"),
    ("MOZ_ENABLE_WAYLAND", "0"),
    ("ELECTRON_OZONE_PLATFORM_HINT", "x11"),
    ("SDL_VIDEODRIVER", "x11"),
];

/// Variables that would send a program to the host session instead.
const HOST_SESSION: [&str; 6] =
    ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "XAUTHORITY", "XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

/// Read-only paths every sandboxed app gets besides the system, when they
/// exist. Without the font cache every launch would build it again.
const SANDBOX_READ: [&str; 1] = ["/var/cache/fontconfig"];

/// Whether a program gets the desktop's X display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum X11 {
    /// `DISPLAY` is set and toolkits prefer Wayland. The X server starts
    /// with the first program of the realm.
    #[default]
    Offer,
    /// Toolkits use X11 and `WAYLAND_DISPLAY` is left out, for apps that
    /// misbehave on Wayland.
    Force,
    /// No X display, and no X server is started for it.
    Off,
}

/// Settings for [`crate::Compositor::launch_with`].
#[derive(Clone, Debug, Default)]
pub struct LaunchOptions {
    pub x11: X11,
    /// More environment variables, set last.
    pub env: Vec<(String, String)>,
}

/// Settings for [`crate::Compositor::launch_sandboxed`].
#[derive(Clone, Debug, Default)]
pub struct SandboxOptions {
    pub x11: X11,
    /// The app may use the network. Without it the sandbox gets its own
    /// network namespace, which also hides the host's abstract Unix
    /// sockets, such as the one of the host's X server. Agents that hold
    /// `web` may get it (D29).
    pub network: bool,
    /// More absolute paths the app may read.
    pub read: Vec<PathBuf>,
    /// More environment variables, set last.
    pub env: Vec<(String, String)>,
}

/// What a program connects to, as the program sees it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Session {
    /// `WAYLAND_DISPLAY`.
    pub wayland: String,
    /// The X display number.
    pub display: Option<u32>,
    /// `DBUS_SESSION_BUS_ADDRESS`.
    pub bus: Option<String>,
    pub x11: X11,
}

impl Session {
    /// The variables to set after [`HOST_SESSION`] is removed.
    pub fn vars(&self) -> Vec<(String, String)> {
        let display = self.display.filter(|_| self.x11 != X11::Off);
        let force = self.x11 == X11::Force && display.is_some();
        let mut vars = Vec::new();
        if !force {
            vars.push(("WAYLAND_DISPLAY".to_string(), self.wayland.clone()));
        }
        if let Some(n) = display {
            vars.push(("DISPLAY".to_string(), format!(":{n}")));
        }
        if let Some(bus) = &self.bus {
            vars.push(("DBUS_SESSION_BUS_ADDRESS".to_string(), bus.clone()));
        }
        let toolkits: &[(&str, &str)] = if force {
            &X11_ONLY
        } else if display.is_some() {
            &WAYLAND_FIRST
        } else {
            &WAYLAND_ONLY
        };
        vars.extend(toolkits.iter().chain(&SOFTWARE).map(|(k, v)| (k.to_string(), v.to_string())));
        vars
    }
}

/// The command that starts `program` with `args` for `session`, in `cwd`
/// when given. Output goes nowhere. See [`own_group`].
pub(crate) fn command(
    session: &Session,
    program: impl AsRef<OsStr>,
    args: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for name in HOST_SESSION {
        cmd.env_remove(name);
    }
    if session.bus.is_some() {
        cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
    }
    cmd.envs(session.vars()).envs(env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    own_group(&mut cmd);
    cmd
}

/// What a sandboxed program sees of its desktop.
pub(crate) struct SandboxPlan<'a> {
    pub agent_dir: &'a Path,
    /// The agent realm's runtime directory on the host, with its Wayland
    /// socket and bus. The program sees it at the same path as `XDG_RUNTIME_DIR`.
    pub run_dir: &'a Path,
    /// The agent realm's X socket.
    pub x_socket: Option<PathBuf>,
    pub options: &'a SandboxOptions,
}

/// The command that starts `program` in bubblewrap. `session` must name
/// the Wayland socket relative to `run_dir`.
pub(crate) fn sandboxed(session: &Session, plan: &SandboxPlan, program: &str, args: &[String]) -> io::Result<Command> {
    let bwrap = sandbox::find().map_err(io::Error::other)?;
    let program = resolve(program)?;
    // A program outside the system directories is shown read-only, by its
    // path and by its real path. Inside them it is visible already, and a
    // mount on a symlink there, such as /usr/bin/sh, would fail.
    let mut read: Vec<PathBuf> = Vec::new();
    for path in [program.clone(), program.canonicalize().unwrap_or_else(|_| program.clone())] {
        if !sandbox::SYSTEM.iter().any(|dir| path.starts_with(dir)) && !read.contains(&path) {
            read.push(path);
        }
    }
    read.extend(SANDBOX_READ.iter().map(PathBuf::from).filter(|p| p.exists()));
    read.extend(plan.options.read.iter().cloned());
    let mounts = sandbox::Mounts {
        dir: plan.agent_dir.to_path_buf(),
        read,
        write: vec![plan.run_dir.to_path_buf()],
        sockets: plan.x_socket.iter().cloned().collect(),
        hide: sandbox::user_home(),
        ..sandbox::Mounts::default()
    };
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let mut bwrap_args: Vec<OsString> = Vec::new();
    if !plan.options.network {
        bwrap_args.push("--unshare-net".into());
    }
    bwrap_args.extend(sandbox::args(&mounts, &program, &args));

    let mut cmd = Command::new(bwrap);
    cmd.args(bwrap_args).env_clear().envs(sandbox::kept_env(false)).envs(session.vars());
    let tmp = |rest: &str| format!("/tmp/{rest}");
    for (k, v) in [
        ("HOME", plan.agent_dir.display().to_string()),
        ("XDG_RUNTIME_DIR", plan.run_dir.display().to_string()),
        // App settings and caches stay inside the sandbox and go with it,
        // so they neither fill the agent's git history nor leak into it.
        ("XDG_CONFIG_HOME", tmp("xdg/config")),
        ("XDG_DATA_HOME", tmp("xdg/data")),
        ("XDG_STATE_HOME", tmp("xdg/state")),
        ("XDG_CACHE_HOME", tmp("xdg/cache")),
        ("QT_X11_NO_MITSHM", "1".to_string()),
    ] {
        cmd.env(k, v);
    }
    cmd.envs(plan.options.env.iter().map(|(k, v)| (k, v)));
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    own_group(&mut cmd);
    Ok(cmd)
}

/// The absolute path of `program`, searched in PATH unless it has a slash.
fn resolve(program: &str) -> io::Result<PathBuf> {
    let path = if program.contains('/') {
        std::path::absolute(program)?
    } else {
        find(program).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{program} is not in PATH")))?
    };
    if path.is_file() { Ok(path) } else { Err(io::Error::new(io::ErrorKind::NotFound, format!("{} does not exist", path.display()))) }
}

fn find(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|p| p.is_file())
}

/// The program is in PATH.
pub fn installed(program: &str) -> bool {
    find(program).is_some()
}

/// Makes the child lead its own process group, so [`stop`] also ends what
/// it started, such as the bus of `dbus-run-session`. It gets SIGTERM when
/// the thread that started it ends, which is the app's main thread.
pub(crate) fn own_group(cmd: &mut Command) {
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The app may have died between fork and prctl.
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

/// Starts a command made here.
pub(crate) fn spawn(mut cmd: Command) -> io::Result<Child> {
    cmd.spawn()
}

/// Ends a launched program and its process group. They get SIGTERM, and
/// what still runs after `grace` gets SIGKILL. A sandboxed program ends
/// with its bubblewrap, which takes the whole sandbox with it.
pub fn stop(child: &mut Child, grace: std::time::Duration) {
    let group = -(child.id() as libc::pid_t);
    // SAFETY: plain signals to the group the child leads.
    unsafe { libc::kill(group, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + grace;
    while child.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe { libc::kill(group, libc::SIGKILL) };
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(vars: &'a [(String, String)], key: &str) -> Option<&'a str> {
        vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn wayland_comes_first_and_x11_can_be_forced() {
        let session = Session { wayland: "meerora-wl-ember".into(), display: Some(7), bus: Some("unix:path=/b".into()), x11: X11::Offer };
        let vars = session.vars();
        assert_eq!(get(&vars, "WAYLAND_DISPLAY"), Some("meerora-wl-ember"));
        assert_eq!(get(&vars, "DISPLAY"), Some(":7"));
        assert_eq!(get(&vars, "GDK_BACKEND"), Some("wayland,x11"));
        assert_eq!(get(&vars, "QT_QPA_PLATFORM"), Some("wayland;xcb"));
        assert_eq!(get(&vars, "DBUS_SESSION_BUS_ADDRESS"), Some("unix:path=/b"));

        let forced = Session { x11: X11::Force, ..session.clone() }.vars();
        assert_eq!(get(&forced, "WAYLAND_DISPLAY"), None, "forced X11 leaves Wayland out");
        assert_eq!(get(&forced, "GDK_BACKEND"), Some("x11"));
        assert_eq!(get(&forced, "QT_QPA_PLATFORM"), Some("xcb"));

        let off = Session { x11: X11::Off, ..session.clone() }.vars();
        assert_eq!(get(&off, "DISPLAY"), None);
        assert_eq!(get(&off, "GDK_BACKEND"), Some("wayland"));

        let no_x = Session { display: None, x11: X11::Force, ..session }.vars();
        assert_eq!(get(&no_x, "WAYLAND_DISPLAY"), Some("meerora-wl-ember"), "without an X server it stays on Wayland");
    }

    #[test]
    fn the_sandbox_shows_the_agent_realm_only() {
        let Ok(_) = sandbox::find() else {
            eprintln!("skipped: bwrap is not installed");
            return;
        };
        let session = Session { wayland: "wayland-0".into(), display: Some(9), bus: Some("unix:path=/run/r/bus".into()), x11: X11::Offer };
        let options = SandboxOptions::default();
        let plan = SandboxPlan {
            agent_dir: Path::new("/data/agents/wren"),
            run_dir: Path::new("/run/user/1000/meerora-wl-wren.d/agent"),
            x_socket: Some("/tmp/.X11-unix/X9".into()),
            options: &options,
        };
        let cmd = sandboxed(&session, &plan, "sh", &["-c".into(), "true".into()]).unwrap();
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let has = |w: &[&str]| args.windows(w.len()).any(|x| x == w);
        assert_eq!(args[0], "--unshare-net", "no network unless asked");
        assert!(has(&["--bind", "/data/agents/wren", "/data/agents/wren"]));
        assert!(has(&["--bind", "/run/user/1000/meerora-wl-wren.d/agent", "/run/user/1000/meerora-wl-wren.d/agent"]));
        assert!(has(&["--bind-try", "/tmp/.X11-unix/X9", "/tmp/.X11-unix/X9"]));
        assert!(has(&["--chdir", "/data/agents/wren"]));
        let env: Vec<(String, String)> =
            cmd.get_envs().filter_map(|(k, v)| Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned()))).collect();
        assert_eq!(get(&env, "HOME"), Some("/data/agents/wren"));
        assert_eq!(get(&env, "XDG_RUNTIME_DIR"), Some("/run/user/1000/meerora-wl-wren.d/agent"));
        assert_eq!(get(&env, "WAYLAND_DISPLAY"), Some("wayland-0"));
        assert_eq!(get(&env, "DISPLAY"), Some(":9"));
        assert!(get(&env, "SSH_AUTH_SOCK").is_none() && get(&env, "XAUTHORITY").is_none());

        let with_net = SandboxOptions { network: true, ..SandboxOptions::default() };
        let plan = SandboxPlan { options: &with_net, ..plan };
        let cmd = sandboxed(&session, &plan, "sh", &[]).unwrap();
        assert!(!cmd.get_args().any(|a| a == "--unshare-net"));
    }
}
