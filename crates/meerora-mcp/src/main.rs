//! `meerora-mcp --agent ID --data-dir DIR --socket PATH`
//!
//! MCP server on stdio that gives one agent's runtime the meerora tools
//! (D22). Each tool call is forwarded to the agent daemon as
//! `Command::Tool` and answered from the matching `Event::ToolResult`.
//!
//! Tools: `figure_set` (D24, the param names and values of the agent's own
//! figure), `go_to` (the stations of the space), `set_activity`, `ask_user`,
//! `note` and `request_capability` (D8), which waits up to ten minutes for
//! the user's decision while the others answer within [`CALL_TIMEOUT`]. The
//! app tools of D30 (`apps_list`, `app_read`, `app_screenshot`, `app_act`,
//! `app_open`) wait up to `daemon::APP_TIMEOUT` for the app, and a screenshot
//! comes back as MCP image content. The helper (`role = "helper"`) also gets
//! `config_read`, which waits like the app tools because `check` takes a
//! moment, and `config_change`, which waits for the user like
//! `request_capability`.
//!
//! stdout carries only JSON-RPC. Logs go to stderr.

/// One line on stderr. Failures to write it are ignored. Tests use
/// `eprintln!`, which the test harness captures.
#[cfg(not(test))]
macro_rules! log {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "meerora-mcp: {}", format_args!($($arg)*));
    }};
}
#[cfg(test)]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("meerora-mcp: {}", format_args!($($arg)*))
    };
}
pub(crate) use log;

mod daemon;
mod server;
#[cfg(test)]
mod testing;
mod tools;

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use meerora_protocol::{assets_dir, resolve_data_dir, socket_path};

use crate::daemon::Daemon;
use crate::server::Server;
use crate::tools::Tools;

/// How long a tool call waits for the daemon.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

const HELP: &str = "\
meerora-mcp, the MCP server that gives one agent's runtime the meerora tools

Usage:
  meerora-mcp --agent ID [--data-dir DIR] [--space DIR] [--socket PATH]
  meerora-mcp --help

It speaks MCP on stdin and stdout and forwards every tool call to the agent
daemon. Logs go to stderr.

Options:
  --agent ID       The agent, the name of its directory below <data dir>/agents.
  --data-dir DIR   Folder with agents/ and the rooms. Defaults to $MEERORA_DATA_DIR,
                   then ~/.local/share/meerora if it exists, then the shipped assets ($MEERORA_ASSETS,
                   <prefix>/share/meerora of this binary, then the assets/ folder of the repository).
  --space DIR      Folder of the room with its space.js. Defaults to the room in meerora.toml.
  --socket PATH    Socket of the agent daemon. Defaults to $MEERORA_SOCKET, then
                   $XDG_RUNTIME_DIR/meerora/agentd.sock, then /tmp/meerora-$USER/agentd.sock.
  -h, --help       Show this help.
";

#[derive(Debug, PartialEq)]
enum Cli {
    Help,
    Run { agent: String, data_dir: Option<PathBuf>, space: Option<PathBuf>, socket: Option<PathBuf> },
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let mut args = args.into_iter();
    let (mut agent, mut data_dir, mut space, mut socket) = (None, None, None, None);
    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            return Ok(Cli::Help);
        }
        let Some(flag) = arg.strip_prefix("--") else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        let (name, value) = match flag.split_once('=') {
            Some((name, value)) => (name.to_string(), value.to_string()),
            None => (flag.to_string(), args.next().ok_or_else(|| format!("--{flag} needs a value"))?),
        };
        match name.as_str() {
            "agent" => agent = Some(value),
            "data-dir" => data_dir = Some(PathBuf::from(value)),
            "space" => space = Some(PathBuf::from(value)),
            "socket" => socket = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option --{name}")),
        }
    }
    let agent = agent.ok_or("--agent is required")?;
    if agent.is_empty() || agent.starts_with('.') || agent.contains('/') {
        return Err(format!("{agent:?} is not an agent directory name"));
    }
    Ok(Cli::Run { agent, data_dir, space, socket })
}

fn main() -> ExitCode {
    let (agent, data_dir, space, socket) = match parse_args(std::env::args().skip(1)) {
        Ok(Cli::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Cli::Run { agent, data_dir, space, socket }) => (agent, data_dir, space, socket),
        Err(e) => {
            log!("{e}. See meerora-mcp --help.");
            return ExitCode::from(2);
        }
    };
    let data_dir = data_dir.unwrap_or_else(|| {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap_or(Path::new("."));
        resolve_data_dir(&assets_dir(&repo.join("assets")))
    });
    let socket = socket.unwrap_or_else(socket_path);
    log!("serving agent {agent} from {} with the daemon at {}", data_dir.display(), socket.display());

    let tools = match &space {
        Some(space) => Tools::load_from(&data_dir, space, &agent),
        None => Tools::load(&data_dir, &agent),
    };
    let mut server = Server::new(tools, Daemon::new(socket, &agent, CALL_TIMEOUT));
    match serve(&mut server, io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        // The client went away while this server was writing to it.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            log!("stopping after an I/O error ({e})");
            ExitCode::FAILURE
        }
    }
}

/// Answers one message per line until stdin closes.
fn serve(server: &mut Server, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        // Invalid UTF-8 becomes a parse error instead of ending the server.
        let text = String::from_utf8(std::mem::take(&mut line)).unwrap_or_else(|_| "\u{FFFD}".into());
        if let Some(reply) = server.handle_line(&text) {
            output.write_all(reply.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_line() {
        assert_eq!(
            parse_args(args(&["--agent", "wren", "--data-dir=/d", "--socket", "/s.sock"])),
            Ok(Cli::Run { agent: "wren".into(), data_dir: Some("/d".into()), space: None, socket: Some("/s.sock".into()) })
        );
        assert_eq!(parse_args(args(&["--agent=wren"])), Ok(Cli::Run { agent: "wren".into(), data_dir: None, space: None, socket: None }));
        assert_eq!(
            parse_args(args(&["--agent=wren", "--space", "/d/spaces/office"])),
            Ok(Cli::Run { agent: "wren".into(), data_dir: None, space: Some("/d/spaces/office".into()), socket: None })
        );
        assert_eq!(parse_args(args(&["--agent", "wren", "-h"])), Ok(Cli::Help));
        assert_eq!(parse_args(args(&[])), Err("--agent is required".into()));
        assert_eq!(parse_args(args(&["--agent"])), Err("--agent needs a value".into()));
        assert_eq!(parse_args(args(&["--agent", "../x"])), Err("\"../x\" is not an agent directory name".into()));
        assert_eq!(parse_args(args(&["--agent", "wren", "--speed", "2"])), Err("unknown option --speed".into()));
        assert_eq!(parse_args(args(&["wren"])), Err("unexpected argument \"wren\"".into()));
    }

    #[test]
    fn serve_answers_line_by_line() {
        let dir = testing::TestDir::new("serve");
        testing::write_data_dir(&dir);
        let mut server = Server::new(Tools::load(&dir.0, "wren"), Daemon::new(dir.path("d.sock"), "wren", CALL_TIMEOUT));
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\n\n\xff\xfe\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}";
        let mut output = Vec::new();
        serve(&mut server, &input[..], &mut output).unwrap();
        let lines: Vec<serde_json::Value> = String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[1]["error"]["code"], -32700);
        assert_eq!(lines[2]["id"], 2);
    }
}
