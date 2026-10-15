//! The daemon's index client (D25): searching the meerora index and
//! importing external agents into agent directories (D21). Both talk to the
//! network, so they run on their own threads and report back through
//! [`Msg::Index`]. Also the command line clients behind `meerora-agentd
//! search` and `import`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};
use meerora_protocol::{
    AgentConfig, App, CapabilitiesConfig, Command, DEFAULT_INDEX_URL, EXTERNAL_TRIANGLES, Event, ExternalAgent, IndexEntry, Manifest,
    OnDone, PROTOCOL_VERSION, Role, RuntimeConfig, RuntimeKind, decode_event, encode, load_agents,
};
use meerora_script::{FigureDesc, Limits, build_figure};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::daemon::{Daemon, Msg, Outgoing};
use crate::runs::now_ms;

/// The endpoint protocol meerora speaks (Q9).
pub const PROTOCOL: &str = "a2a/1.0";
/// Search results whose figures are fetched for the preview.
const PREVIEWS: usize = 20;
/// The largest figure script, agent card and index answer meerora reads.
const MAX_FIGURE: u64 = 256 * 1024;
const MAX_CARD: u64 = 64 * 1024;
const MAX_ANSWER: u64 = 4 * 1024 * 1024;
/// Where an imported agent arrives and waits (D25).
const DOOR: &str = "door";

/// What an index thread reports.
pub enum Done {
    Results { client: u64, request: String, result: Result<Vec<IndexEntry>, String> },
    Imported { client: u64, manifest: String, result: Result<Imported, String> },
}

/// A finished import.
pub struct Imported {
    pub agent: String,
    /// False when the directory existed and was updated.
    pub created: bool,
    pub welcome: String,
}

impl Daemon {
    /// The index the daemon talks to: `[index] url` of `meerora.toml`, else
    /// the local default (Q10).
    pub(crate) fn index_url(&self) -> String {
        self.opts.settings.index.url.clone().unwrap_or_else(|| DEFAULT_INDEX_URL.into()).trim_end_matches('/').to_string()
    }

    /// `Command::IndexSearch`, answered with `Event::IndexResults` to `client`.
    pub(crate) fn index_search(&mut self, client: u64, request: String, query: String) {
        let (index, tx) = (self.index_url(), self.tx.clone());
        // With an account the index shows friends their friends' profile fields (D26).
        let token = crate::social::token_for(&self.opts.data_dir, &index);
        info!("client {client} searches the index for {query:?}");
        let spawned = std::thread::Builder::new().name("index-search".into()).spawn(move || {
            let result = search_as(&index, &query, token.as_deref());
            let _ = tx.send(Msg::Index(Done::Results { client, request, result }));
        });
        if let Err(e) = spawned {
            warn!("cannot start a search thread ({e})");
        }
    }

    /// `Command::ImportAgent`, answered with `Event::Imported` to `client`.
    pub(crate) fn import_agent(&mut self, client: u64, manifest: String) {
        let (index, tx, data_dir) = (self.index_url(), self.tx.clone(), self.opts.data_dir.clone());
        let stations = &self.opts.stations;
        let home = if stations.iter().any(|s| s == DOOR) { DOOR } else { stations.first().map_or(DOOR, String::as_str) }.to_string();
        info!("client {client} imports {manifest} from {index}");
        let spawned = std::thread::Builder::new().name("index-import".into()).spawn(move || {
            let result = import(&data_dir, &index, &manifest, &home);
            let _ = tx.send(Msg::Index(Done::Imported { client, manifest, result }));
        });
        if let Err(e) = spawned {
            warn!("cannot start an import thread ({e})");
        }
    }

    pub(crate) fn on_index(&mut self, done: Done) {
        match done {
            Done::Results { client, request, result } => {
                let (results, error) = match result {
                    Ok(results) => (results, None),
                    Err(e) => {
                        warn!("index search failed ({e})");
                        (Vec::new(), Some(e))
                    }
                };
                self.out.push(Outgoing::To(client, Event::IndexResults { request, results, error }));
            }
            Done::Imported { client, manifest, result } => match result {
                Ok(imported) => {
                    info!("{manifest} {} agents/{}", if imported.created { "moved into" } else { "updated" }, imported.agent);
                    // The answer goes out before the agent's events, so the
                    // client that asked knows the directory when they arrive.
                    let agent = imported.agent.clone();
                    self.out.push(Outgoing::To(client, Event::Imported { manifest, agent: Some(agent.clone()), error: None }));
                    // The same path as Command::ReloadAgents.
                    self.handle_command(client, Command::ReloadAgents);
                    if imported.created && self.agents.contains_key(&agent) {
                        let id = self.next_id(&agent);
                        self.post(&agent, id, Role::Agent, imported.welcome, now_ms());
                    }
                }
                Err(e) => {
                    warn!("importing {manifest} failed ({e})");
                    self.out.push(Outgoing::To(client, Event::Imported { manifest, agent: None, error: Some(e) }));
                }
            },
        }
    }
}

