//! `meerora-index`, the index server for external agents (D25) and a demo
//! A2A agent (Q9).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use meerora_index::{IndexOptions, start_demo_agent, start_index};
use tracing::info;

const HELP: &str = "\
meerora-index, the index where organisations and people publish external agents

Usage:
  meerora-index serve [--addr ADDR] [--data DIR] [--public-url URL] [--demo] [--demo-agent URL] [--allow-http-localhost]
  meerora-index demo-agent [--addr ADDR]
  meerora-index --help

Commands:
  serve       Run the index. Manifests are JSON files in DIR/manifests, accounts, profiles,
              follows and posts are in DIR/social.json and photos in DIR/photos.
  demo-agent  Run an A2A 1.0 agent that answers for the demo manifests with canned replies.

Options:
  --addr ADDR               Address to listen on. Defaults to 127.0.0.1:7878 for serve and
                            127.0.0.1:7879 for demo-agent.
  --data DIR                Where serve keeps manifests. Defaults to $XDG_DATA_HOME/meerora-index,
                            then ~/.local/share/meerora-index.
  --public-url URL          The https URL people reach this index at, for the photo and person agent
                            URLs it hands out. Defaults to http://ADDR, which needs
                            --allow-http-localhost for person manifests.
  --demo                    Add three demo manifests (kept in memory only) and serve their figures,
                            pictures and /.well-known/meerora.json. The demo answers for the
                            reserved .example domains, so Meridian verifies. Alex Example gets an
                            account with a profile and accepts friend requests at once, and Mira,
                            Tess and Alex have posts for the feed.
  --demo-agent URL          Where the demo manifests send prompts. Defaults to http://127.0.0.1:7879.
  --allow-http-localhost    Accept http://127.0.0.1:<port> and http://localhost:<port> in place of
                            https URLs and domains, for tests and the demo only.
  -h, --help                Show this help.

API (key: needs Authorization: Bearer TOKEN):
  GET  /v1/health
  GET  /v1/search?q=TEXT                  ranked [IndexEntry], organisations and people together
  GET  /v1/manifests/{id}                 one IndexEntry
  POST /v1/manifests                      register or update an organisation's manifest (JSON body, at
                                          most 16 KB). Verified when https://DOMAIN/.well-known/meerora.json
                                          lists the id with the SHA-256 of the posted bytes. The
                                          first unverified upload answers with an edit_secret, later
                                          unverified updates send it as Authorization: Bearer SECRET.
  POST /v1/accounts                       body {handle}, creates an account, the answer holds the token once
  GET  /v1/account                        key, who the token belongs to
  PUT  /v1/profiles/{handle}              key, a profile (section 5 format), registers the person
  GET  /v1/profiles/{handle}              the profile, friends fields only for friends
  PUT  /v1/profiles/{handle}/figure       key, the personal agent's figure script
  POST /v1/profiles/{handle}/photos       key, PNG or JPEG bytes
  GET  /v1/photos/{id}                    a photo, as the owner's profile allows
  POST /v1/follows                        key, body {actor}, follow a handle or a manifest id
  DELETE /v1/follows/{actor}              key, unfollow
  POST /v1/friends                        key, body {handle}, ask to be friends
  POST /v1/friends/{handle}/accept        key, also /decline
  GET  /v1/relations                      key, follows, followers and friend requests
  POST /v1/posts                          key, body {text, visibility}, a public or friends post
  GET  /v1/feed?limit=N&before=CURSOR     key, the feed, newest first

The log level comes from RUST_LOG and is info by default.
";

#[derive(Debug, PartialEq)]
enum Cli {
    Help,
    Serve { addr: String, data: Option<PathBuf>, options: IndexOptions },
    DemoAgent { addr: String },
}

