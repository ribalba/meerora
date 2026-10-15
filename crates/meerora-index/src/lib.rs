//! The meerora index (D25): a small HTTP JSON server where organisations
//! and people register external agent manifests, with domain verification
//! for organisations and one search over both (D26). It also holds a demo
//! A2A agent, so the path from search to chat works offline (Q9).
//!
//! People have accounts with a profile, follow and befriend each other and
//! read a feed of posts (D26, D27, [`social`]).
//!
//! This is a local index until a public one exists (Q10). It limits
//! connections, request sizes and the rate of new accounts, uploads and
//! posts per address, but has no moderation yet.

pub mod a2a;
pub mod api;
pub mod demo;
pub mod http;
pub mod rate;
pub mod search;
pub mod social;
pub mod social_api;
pub mod store;
pub mod validate;
pub mod verify;

use std::io;
use std::net::{TcpListener, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::api::Index;
use crate::http::Server;
use crate::rate::RateLimiter;
use crate::social::Social;
use crate::store::Store;
use crate::validate::Policy;
use crate::verify::Verifier;

/// Lower-case hex SHA-256, as in `figure.sha256` of a manifest.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// How to start an index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexOptions {
    pub allow_http_localhost: bool,
    /// Seeds the demo manifests whose endpoints point at this demo agent URL.
    pub demo_agents: Option<String>,
    /// The URL people reach this index at, such as `https://index.example.org`,
    /// for the URLs it hands out (photos, person manifests). Defaults to
    /// `http://<listening address>`.
    pub public_url: Option<String>,
}

/// Starts an index on `addr`. With `data` the manifests are kept in that
/// directory, without it only in memory. Demo manifests are never written.
pub fn start_index(addr: impl ToSocketAddrs, data: Option<&Path>, options: &IndexOptions) -> io::Result<(Server, Arc<Index>)> {
    let listener = TcpListener::bind(addr)?;
    let local = format!("http://{}", listener.local_addr()?);
    let base = options.public_url.as_deref().map_or_else(|| local.clone(), |u| u.trim_end_matches('/').to_string());
    let (store, social) = match data {
        Some(dir) => (Store::open(dir)?, Social::open(dir)?),
        None => (Store::memory(), Social::memory()),
    };
    let demo_agents = options.demo_agents.as_deref().map(|a| a.trim_end_matches('/').to_string());
    let index = Arc::new(Index {
        store,
        // The demo organisations verify through the index's own well-known file.
        verifier: Verifier { allow_http_localhost: options.allow_http_localhost, demo_base: demo_agents.as_ref().map(|_| local.clone()) },
        policy: Policy { allow_http_localhost: options.allow_http_localhost },
        demo_agents: demo_agents.clone(),
        social,
        base: base.clone(),
        limiter: RateLimiter::default(),
    });
    let server = Server::from_listener(listener, index.handler())?;
    if let Some(agents) = &demo_agents {
        // The server answers already, so the well-known file can be fetched.
        for agent in demo::AGENTS.iter().filter(|a| a.kind == meerora_protocol::ManifestKind::Organisation) {
            let bytes = agent.manifest_bytes(&base, agents);
            if let Err(r) = index.add(agent.manifest(&base, agents), &sha256_hex(&bytes), None, false) {
                return Err(io::Error::other(format!("demo manifest {}: {} {:?}", agent.id(), r.error, r.problems)));
            }
        }
        demo::seed_social(&index, &base, agents).map_err(|e| io::Error::other(format!("demo profiles and posts: {e}")))?;
    }
    Ok((server, index))
}

/// Starts the demo A2A agent on `addr`. `delay` is the pause between
/// streamed chunks.
pub fn start_demo_agent(addr: impl ToSocketAddrs, delay: Duration) -> io::Result<Server> {
    let listener = TcpListener::bind(addr)?;
    let base = format!("http://{}", listener.local_addr()?);
    let agents = Arc::new(a2a::DemoAgents::new(base, delay));
    Server::from_listener(listener, agents.handler())
}

/// A demo index and demo agent on free local ports, for tests here and in
/// the daemon and runtime crates.
pub struct Demo {
    pub agents: Server,
    pub index: Server,
}

impl Demo {
    pub fn start() -> io::Result<Self> {
        let agents = start_demo_agent("127.0.0.1:0", Duration::from_millis(5))?;
        let options = IndexOptions { allow_http_localhost: true, demo_agents: Some(agents.url()), public_url: None };
        let (index, _) = start_index("127.0.0.1:0", None, &options)?;
        Ok(Self { agents, index })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meerora_protocol::IndexEntry;

    #[test]
    fn sha256_of_abc() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn the_demo_index_verifies_meridian_and_serves_its_files() {
        let demo = Demo::start().unwrap();
        let http = ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build());
        let results: Vec<IndexEntry> =
            http.get(&format!("{}/v1/search?q=", demo.index.url())).call().unwrap().body_mut().read_json().unwrap();
        let verified: Vec<(&str, bool)> = results.iter().map(|e| (e.manifest.name.as_str(), e.verified)).collect();
        assert_eq!(verified, [("Alex", true), ("Mira", true), ("Tess", false)], "Alex is verified through the demo account");
        assert!(results[0].profile.as_ref().is_some_and(|p| p.places.is_empty()), "the public profile, without friends fields");
        let mira = &results[1].manifest;
        let script = http.get(&mira.figure.url).call().unwrap().body_mut().read_to_string().unwrap();
        assert_eq!(sha256_hex(script.as_bytes()), mira.figure.sha256);
        let picture = http.get(mira.picture.as_deref().unwrap()).call().unwrap().body_mut().read_to_vec().unwrap();
        assert!(picture.starts_with(b"\x89PNG"));
        let card: serde_json::Value = http.get(mira.a2a_card.as_deref().unwrap()).call().unwrap().body_mut().read_json().unwrap();
        assert_eq!(card["capabilities"]["streaming"], true);
        assert_eq!(card["supportedInterfaces"][0]["url"], mira.endpoint.url.as_str());
    }
}