fn http() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(15)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// GETs `url` and returns at most `limit` bytes of a 200 answer.
fn fetch(http: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    read_answer(url, http.get(url).call(), limit)
}

/// [`fetch`] with the account's bearer token.
fn fetch_as(http: &ureq::Agent, url: &str, limit: u64, token: &str) -> Result<Vec<u8>, String> {
    read_answer(url, http.get(url).header("Authorization", &format!("Bearer {token}")).call(), limit)
}

fn read_answer(url: &str, response: Result<ureq::http::Response<ureq::Body>, ureq::Error>, limit: u64) -> Result<Vec<u8>, String> {
    let response = response.map_err(|e| match e {
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => format!("cannot reach {url}"),
        ureq::Error::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => format!("cannot reach {url}"),
        ureq::Error::Timeout(_) => format!("{url} did not answer in time"),
        other => format!("cannot fetch {url} ({other})"),
    })?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response.into_body().into_reader().take(limit + 1).read_to_end(&mut bytes).map_err(|e| format!("cannot read {url} ({e})"))?;
    if status != 200 {
        let text = String::from_utf8_lossy(&bytes);
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| text.lines().next().unwrap_or("").chars().take(160).collect());
        return Err(format!("{url} answered {status}: {message}"));
    }
    if bytes.len() as u64 > limit {
        return Err(format!("{url} is larger than {} KB", limit / 1024));
    }
    Ok(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The figure script of a manifest, checked against its sha256.
fn figure_script(http: &ureq::Agent, manifest: &Manifest) -> Result<String, String> {
    let bytes = fetch(http, &manifest.figure.url, MAX_FIGURE)?;
    if !sha256_hex(&bytes).eq_ignore_ascii_case(manifest.figure.sha256.trim()) {
        return Err(format!("the figure of {} does not match the checksum in its manifest", manifest.name));
    }
    String::from_utf8(bytes).map_err(|_| format!("the figure of {} is not text", manifest.name))
}

/// Searches the index and adds the checked figure scripts for the preview.
#[cfg(test)]
pub fn search(index: &str, query: &str) -> Result<Vec<IndexEntry>, String> {
    search_as(index, query, None)
}

/// [`search`] with the account's token, so people's profiles come filtered for the user (D26).
pub fn search_as(index: &str, query: &str, token: Option<&str>) -> Result<Vec<IndexEntry>, String> {
    let http = http();
    let url = format!("{index}/v1/search?q={}", encode_query(query));
    let bytes = match token {
        Some(token) => fetch_as(&http, &url, MAX_ANSWER, token),
        None => fetch(&http, &url, MAX_ANSWER),
    }
    .map_err(|e| index_error(index, e))?;
    let mut results: Vec<IndexEntry> =
        serde_json::from_slice(&bytes).map_err(|e| format!("the index at {index} sent results meerora cannot read ({e})"))?;
    std::thread::scope(|scope| {
        let http = &http;
        for entry in results.iter_mut().take(PREVIEWS) {
            scope.spawn(move || match figure_script(http, &entry.manifest) {
                Ok(script) => entry.figure_script = Some(script),
                Err(e) => {
                    info!("no preview for {} ({e})", entry.manifest.id);
                    entry.figure_script = None;
                }
            });
        }
    });
    Ok(results)
}

fn index_error(index: &str, e: String) -> String {
    if e.starts_with("cannot reach") {
        format!("The meerora index at {index} does not answer. Start one with meerora-index serve, or set [index] url in meerora.toml.")
    } else {
        e
    }
}

/// Fetches a manifest and its figure and writes the agent directory.
/// Importing the same manifest again updates the directory it went to.
pub fn import(data_dir: &Path, index: &str, id: &str, home: &str) -> Result<Imported, String> {
    let http = http();
    let url = format!("{index}/v1/manifests/{id}");
    let bytes = fetch(&http, &url, MAX_ANSWER).map_err(|e| index_error(index, e))?;
    let entry: IndexEntry = serde_json::from_slice(&bytes).map_err(|e| format!("the index sent a manifest meerora cannot read ({e})"))?;
    let m = &entry.manifest;
    if m.id != id {
        return Err(format!("the index answered with {} instead of {id}", m.id));
    }
    if m.endpoint.protocol != PROTOCOL {
        return Err(format!("{} speaks {}, and meerora speaks {PROTOCOL}", m.name, m.endpoint.protocol));
    }
    let script = figure_script(&http, m)?;
    let figure = check_figure(&script, &m.name)?;
    let card = m.a2a_card.as_deref().and_then(|url| match fetch(&http, url, MAX_CARD) {
        Ok(bytes) if serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|v| v.is_object()) => Some(bytes),
        Ok(_) => {
            warn!("the agent card of {id} is not a JSON object, so the runtime finds out about streaming itself");
            None
        }
        Err(e) => {
            warn!("no agent card for {id} ({e})");
            None
        }
    });
    write_agent(data_dir, index, &entry, &script, &figure, card.as_deref(), home)
}

