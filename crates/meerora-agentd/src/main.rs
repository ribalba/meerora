//! The meerora agent daemon (D16, D22, D23). It owns the state of all agents
//! and serves the app over the Unix socket of [`meerora_protocol::socket_path`].
//! Real agents run on LLM runtimes with the meerora tools, wake on their
//! schedules and keep their directories in git (D21). Mock agents play a
//! scenario. The daemon also runs the figure builder of screen 3.

mod app_tools;
mod ask;
mod builder;
mod capabilities;
mod check;
mod config_tools;
mod daemon;
mod devices;
mod engine;
mod figures;
mod git;
mod graveyard;
mod helper;
mod index;
mod init;
mod interview;
mod news;
mod runs;
mod scenario;
mod schedule;
mod server;
mod signal;
mod social;
mod space;
mod store;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;

use anyhow::{Context, bail};
use meerora_protocol::{AgentDir, Command, Settings, decode_event, load_agents, resolve_data_dir, socket_path};
use tracing::{error, info, warn};

use crate::daemon::{Daemon, Options, real_runtimes};
use crate::scenario::{Report, Scenario, is_mock, kind_name, summary, validate};
use crate::server::SocketGuard;

const HELP: &str = "\
meerora-agentd, the agent daemon of meerora

Usage:
  meerora-agentd [--data-dir DIR] [--scenario FILE] [--socket PATH] [--speed F]
  meerora-agentd say [--socket PATH] <agent> <text>
  meerora-agentd cancel [--socket PATH] <agent>
  meerora-agentd ack [--socket PATH] <agent>
  meerora-agentd send [--socket PATH] '<event json>'
  meerora-agentd reload [--socket PATH]
  meerora-agentd search [--socket PATH] <query>
  meerora-agentd import [--socket PATH] <manifest id>
  meerora-agentd social [--socket PATH] <action> [argument]
  meerora-agentd retire [--socket PATH] <agent>
  meerora-agentd restore [--socket PATH] <id in the graveyard>
  meerora-agentd graveyard [--socket PATH]
  meerora-agentd check [--data-dir DIR] [--scenario FILE]
  meerora-agentd init [--data-dir DIR]
  meerora-agentd --help

Commands:
  (none)   Run the daemon. Real agents run on their runtimes and schedules, mock agents
           play the scenario, and clients connect over the socket.
  say      Send a message to an agent and print its events until the run finishes.
  cancel   Stop the agent's current run.
  ack      Tell the daemon that you attended to an agent that needs you.
  send     Inject one event, for example '{\"type\":\"go_to\",\"agent\":\"wren\",\"station\":\"door\"}'.
  reload   Make the daemon rescan the agent directories.
  search   Search the meerora index of [index] url in meerora.toml (D25). An empty query lists everything.
  import   Import an external agent from the index by manifest id, for example meridian.example/mira.
  social   The personal agent and the feed (D26 to D28): account [HANDLE], publish PROFILE.json,
           relations, follow ACTOR, unfollow ACTOR, befriend HANDLE, accept HANDLE, decline HANDLE,
           feed, drafts, approve ID, discard ID and news.
  retire   Move an agent to the graveyard. It stops working and leaves the space, and its
           directory moves to graveyard/ with its files, memory and history.
  restore  Bring a retired agent back from the graveyard to its home station.
  graveyard
           List the retired agents with their ids in the graveyard.
  check    Validate the scenario, the agents, their schedules and runtimes, and print a summary.
  init     Create the data dir with the rooms of the shipped assets and two real agents on
           claude-cli: Claude and Rivet, the helper. The demo agents stay in the shipped assets.

Options:
  --data-dir DIR   Folder with spaces/, agents/ and scenarios/. Defaults to $MEERORA_DATA_DIR,
                   then ~/.local/share/meerora if it exists, then the shipped assets: $MEERORA_ASSETS,
                   then <prefix>/share/meerora of this binary, then the assets/ folder of the repository.
                   For init it defaults to $MEERORA_DATA_DIR, then ~/.local/share/meerora.
  --scenario FILE  Defaults to <data dir>/scenarios/demo.toml.
  --socket PATH    Defaults to $MEERORA_SOCKET, then $XDG_RUNTIME_DIR/meerora/agentd.sock,
                   then /tmp/meerora-$USER/agentd.sock.
  --speed F        Play the mock scenario F times as fast. Defaults to 1. Real schedules keep real time.
  -h, --help       Show this help.