fn parse(args: &[String]) -> anyhow::Result<Cli> {
    let mut it = args.iter();
    let mode = match it.next().map(String::as_str) {
        None | Some("-h" | "--help") => return Ok(Cli::Help),
        Some(m @ ("serve" | "demo-agent")) => m,
        Some(other) => bail!("unknown command {other:?}"),
    };
    let (mut addr, mut data, mut demo, mut demo_agent, mut local, mut public_url) = (None, None, false, None, false, None);
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next().cloned()).with_context(|| format!("{flag} needs a value"));
        match (mode, flag) {
            (_, "-h" | "--help") => return Ok(Cli::Help),
            (_, "--addr") => addr = Some(value()?),
            ("serve", "--data") => data = Some(PathBuf::from(value()?)),
            ("serve", "--demo") => demo = true,
            ("serve", "--demo-agent") => demo_agent = Some(value()?),
            ("serve", "--allow-http-localhost") => local = true,
            ("serve", "--public-url") => public_url = Some(value()?),
            _ => bail!("unknown option {flag} for {mode}"),
        }
    }
    Ok(match mode {
        "serve" => Cli::Serve {
            addr: addr.unwrap_or_else(|| "127.0.0.1:7878".into()),
            data,
            options: IndexOptions {
                allow_http_localhost: local,
                demo_agents: demo.then(|| demo_agent.unwrap_or_else(|| "http://127.0.0.1:7879".into())),
                public_url,
            },
        },
        _ => Cli::DemoAgent { addr: addr.unwrap_or_else(|| "127.0.0.1:7879".into()) },
    })
}

fn default_data() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("meerora-index"))
}

fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(ansi).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse(&args) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("error: {e:#}\n\nRun meerora-index --help for the usage.");
            return ExitCode::from(2);
        }
    };
    let result = match cli {
        Cli::Help => {
            print!("{HELP}");
            Ok(())
        }
        Cli::Serve { addr, data, options } => serve(&addr, data, &options),
        Cli::DemoAgent { addr } => demo_agent(&addr),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn serve(addr: &str, data: Option<PathBuf>, options: &IndexOptions) -> anyhow::Result<()> {
    let data = data.or_else(default_data).context("cannot find a data directory, so pass --data")?;
    let (server, index) = start_index(addr, Some(&data), options).with_context(|| format!("cannot start the index on {addr}"))?;
    info!(
        "meerora index on {} with {} manifests from {}{}",
        server.url(),
        index.store.len(),
        data.display(),
        options.demo_agents.as_ref().map(|a| format!(", demo agents at {a}")).unwrap_or_default()
    );
    server.wait();
    Ok(())
}

fn demo_agent(addr: &str) -> anyhow::Result<()> {
    let server = start_demo_agent(addr, Duration::from_millis(120)).with_context(|| format!("cannot start the demo agent on {addr}"))?;
    info!("demo A2A agent on {} (agent cards at /a2a/<name>/agent-card.json)", server.url());
    server.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> anyhow::Result<Cli> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_the_commands() {
        match p(&["serve", "--demo", "--allow-http-localhost", "--addr=127.0.0.1:0", "--data", "/tmp/x"]).unwrap() {
            Cli::Serve { addr, data, options } => {
                assert_eq!(addr, "127.0.0.1:0");
                assert_eq!(data, Some(PathBuf::from("/tmp/x")));
                assert!(options.allow_http_localhost);
                assert_eq!(options.demo_agents.as_deref(), Some("http://127.0.0.1:7879"));
            }
            other => panic!("{other:?}"),
        }
        match p(&["serve"]).unwrap() {
            Cli::Serve { addr, options, .. } => assert_eq!((addr.as_str(), options.demo_agents), ("127.0.0.1:7878", None)),
            other => panic!("{other:?}"),
        }
        match p(&["serve", "--public-url", "https://index.example.org"]).unwrap() {
            Cli::Serve { options, .. } => assert_eq!(options.public_url.as_deref(), Some("https://index.example.org")),
            other => panic!("{other:?}"),
        }
        assert_eq!(p(&["demo-agent"]).unwrap(), Cli::DemoAgent { addr: "127.0.0.1:7879".into() });
        assert_eq!(p(&[]).unwrap(), Cli::Help);
        assert!(p(&["demo-agent", "--demo"]).is_err());
        assert!(p(&["serve", "--addr"]).is_err());
        assert!(p(&["publish"]).is_err());
    }
}