/// Builds the figure in the sandbox and holds it to the external budget (Q13).
pub fn check_figure(script: &str, name: &str) -> Result<FigureDesc, String> {
    let limits = Limits { hard_triangle_cap: EXTERNAL_TRIANGLES, ..Limits::default() };
    let figure =
        build_figure(script, "figure.js", &limits).map_err(|e| format!("the figure of {name} does not pass the validator ({e})"))?;
    if figure.triangles > EXTERNAL_TRIANGLES {
        return Err(format!("the figure of {name} has {} triangles, external figures may have {EXTERNAL_TRIANGLES}", figure.triangles));
    }
    Ok(figure)
}

fn write_agent(
    data_dir: &Path,
    index: &str,
    entry: &IndexEntry,
    script: &str,
    figure: &FigureDesc,
    card: Option<&[u8]>,
    home: &str,
) -> Result<Imported, String> {
    let m = &entry.manifest;
    let agents = data_dir.join("agents");
    let existing =
        load_agents(data_dir).into_iter().filter_map(Result::ok).find(|a| a.config.external.as_ref().is_some_and(|e| e.manifest == m.id));
    let (id, created) = match &existing {
        Some(dir) => (dir.id.clone(), false),
        None => (free_slug(&agents, &m.name), true),
    };
    let dir = agents.join(&id);
    let config = AgentConfig {
        name: m.name.trim().to_string(),
        color: dominant_color(figure),
        figure: "figure.js".into(),
        home: existing.as_ref().map_or_else(|| home.to_string(), |d| d.config.home.clone()),
        instructions: None,
        on_done: OnDone::Wait,
        apps: vec![App::Chat],
        workspace: Vec::new(),
        runtime: Some(RuntimeConfig { kind: RuntimeKind::External, model: None, url: Some(m.endpoint.url.clone()) }),
        schedule: Vec::new(),
        budget: None,
        capabilities: CapabilitiesConfig::default(),
        external: Some(ExternalAgent {
            manifest: m.id.clone(),
            domain: m.owner.domain.clone(),
            verified: entry.verified,
            index: index.to_string(),
            privacy: m.privacy.clone(),
        }),
        role: None,
    };
    let toml = toml::to_string(&config).map_err(|e| e.to_string())?;
    let header = format!(
        "# Imported from the meerora index at {}. What you type to {} goes to {}.\n# It gets no files and no tools.\n",
        comment_text(index),
        comment_text(&config.name),
        comment_text(&m.endpoint.url)
    );
    std::fs::create_dir_all(dir.join("config")).map_err(|e| format!("cannot create {} ({e})", dir.display()))?;
    // The figure first, so the agent never appears without it.
    write_if_changed(&dir.join("figure.js"), script.as_bytes())?;
    if let Some(card) = card {
        write_if_changed(&dir.join(meerora_runtime::EXTERNAL_CARD_FILE), card)?;
    }
    write_if_changed(&dir.join("agent.toml"), format!("{header}{toml}").as_bytes())?;
    Ok(Imported { agent: id, created, welcome: m.welcome.clone() })
}