The log level comes from RUST_LOG and is info by default.
";

/// Where the agents and the scenario come from.
#[derive(Clone, Debug, Default, PartialEq)]
struct Paths {
    data_dir: Option<PathBuf>,
    scenario: Option<PathBuf>,
}

/// A parsed command line.
#[derive(Clone, Debug, PartialEq)]
enum Cli {
    Help,
    Run { paths: Paths, socket: Option<PathBuf>, speed: f64 },
    Client { socket: Option<PathBuf>, command: Command },
    Say { socket: Option<PathBuf>, agent: String, text: String },
    Index { socket: Option<PathBuf>, command: Command },
    Social { socket: Option<PathBuf>, args: Vec<String> },
    Graveyard { socket: Option<PathBuf>, command: Option<Command> },
    Check { paths: Paths },
    Init { data_dir: Option<PathBuf> },
}

fn parse_args(args: Vec<String>) -> anyhow::Result<Cli> {
    let mut args = args.into_iter().peekable();
    let mode = match args.peek().map(String::as_str) {
        Some(
            m @ ("send" | "ack" | "say" | "cancel" | "reload" | "search" | "import" | "social" | "check" | "init" | "retire" | "restore"
            | "graveyard"),
        ) => m.to_string(),
        _ => "run".to_string(),
    };
    if mode != "run" {
        args.next();
    }
    let allowed: &[&str] = match mode.as_str() {
        "run" => &["data-dir", "scenario", "socket", "speed"],
        "check" => &["data-dir", "scenario"],
        "init" => &["data-dir"],
        _ => &["socket"],
    };

    let (mut paths, mut socket, mut speed) = (Paths::default(), None, 1.0);
    let mut positional = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            return Ok(Cli::Help);
        }
        if arg == "--" {
            positional.extend(args.by_ref());
            break;
        }
        let Some(flag) = arg.strip_prefix("--") else {
            if arg.starts_with('-') && arg.len() > 1 {
                bail!("unknown option {arg}");
            }
            positional.push(arg);
            continue;
        };
        let (name, value) = match flag.split_once('=') {
            Some((name, value)) => (name.to_string(), value.to_string()),
            None => (flag.to_string(), args.next().with_context(|| format!("--{flag} needs a value"))?),
        };
        if !allowed.contains(&name.as_str()) {
            bail!("unknown option --{name} for {}", if mode == "run" { "the daemon" } else { mode.as_str() });
        }
        match name.as_str() {
            "data-dir" => paths.data_dir = Some(value.into()),
            "scenario" => paths.scenario = Some(value.into()),
            "socket" => socket = Some(value.into()),
            _ => {
                speed = value
                    .parse()
                    .ok()
                    .filter(|s: &f64| s.is_finite() && *s > 0.0)
                    .with_context(|| format!("--speed needs a number above 0, not {value:?}"))?
            }
        }
    }

    // social checks its own arguments.
    if mode == "social" {
        return Ok(Cli::Social { socket, args: positional });
    }
    let wanted = match mode.as_str() {
        "send" | "ack" | "cancel" | "search" | "import" | "retire" | "restore" => 1,
        "say" => 2,
        _ => 0,
    };
    if positional.len() != wanted {
        let what = match mode.as_str() {
            "send" => "one event as JSON",
            "say" => "an agent id and a text",
            "ack" | "cancel" | "retire" => "the id of one agent",
            "restore" => "one id in the graveyard",
            "search" => "one query",
            "import" => "one manifest id",
            _ => "no arguments",
        };
        bail!("{} takes {what}, not {positional:?}", if mode == "run" { "the daemon" } else { mode.as_str() });
    }
    let mut positional = positional.into_iter();
    let mut next = || positional.next().unwrap_or_default();
    let cli = match mode.as_str() {
        "send" => {
            let json = next();
            let event = decode_event(&json).with_context(|| format!("{json} is not a valid event"))?;
            Cli::Client { socket, command: Command::Inject { event } }
        }
        "ack" => Cli::Client { socket, command: Command::Ack { agent: next() } },
        "cancel" => Cli::Client { socket, command: Command::Cancel { agent: next() } },
        "reload" => Cli::Client { socket, command: Command::ReloadAgents },
        "say" => Cli::Say { socket, agent: next(), text: next() },
        "search" => Cli::Index { socket, command: Command::IndexSearch { request: String::new(), query: next() } },
        "import" => Cli::Index { socket, command: Command::ImportAgent { manifest: next() } },
        "retire" => Cli::Graveyard { socket, command: Some(Command::Retire { agent: next() }) },
        "restore" => Cli::Graveyard { socket, command: Some(Command::Restore { agent: next() }) },
        "graveyard" => Cli::Graveyard { socket, command: None },
        "check" => Cli::Check { paths },
        "init" => Cli::Init { data_dir: paths.data_dir },
        _ => Cli::Run { paths, socket, speed },
    };
    Ok(cli)
}

fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(ansi).init();

    let cli = match parse_args(std::env::args().skip(1).collect()) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("error: {e:#}\n\nRun meerora-agentd --help for the usage.");
            return ExitCode::from(2);
        }
    };
    let result = match cli {
        Cli::Help => {
            print!("{HELP}");
            Ok(ExitCode::SUCCESS)
        }
        Cli::Run { paths, socket, speed } => run_daemon(&paths, socket.unwrap_or_else(socket_path), speed),
        Cli::Client { socket, command } => server::send_command(&socket.unwrap_or_else(socket_path), &command).map(|()| ExitCode::SUCCESS),
        Cli::Say { socket, agent, text } => {
            server::say(&socket.unwrap_or_else(socket_path), &agent, &text, &mut std::io::stdout()).map(|()| ExitCode::SUCCESS)
        }
        Cli::Social { socket, args } => {
            social::run_cli(&socket.unwrap_or_else(socket_path), &args, &mut std::io::stdout()).map(|()| ExitCode::SUCCESS)
        }
        Cli::Index { socket, command } => {
            index::run_cli(&socket.unwrap_or_else(socket_path), command, &mut std::io::stdout()).map(|()| ExitCode::SUCCESS)
        }
        Cli::Graveyard { socket, command } => {
            graveyard::run_cli(&socket.unwrap_or_else(socket_path), command, &mut std::io::stdout()).map(|()| ExitCode::SUCCESS)
        }
        Cli::Check { paths } => check(&paths),
        Cli::Init { data_dir } => run_init(data_dir),
    };
    result.unwrap_or_else(|e| {
        eprintln!("error: {e:#}");
        ExitCode::FAILURE
    })
}

/// Everything the daemon and `check` read at the start, and what is wrong with it.
struct Loaded {
    data_dir: PathBuf,
    scenario_path: Option<PathBuf>,
    scenario: Scenario,
    agents: Vec<AgentDir>,
    settings: Settings,
    stations: Vec<String>,
    /// Where the stations came from.
    stations_from: &'static str,
    report: Report,
}

