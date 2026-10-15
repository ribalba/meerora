//! Checks of the runtimes for `meerora-agentd check` (D22). Everything here
//! is a warning, because a missing runtime only stops the agents that use it.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use meerora_protocol::{AgentDir, RuntimeConfig, RuntimeKind, Settings};

use crate::builder;

const OLLAMA_URL: &str = "http://localhost:11434";

/// Warnings about the runtimes of the agents and of the builder.
pub fn runtime_warnings(agents: &[AgentDir], settings: &Settings) -> Vec<String> {
    let mut warnings = Vec::new();
    for agent in agents {
        if let Some(config) = &agent.config.runtime
            && let Some(problem) = check_runtime(config, settings)
        {
            warnings.push(format!("agent {} {problem}", agent.id));
        }
    }
    let builder = settings.builder.clone().unwrap_or_else(builder::default_runtime);
    if let Some(problem) = check_runtime(&builder, settings) {
        warnings.push(format!("the builder {problem}"));
    }
    warnings
}

fn check_runtime(config: &RuntimeConfig, settings: &Settings) -> Option<String> {
    match config.kind {
        RuntimeKind::ClaudeCli => binary(settings.paths.claude.as_deref(), "claude", "paths.claude"),
        RuntimeKind::CodexCli => binary(settings.paths.codex.as_deref(), "codex", "paths.codex"),
        RuntimeKind::Ollama => {
            let url = config.url.as_deref().or(settings.paths.ollama.as_deref()).unwrap_or(OLLAMA_URL);
            ollama_answers(url).err().map(|e| format!("runs on ollama, but {url} does not answer ({e})"))
        }
        RuntimeKind::External => config.url.is_none().then(|| "is external but has no [runtime] url, so import it again".into()),
        RuntimeKind::Api => Some("runs on api, which has no runtime yet".into()),
        RuntimeKind::Mock => None,
    }
}

/// Whether the per-agent sandbox works (D16). `Ok` is one line for `check`,
/// `Err` the warning when it is on and does not work.
pub fn sandbox(settings: &Settings) -> Result<String, String> {
    if !settings.sandbox.enabled {
        return Ok("off ([sandbox] enabled = false), agents run with the rights of milestone 2 (D29)".into());
    }
    meerora_runtime::sandbox::check().map_err(|e| format!("the sandbox does not work, so real agents fail until it does. {e}"))
}

fn binary(configured: Option<&Path>, name: &str, setting: &str) -> Option<String> {
    match configured {
        Some(path) if is_executable(path) => None,
        Some(path) => Some(format!("needs {name}, but {} from {setting} is not an executable file", path.display())),
        None if which(name).is_some() => None,
        None => Some(format!("needs {name}, which is not on PATH. Install it or set {setting} in meerora.toml.")),
    }
}

/// The first executable `name` on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Asks Ollama for its version with a plain HTTP request. HTTPS URLs only get a connection test.
fn ollama_answers(url: &str) -> Result<(), String> {
    let (https, rest) = match url.split_once("://") {
        Some(("https", rest)) => (true, rest),
        Some((_, rest)) => (false, rest),
        None => (false, url),
    };
    let host_port = rest.split('/').next().unwrap_or(rest);
    let address = if host_port.contains(':') { host_port.to_string() } else { format!("{host_port}:{}", if https { 443 } else { 80 }) };
    let socket = address.to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("no address")?;
    let mut stream = TcpStream::connect_timeout(&socket, Duration::from_secs(1)).map_err(|e| e.to_string())?;
    if https {
        return Ok(());
    }
    stream.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    stream.write_all(format!("GET /api/version HTTP/1.0\r\nHost: {host_port}\r\n\r\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut head = [0u8; 32];
    let n = stream.read(&mut head).map_err(|e| e.to_string())?;
    let status = String::from_utf8_lossy(&head[..n]);
    if status.starts_with("HTTP/1.") && status.contains(" 200") {
        Ok(())
    } else {
        Err(format!("unexpected answer {:?}", status.lines().next().unwrap_or("")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::tests::agent;

    #[test]
    fn missing_binaries_and_dead_ollama_are_warnings() {
        let settings = Settings {
            paths: meerora_protocol::PathSettings {
                claude: Some("/nowhere/claude".into()),
                codex: Some("/bin/sh".into()),
                ollama: Some("http://127.0.0.1:9".into()),
            },
            ..Settings::default()
        };
        let agents = vec![
            agent("a", "name = \"A\"\nhome = \"desk\"\n[runtime]\nkind = \"claude-cli\""),
            agent("b", "name = \"B\"\nhome = \"desk\"\n[runtime]\nkind = \"codex-cli\""),
            agent("c", "name = \"C\"\nhome = \"desk\"\n[runtime]\nkind = \"ollama\""),
            agent("d", "name = \"D\"\nhome = \"desk\"\n[runtime]\nkind = \"mock\""),
        ];
        let warnings = runtime_warnings(&agents, &settings).join("\n");
        assert!(warnings.contains("agent a needs claude, but /nowhere/claude"), "{warnings}");
        assert!(!warnings.contains("agent b"), "{warnings}");
        assert!(warnings.contains("agent c runs on ollama, but http://127.0.0.1:9 does not answer"), "{warnings}");
        assert!(!warnings.contains("agent d"), "{warnings}");
        assert!(warnings.contains("the builder needs claude"), "{warnings}");
    }
}