/// Text from the index for a TOML comment. A line break must not end the
/// comment and start a line of settings.
fn comment_text(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if std::fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    meerora_protocol::safefs::replace_file(path, bytes).map_err(|e| format!("cannot write {} ({e})", path.display()))
}

/// A directory name from the agent's name that is not taken yet.
fn free_slug(agents: &Path, name: &str) -> String {
    let s: String = name.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    let base = if s.is_empty() { "agent".to_string() } else { s };
    let mut id = base.clone();
    let mut n = 2;
    while agents.join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// The colour that covers the most surface of the figure, ignoring glowing
/// parts, as `#RRGGBB`.
pub fn dominant_color(figure: &FigureDesc) -> String {
    let mesh = &figure.base;
    let mut areas: Vec<([u8; 3], f32)> = Vec::new();
    for tri in mesh.indices.as_chunks::<3>().0 {
        let [a, b, c] = tri.map(|i| &mesh.vertices[i as usize]);
        if a.emissive > 0.0 {
            continue;
        }
        let (p, q, r) = (a.position, b.position, c.position);
        let u = [q[0] - p[0], q[1] - p[1], q[2] - p[2]];
        let v = [r[0] - p[0], r[1] - p[1], r[2] - p[2]];
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        let area = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt() / 2.0;
        let rgb = a.color.map(|v| (linear_to_srgb(v) * 255.0).round().clamp(0.0, 255.0) as u8);
        match areas.iter_mut().find(|(c, _)| *c == rgb) {
            Some((_, total)) => *total += area,
            None => areas.push((rgb, area)),
        }
    }
    areas
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map_or_else(|| "#E8EAF0".into(), |(c, _)| format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2]))
}

fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// Encodes a query value for a URL.
fn encode_query(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") }
        })
        .collect()
}

/// How long the command line clients wait for the daemon's answer.
const CLI_TIMEOUT: Duration = Duration::from_secs(60);

/// Sends `command` and returns the first event `answer` accepts.
fn ask_daemon(socket: &Path, command: &Command, answer: impl Fn(&Event) -> bool) -> anyhow::Result<Event> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("cannot connect to {}. Is meerora-agentd running?", socket.display()))?;
    stream.set_read_timeout(Some(CLI_TIMEOUT))?;
    let hello = Command::Hello { version: PROTOCOL_VERSION, client: format!("meerora-agentd {} (index)", env!("CARGO_PKG_VERSION")) };
    stream.write_all(format!("{}{}", encode(&hello), encode(command)).as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("no answer from the daemon")? == 0 {
            bail!("the daemon closed the connection");
        }
        if let Ok(event) = decode_event(&line)
            && answer(&event)
        {
            return Ok(event);
        }
    }
}

/// `meerora-agentd search` and `import`: sends the command and prints the answer.
pub fn run_cli(socket: &Path, command: Command, out: &mut dyn Write) -> anyhow::Result<()> {
    match command {
        Command::IndexSearch { query, .. } => search_cli(socket, &query, out),
        Command::ImportAgent { manifest } => import_cli(socket, &manifest, out),
        other => bail!("{other:?} is not an index command"),
    }
}

/// Prints one line per result and its welcome text below.
fn search_cli(socket: &Path, query: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let request = format!("cli-{}", now_ms());
    let command = Command::IndexSearch { request: request.clone(), query: query.into() };
    let event = ask_daemon(socket, &command, |e| matches!(e, Event::IndexResults { request: r, .. } if *r == request))?;
    let Event::IndexResults { results, error, .. } = event else { unreachable!("ask_daemon checked the event") };
    if let Some(error) = error {
        bail!("{error}");
    }
    if results.is_empty() {
        writeln!(out, "nothing found for {query:?}")?;
    }
    for e in &results {
        let m = &e.manifest;
        let kind = match m.kind {
            meerora_protocol::ManifestKind::Organisation => "organisation",
            meerora_protocol::ManifestKind::Person => "person",
        };
        writeln!(
            out,
            "{}  {} · {} · {} {} · {kind} · figure {}",
            m.id,
            m.name,
            m.owner.name,
            m.owner.domain,
            if e.verified { "verified" } else { "unverified" },
            if e.figure_script.is_some() { "checked" } else { "missing" },
        )?;
        writeln!(out, "    {}", m.welcome)?;
    }
    Ok(())
}