fn load(paths: &Paths) -> anyhow::Result<Loaded> {
    let data_dir = paths.data_dir.clone().unwrap_or_else(default_data_dir);
    let default_scenario = data_dir.join("scenarios").join("demo.toml");
    let scenario_path = match &paths.scenario {
        Some(path) => Some(path.clone()),
        None => default_scenario.is_file().then_some(default_scenario),
    };
    let mut scenario = match &scenario_path {
        Some(path) => Scenario::load(path)?,
        None => Scenario::default(),
    };
    let (mut agents, mut agent_errors) = (Vec::new(), Vec::new());
    for result in load_agents(&data_dir) {
        match result {
            Ok(agent) => agents.push(agent),
            Err(e) => agent_errors.push(e),
        }
    }
    if agents.is_empty() && agent_errors.is_empty() {
        agent_errors.push(format!("no agent directories in {}", data_dir.join("agents").display()));
    }
    let (stations, station_warning) = space::stations_or_fallback(&data_dir);
    // The steps of retired agents wait in the graveyard with them.
    let retired = graveyard::skip_retired(&mut scenario, &data_dir, &agents);
    let mut report = validate(&scenario, &agents, &agent_errors, &stations);
    report.warnings.extend(retired);
    let stations_from = if station_warning.is_some() { "built in" } else { "from space.js" };
    report.warnings.extend(station_warning);
    // Data dirs from init have no mock agents, so they need no scenario.
    if scenario_path.is_none() && agents.iter().any(|a| is_mock(&a.config)) {
        report.warnings.push(format!("there is no {}, so mock agents stand still", data_dir.join("scenarios/demo.toml").display()));
    }
    let settings = Settings::load(&data_dir).unwrap_or_else(|e| {
        report.errors.push(e);
        Settings::default()
    });
    report.warnings.extend(check::runtime_warnings(&agents, &settings));
    if agents.iter().any(|a| !is_mock(&a.config))
        && let Err(problem) = check::sandbox(&settings)
    {
        report.warnings.push(problem);
    }
    if agents.iter().any(|a| !is_mock(&a.config)) && bridge_path().is_none() {
        report.warnings.push("meerora-mcp is not next to meerora-agentd, so real agents run without the meerora tools. Build it with cargo build -p meerora-mcp.".into());
    }
    Ok(Loaded { data_dir, scenario_path, scenario, agents, settings, stations, stations_from, report })
}

/// The shipped assets: `$MEERORA_ASSETS`, the install prefix of this binary,
/// then the `assets/` folder of the repository (see `meerora_protocol::assets_dir`).
fn repo_assets() -> PathBuf {
    meerora_protocol::assets_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap_or(Path::new(".")).join("assets"))
}

/// The shipped assets, unless the user has a data dir.
fn default_data_dir() -> PathBuf {
    resolve_data_dir(&repo_assets())
}

/// The MCP bridge next to this binary (D22).
fn bridge_path() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?.with_file_name("meerora-mcp");
    path.is_file().then_some(path)
}

fn run_daemon(paths: &Paths, socket: PathBuf, speed: f64) -> anyhow::Result<ExitCode> {
    // The helper exists before the agents load (helper.rs).
    let data_dir = paths.data_dir.clone().unwrap_or_else(default_data_dir);
    let (stations, _) = space::stations_or_fallback(&data_dir);
    match helper::ensure(&data_dir, &stations) {
        helper::Ensured::Present(id) => info!("{id} is meerora's helper"),
        helper::Ensured::Created(dir) => info!("created meerora's helper in {}", dir.display()),
        // The demo data in the shipped assets keeps its mock Rivet.
        helper::Ensured::Missing(why) if data_dir == repo_assets() => info!("{why}"),
        helper::Ensured::Missing(why) => warn!("{why}"),
    }
    let loaded = load(paths)?;
    for warning in &loaded.report.warnings {
        warn!("{warning}");
    }
    if !loaded.report.is_ok() {
        for e in &loaded.report.errors {
            error!("{e}");
        }
        bail!("{} has {}. Run meerora-agentd check for details.", loaded.data_dir.display(), count_errors(&loaded.report));
    }

    let listener = server::bind(&socket)?;
    let _guard = SocketGuard(socket.clone());
    signal::remove_on_signal(socket.clone()).context("cannot watch for signals")?;
    let real = loaded.agents.iter().filter(|a| !is_mock(&a.config)).count();
    info!(
        "serving {} agents ({real} real) from {} on {}, mock scenario {} at speed {speed}",
        loaded.agents.len(),
        loaded.data_dir.display(),
        socket.display(),
        loaded.scenario_path.as_ref().map_or("none".into(), |p| p.display().to_string()),
    );
    let (tx, rx) = mpsc::channel();
    let options = Options {
        data_dir: loaded.data_dir,
        socket,
        settings: loaded.settings,
        stations: loaded.stations,
        factory: real_runtimes(),
        bridge: bridge_path(),
        git: true,
        speed,
    };
    let daemon = Daemon::new(options, loaded.scenario, loaded.agents, tx.clone());
    server::serve(listener, daemon, tx, rx)?;
    Ok(ExitCode::SUCCESS)
}

/// What `check` reports: the lines for stdout, the warnings and errors for
/// stderr, the last line and whether the data dir is ok.
struct CheckReport {
    lines: Vec<String>,
    problems: Vec<String>,
    verdict: String,
    ok: bool,
}

fn check_report(paths: &Paths) -> anyhow::Result<CheckReport> {
    let loaded = load(paths)?;
    let mut lines = vec![
        format!("data dir  {}", loaded.data_dir.display()),
        format!("scenario  {}", loaded.scenario_path.as_ref().map_or("none".into(), |p| p.display().to_string())),
        format!("stations  {} {}: {}", loaded.stations.len(), loaded.stations_from, loaded.stations.join(", ")),
        format!("sandbox   {}", check::sandbox(&loaded.settings).unwrap_or_else(|_| "does not work, see the warning below".into())),
    ];
    lines.extend(summary(&loaded.scenario, &loaded.agents));
    for agent in loaded.agents.iter().filter(|a| !is_mock(&a.config)) {
        lines.push(format!("{:<9} {}", agent.id, describe_real(agent)));
    }
    // Devices (D7, D8). One that does not load is a warning, the daemon runs without it.
    let (devices, device_problems) = devices::check(&loaded.data_dir, &loaded.stations, &loaded.agents);
    lines.extend(devices);
    let mut problems: Vec<String> = device_problems.iter().map(|p| format!("warning: {p}")).collect();
    problems.extend(loaded.report.warnings.iter().map(|w| format!("warning: {w}")));
    problems.extend(loaded.report.errors.iter().map(|e| format!("error: {e}")));
    let ok = loaded.report.is_ok();
    let verdict = if ok { "ok".to_string() } else { count_errors(&loaded.report) };
    Ok(CheckReport { lines, problems, verdict, ok })
}