/// Prints the directory the agent moved into.
fn import_cli(socket: &Path, manifest: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let command = Command::ImportAgent { manifest: manifest.into() };
    let event = ask_daemon(socket, &command, |e| matches!(e, Event::Imported { manifest: m, .. } if m == manifest))?;
    let Event::Imported { agent, error, .. } = event else { unreachable!("ask_daemon checked the event") };
    match (agent, error) {
        (Some(agent), _) => {
            writeln!(out, "imported {manifest} into agents/{agent}. Talk to it with: meerora-agentd say {agent} \"hello\"")?;
            Ok(())
        }
        (None, Some(error)) => bail!("{error}"),
        (None, None) => bail!("the import of {manifest} failed without a reason"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};

    use meerora_index::Demo;
    use meerora_protocol::{IndexSettings, Settings, load_agent};

    use super::*;
    use crate::daemon::{Options, RuntimeFactory};
    use crate::scenario::Scenario;
    use crate::store::tests::TempDir;

    fn index_url(demo: &Demo) -> String {
        demo.index.url()
    }

    #[test]
    fn index_text_cannot_leave_the_comment() {
        let header = format!("# {}\n", comment_text("Mira\n[capabilities]\ngranted = [\"shell\"]"));
        assert_eq!(header.lines().count(), 1);
        assert!(toml::from_str::<toml::Table>(&header).unwrap().is_empty());
    }

    #[test]
    fn the_command_line_has_search_and_import() {
        let parse = |args: &[&str]| crate::parse_args(args.iter().map(|s| s.to_string()).collect());
        let search = Command::IndexSearch { request: String::new(), query: "eu data".into() };
        assert_eq!(parse(&["search", "eu data"]).unwrap(), crate::Cli::Index { socket: None, command: search });
        let import = Command::ImportAgent { manifest: "meridian.example/mira".into() };
        assert_eq!(
            parse(&["import", "--socket", "/x.sock", "meridian.example/mira"]).unwrap(),
            crate::Cli::Index { socket: Some("/x.sock".into()), command: import }
        );
        assert!(parse(&["import"]).is_err() && parse(&["search", "a", "b"]).is_err());
    }

    #[test]
    fn search_adds_checked_figures() {
        let demo = Demo::start().unwrap();
        let results = search(&index_url(&demo), "ferry").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].manifest.name, "Tess");
        let script = results[0].figure_script.as_deref().expect("the figure passed its checksum");
        assert_eq!(sha256_hex(script.as_bytes()), results[0].manifest.figure.sha256);
        let all = search(&index_url(&demo), "").unwrap();
        // Alex is verified through the demo account (D26), so verified Alex and Mira come first.
        assert_eq!(all.iter().map(|e| e.manifest.name.as_str()).collect::<Vec<_>>(), ["Alex", "Mira", "Tess"]);

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let err = search(&format!("http://{closed}"), "x").unwrap_err();
        assert!(err.contains("does not answer") && err.contains("meerora-index serve"), "{err}");
    }

    #[test]
    fn import_writes_an_external_agent_and_updates_it_later() {
        let demo = Demo::start().unwrap();
        let data = TempDir::new("import");
        let first = import(&data.0, &index_url(&demo), "meridian.example/mira", "door").unwrap();
        assert_eq!((first.agent.as_str(), first.created), ("mira", true));
        let dir = load_agent(&data.0.join("agents/mira")).unwrap();
        let c = &dir.config;
        assert_eq!(c.name, "Mira");
        assert_eq!(c.color, "#1F6F6B", "the teal body covers the most surface");
        assert_eq!(c.home, "door");
        assert_eq!(c.apps, [App::Chat]);
        assert_eq!(c.capabilities, CapabilitiesConfig::default());
        assert!(c.schedule.is_empty() && c.instructions.is_none());
        let runtime = c.runtime.as_ref().unwrap();
        assert_eq!(runtime.kind, RuntimeKind::External);
        assert_eq!(runtime.url.as_deref(), Some(format!("{}/a2a/mira", demo.agents.url()).as_str()));
        let external = c.external.as_ref().unwrap();
        assert_eq!(external.manifest, "meridian.example/mira");
        assert_eq!(external.domain, "meridian.example");
        assert!(external.verified);
        assert_eq!(external.index, index_url(&demo));
        assert_eq!(external.privacy.as_deref(), Some("Meridian sees what you say to Mira and nothing else."));
        let toml = std::fs::read_to_string(data.0.join("agents/mira/agent.toml")).unwrap();
        assert!(toml.starts_with("# Imported from the meerora index"), "{toml}");
        let card: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(data.0.join("agents/mira/config/agent-card.json")).unwrap()).unwrap();
        assert_eq!(card["capabilities"]["streaming"], true);
        assert!(std::fs::read_to_string(data.0.join("agents/mira/figure.js")).unwrap().contains("Meridian"));

        // An agent of the user called Mira keeps its directory, and a second
        // import goes back to the same one.
        std::fs::create_dir_all(data.0.join("agents/mira-own")).unwrap();
        let again = import(&data.0, &index_url(&demo), "meridian.example/mira", "door").unwrap();
        assert_eq!((again.agent.as_str(), again.created), ("mira", false));
        assert_eq!(std::fs::read_dir(data.0.join("agents")).unwrap().count(), 2);

        let person = import(&data.0, &index_url(&demo), "alex/alex", "door").unwrap();
        let alex = load_agent(&data.0.join("agents").join(&person.agent)).unwrap();
        assert!(alex.config.external.unwrap().verified, "people are verified through their account");
    }

    #[test]
    fn a_figure_that_does_not_match_its_checksum_is_refused() {
        let demo = Demo::start().unwrap();
        let data = TempDir::new("import-sha");
        // A small index whose manifest points at Mira's figure with a wrong checksum.
        let mut entry: IndexEntry =
            http().get(&format!("{}/v1/manifests/meridian.example/mira", index_url(&demo))).call().unwrap().body_mut().read_json().unwrap();
        entry.manifest.figure.sha256 = "0".repeat(64);
        let body = serde_json::to_vec(&entry).unwrap();
        let fake = meerora_index::http::Server::start(
            "127.0.0.1:0",
            std::sync::Arc::new(move |_req: meerora_index::http::Request| {
                meerora_index::http::Response::bytes(200, "application/json", body.clone())
            }),
        )
        .unwrap();
        let err = import(&data.0, &fake.url(), "meridian.example/mira", "door").err().expect("refused");
        assert!(err.contains("does not match the checksum"), "{err}");
        assert!(!data.0.join("agents").exists(), "nothing was written");
    }

    #[test]
    fn figures_over_the_external_budget_are_refused() {
        let wren = include_str!("../../../assets/agents/wren/figure.js");
        let local = build_figure(wren, "wren.js", &Limits::default()).unwrap();
        let result = check_figure(wren, "Wren");
        if local.triangles > EXTERNAL_TRIANGLES {
            assert!(result.is_err());
        }
        let big = "const b = sphere(20, 4).material(\"felt\", \"#888888\");\n\
                   export default figure({ parts: [b], anchors: { ring: b.top.above(10), face: b }, \
                   animations: { working: bob(), waiting: hop(), resting: breathe() }, budget: { triangles: 6000 } });";
        let err = check_figure(big, "Big").unwrap_err();
        assert!(err.contains("1500"), "{err}");
        assert!(check_figure(meerora_index::demo::AGENTS[0].figure, "Mira").is_ok());
    }

    /// A daemon on a temp data dir that points at the demo index.
    struct Harness {
        daemon: Daemon,
        rx: Receiver<Msg>,
        data: TempDir,
    }

    impl Harness {
        fn new(index: &str) -> Self {
            let data = TempDir::new("index-daemon");
            // Reloading reads meerora.toml again, so the index URL lives there.
            std::fs::write(data.0.join("meerora.toml"), format!("[index]\nurl = \"{index}\"\n")).unwrap();
            let (tx, rx) = mpsc::channel();
            let factory: RuntimeFactory = crate::daemon::real_runtimes();
            let options = Options {
                data_dir: data.0.clone(),
                socket: data.0.join("agentd.sock"),
                settings: Settings { index: IndexSettings { url: Some(index.into()) }, ..Settings::default() },
                stations: crate::scenario::tests::stations(),
                factory,
                bridge: None,
                git: false,
                speed: 1.0,
            };
            let daemon = Daemon::new(options, Scenario::default(), Vec::new(), tx);
            Self { daemon, rx, data }
        }

        /// Handles messages until an outgoing event matches.
        fn until(&mut self, done: impl Fn(&Outgoing) -> bool) -> Vec<Event> {
            let mut events = Vec::new();
            loop {
                let msg = self.rx.recv_timeout(Duration::from_secs(10)).expect("an answer before the timeout");
                self.daemon.handle_msg(msg);
                let new = self.daemon.take_out();
                let finished = new.iter().any(&done);
                events.extend(new.into_iter().map(|o| match o {
                    Outgoing::All(e) | Outgoing::To(_, e) => e,
                }));
                if finished {
                    return events;
                }
            }
        }

        fn reply(&mut self, agent: &str, text: &str) -> String {
            self.daemon.handle_command(7, Command::Say { agent: agent.into(), text: text.into() });
            let events = self.until(|o| matches!(o, Outgoing::All(Event::RunFinished { agent: a, .. }) if a == agent));
            events
                .iter()
                .rev()
                .find_map(|e| match e {
                    Event::Message { agent: a, role: Role::Agent, text, done: true, .. } if a == agent => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no reply in {events:?}"))
        }
    }

    #[test]
    fn the_daemon_searches_imports_and_talks_to_the_agent() {
        let demo = Demo::start().unwrap();
        let mut h = Harness::new(&index_url(&demo));
        let to_client = |o: &Outgoing, f: fn(&Event) -> bool| matches!(o, Outgoing::To(7, e) if f(e));

        h.daemon.handle_command(7, Command::IndexSearch { request: "r1".into(), query: "data residency".into() });
        assert!(h.daemon.take_out().is_empty(), "the search runs off the daemon thread");
        let events = h.until(|o| to_client(o, |e| matches!(e, Event::IndexResults { .. })));
        let Some(Event::IndexResults { request, results, error }) = events.last() else { panic!("{events:?}") };
        assert_eq!((request.as_str(), error), ("r1", &None));
        assert_eq!(results[0].manifest.id, "meridian.example/mira");
        assert!(results[0].verified && results[0].figure_script.is_some());

        h.daemon.handle_command(7, Command::ImportAgent { manifest: "meridian.example/mira".into() });
        let events = h.until(|o| to_client(o, |e| matches!(e, Event::Imported { .. })));
        assert!(events.contains(&Event::AgentSpawned { agent: "mira".into(), name: "Mira".into(), system: false }));
        assert!(events.contains(&Event::GoTo { agent: "mira".into(), station: "door".into() }), "external agents arrive at the door");
        assert!(events.iter().any(
            |e| matches!(e, Event::Message { agent, role: Role::Agent, text, .. } if agent == "mira" && text.starts_with("Hi, I'm Mira"))
        ));
        let answered = events.iter().position(|e| matches!(e, Event::Imported { agent: Some(a), error: None, .. } if a == "mira"));
        let spawned = events.iter().position(|e| matches!(e, Event::AgentSpawned { .. }));
        assert!(answered < spawned, "the answer comes before the agent's events: {events:?}");

        // The imported agent answers through the external runtime and keeps the conversation.
        assert!(h.reply("mira", "What do seats cost?").starts_with("Business Plus covers teams from 20 seats"));
        assert!(h.reply("mira", "and for 60?").contains("For 60 seats"));
        let store = crate::store::Store::load(&h.data.0.join("agents/mira"));
        assert!(store.session.is_some_and(|s| s.starts_with("ctx-")), "the context id is the session");

        // A second import updates the directory without a second welcome.
        h.daemon.handle_command(7, Command::ImportAgent { manifest: "meridian.example/mira".into() });
        let events = h.until(|o| to_client(o, |e| matches!(e, Event::Imported { .. })));
        assert!(!events.iter().any(|e| matches!(e, Event::Message { role: Role::Agent, .. })));
        assert!(events.iter().any(|e| matches!(e, Event::Imported { agent: Some(a), .. } if a == "mira")), "{events:?}");

        h.daemon.handle_command(7, Command::ImportAgent { manifest: "nobody.example/x".into() });
        let events = h.until(|o| to_client(o, |e| matches!(e, Event::Imported { .. })));
        assert!(matches!(events.last(), Some(Event::Imported { agent: None, error: Some(e), .. }) if e.contains("404")), "{events:?}");
    }
}