fn check(paths: &Paths) -> anyhow::Result<ExitCode> {
    let report = check_report(paths)?;
    for line in &report.lines {
        println!("{line}");
    }
    for problem in &report.problems {
        eprintln!("{problem}");
    }
    println!("{}", report.verdict);
    Ok(if report.ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// What `meerora-agentd check` prints for `data_dir`, as one text, for the
/// helper's `config_read check` (config_tools.rs).
pub(crate) fn check_text(data_dir: &Path) -> String {
    let paths = Paths { data_dir: Some(data_dir.to_path_buf()), scenario: None };
    match check_report(&paths) {
        Ok(report) => {
            let mut lines = report.lines;
            lines.extend(report.problems);
            lines.push(report.verdict);
            lines.join("\n")
        }
        Err(e) => format!("The check could not run: {e:#}"),
    }
}

/// One line about a real agent for `check`.
fn describe_real(agent: &AgentDir) -> String {
    let config = &agent.config;
    let mut parts = Vec::new();
    if let Some(runtime) = &config.runtime {
        let model = runtime.model.as_deref().map(|m| format!(" with {m}")).unwrap_or_default();
        parts.push(format!("{}{model}", kind_name(runtime.kind)));
    }
    if let Some(role) = config.role {
        parts.push(format!("role {}", serde_json::to_string(&role).unwrap_or_default().trim_matches('"')));
    }
    parts.push(format!("on_done {}", if config.on_done == meerora_protocol::OnDone::Wait { "wait" } else { "rest" }));
    for entry in &config.schedule {
        let when = entry.every.as_deref().map(|e| format!("every {e}")).or_else(|| entry.at.as_deref().map(|a| format!("at {a}")));
        let quiet = entry.quiet_hours.as_deref().map(|q| format!(", quiet {q}")).unwrap_or_default();
        parts.push(format!("wakes {}{quiet}", when.unwrap_or_default()));
    }
    if let Some(tokens) = config.budget.as_ref().and_then(|b| b.tokens_per_day) {
        parts.push(format!("{tokens} tokens per day"));
    }
    if !config.capabilities.granted.is_empty() {
        parts.push(format!("granted {}", config.capabilities.granted.join(" ")));
    }
    parts.join(", ")
}

fn run_init(data_dir: Option<PathBuf>) -> anyhow::Result<ExitCode> {
    let target = data_dir.or_else(init::default_target).context("cannot find a home directory, so pass --data-dir")?;
    for line in init::init(&target, &repo_assets(), true)? {
        println!("{line}");
    }
    println!();
    // A build of the repository runs from target/, an install (install.sh, install-session.sh) from a bin/ folder.
    let from_cargo = std::env::current_exe().is_ok_and(|exe| exe.components().any(|c| c.as_os_str() == "target"));
    if from_cargo {
        println!("Start meerora with `cargo run -p meerora` in the repository. The app starts the daemon by itself.");
    } else {
        println!("Start meerora with `meerora --windowed` or from your app menu. The app starts the daemon by itself.");
    }
    let standard = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")))
        .map(|d| d.join("meerora"));
    let chosen = std::env::var_os("MEERORA_DATA_DIR").map(PathBuf::from);
    if Some(&target) != standard.as_ref() && Some(&target) != chosen.as_ref() {
        println!("This is not the default data dir, so set MEERORA_DATA_DIR={} for the app and the daemon.", target.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn count_errors(report: &Report) -> String {
    match report.errors.len() {
        1 => "1 error".into(),
        n => format!("{n} errors"),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use meerora_protocol::Event;

    fn parse(args: &[&str]) -> anyhow::Result<Cli> {
        parse_args(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parses_the_daemon_flags() {
        assert_eq!(parse(&[]).unwrap(), Cli::Run { paths: Paths::default(), socket: None, speed: 1.0 });
        let cli = parse(&["--data-dir", "/d", "--scenario=/s.toml", "--socket", "/x.sock", "--speed", "2"]).unwrap();
        let paths = Paths { data_dir: Some("/d".into()), scenario: Some("/s.toml".into()) };
        assert_eq!(cli, Cli::Run { paths, socket: Some("/x.sock".into()), speed: 2.0 });
        assert_eq!(parse(&["check", "--help"]).unwrap(), Cli::Help);
    }

    #[test]
    fn parses_the_subcommands() {
        let send = parse(&["send", "--socket", "/x.sock", r#"{"type":"go_to","agent":"wren","station":"door"}"#]).unwrap();
        let event = Event::GoTo { agent: "wren".into(), station: "door".into() };
        assert_eq!(send, Cli::Client { socket: Some("/x.sock".into()), command: Command::Inject { event } });
        assert_eq!(parse(&["ack", "wren"]).unwrap(), Cli::Client { socket: None, command: Command::Ack { agent: "wren".into() } });
        assert_eq!(parse(&["cancel", "wren"]).unwrap(), Cli::Client { socket: None, command: Command::Cancel { agent: "wren".into() } });
        assert_eq!(parse(&["reload"]).unwrap(), Cli::Client { socket: None, command: Command::ReloadAgents });
        assert_eq!(
            parse(&["say", "claude", "hi there"]).unwrap(),
            Cli::Say { socket: None, agent: "claude".into(), text: "hi there".into() }
        );
        assert_eq!(
            parse(&["check", "--data-dir", "/d"]).unwrap(),
            Cli::Check { paths: Paths { data_dir: Some("/d".into()), scenario: None } }
        );
        assert_eq!(parse(&["init", "--data-dir", "/d"]).unwrap(), Cli::Init { data_dir: Some("/d".into()) });
        assert_eq!(
            parse(&["retire", "wren"]).unwrap(),
            Cli::Graveyard { socket: None, command: Some(Command::Retire { agent: "wren".into() }) }
        );
        assert_eq!(
            parse(&["restore", "--socket", "/x.sock", "wren-2"]).unwrap(),
            Cli::Graveyard { socket: Some("/x.sock".into()), command: Some(Command::Restore { agent: "wren-2".into() }) }
        );
        assert_eq!(parse(&["graveyard"]).unwrap(), Cli::Graveyard { socket: None, command: None });
    }

    #[test]
    fn rejects_bad_arguments() {
        for args in [
            &["--speed", "0"][..],
            &["--speed", "fast"],
            &["--socket"],
            &["--colour", "red"],
            &["-v"],
            &["extra"],
            &["check", "--speed", "2"],
            &["init", "--socket", "/x"],
            &["send", "{not json}"],
            &["send"],
            &["ack", "a", "b"],
            &["say", "claude"],
            &["reload", "x"],
            &["retire"],
            &["restore", "a", "b"],
            &["graveyard", "x"],
        ] {
            assert!(parse(args).is_err(), "{args:?} should fail");
        }
    }
}
