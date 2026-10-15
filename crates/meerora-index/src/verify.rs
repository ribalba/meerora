//! Domain verification for organisations (D25). The index fetches
//! `https://<domain>/.well-known/meerora.json`, and the manifest counts as
//! verified only when that file lists its id together with the SHA-256 of
//! the manifest exactly as it was posted:
//!
//! ```json
//! { "meerora": "0.1", "manifests": [
//!     { "id": "meridian.example/mira", "sha256": "<64 lower-case hex digits>" }
//! ] }
//! ```
//!
//! The hash is taken over the exact bytes of the body of `POST /v1/manifests`,
//! so `sha256sum mira.json` gives it for the file the owner posts. A copy of
//! the manifest with another endpoint has another hash, and the domain did
//! not list it. To change the manifest, the owner lists the new hash first
//! and then posts the new file. Entries may repeat an id with several hashes
//! while a change rolls out. A plain id without a hash no longer verifies.
//!
//! The URLs of a verified manifest (endpoint, figure, picture, agent card)
//! must be on the owner's domain or a name below it.
//!
//! The fetch goes to public addresses only: the host must be a public domain
//! name (see [`crate::validate::is_domain`]), the index resolves it itself,
//! refuses it when any address is loopback, private, link-local, shared
//! (CGNAT), unspecified or otherwise special, and connects to the address it
//! checked. No proxy, no redirects, 5 seconds at most.
//!
//! People are verified by their index account (D26, [`crate::social_api`]).

use std::fmt;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use meerora_protocol::{Manifest, ManifestKind};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::validate::{is_domain, is_local_host, is_within, url_host};

/// The largest well-known file the index reads.
const MAX_LISTING: u64 = 64 * 1024;
/// How long one fetch of a well-known file may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The outcome, with a note for the index log and the API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    pub verified: bool,
    pub note: String,
}

impl Verification {
    fn no(note: impl Into<String>) -> Self {
        Self { verified: false, note: note.into() }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Verifier {
    /// `127.0.0.1:<port>` and `localhost:<port>` domains are fetched over
    /// plain http, for tests and the demo.
    pub allow_http_localhost: bool,
    /// Base URL of a demo index that answers for the reserved `.example`
    /// domains (RFC 2606), which nobody can own. Only set by `--demo`.
    pub demo_base: Option<String>,
}

/// One entry of a well-known file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    id: String,
    sha256: Option<String>,
}

impl Verifier {
    /// Where the listing for `domain` lives, or None when the domain cannot be verified.
    pub fn listing_url(&self, domain: &str) -> Option<String> {
        if self.local(domain) {
            return Some(format!("http://{domain}/.well-known/meerora.json"));
        }
        if domain.ends_with(".example") {
            return self.demo_base.as_ref().map(|base| format!("{base}/.well-known/meerora.json"));
        }
        is_domain(domain).then(|| format!("https://{domain}/.well-known/meerora.json"))
    }

    /// A `127.0.0.1:<port>` style domain that the policy lets the index fetch.
    fn local(&self, domain: &str) -> bool {
        self.allow_http_localhost && is_local_host(domain)
    }

    /// Verifies the organisation manifest `m`, posted as bytes whose SHA-256 is `sha256`.
    pub fn verify(&self, m: &Manifest, sha256: &str) -> Verification {
        if m.kind == ManifestKind::Person {
            return Verification::no("people are verified through their index account");
        }
        let domain = m.owner.domain.as_str();
        let foreign = self.foreign_urls(m);
        if !foreign.is_empty() {
            return Verification::no(format!("{} must be on {domain} or a name below it", foreign.join(", ")));
        }
        let Some(url) = self.listing_url(domain) else {
            return Verification::no(format!("{domain} cannot be verified"));
        };
        // The demo index and local test servers are this machine on purpose.
        let public = !(self.local(domain) || domain.ends_with(".example"));
        match fetch_listing(&url, public) {
            Ok(listed) => {
                let ids: Vec<&Listed> = listed.iter().filter(|l| l.id == m.id).collect();
                if ids.is_empty() {
                    Verification::no(format!("{url} does not list {}", m.id))
                } else if ids.iter().any(|l| l.sha256.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(sha256))) {
                    Verification { verified: true, note: format!("{url} lists {} with this manifest's sha256", m.id) }
                } else if ids.iter().all(|l| l.sha256.is_none()) {
                    Verification::no(format!("{url} lists {} without the sha256 of the manifest", m.id))
                } else {
                    Verification::no(format!("{url} lists {} with another sha256, this manifest has {sha256}", m.id))
                }
            }
            Err(e) => Verification::no(format!("{url}: {e}")),
        }
    }

    /// The fields whose URL is not on the owner's domain. Local test domains
    /// may point at any local server, demo domains at the demo servers.
    fn foreign_urls(&self, m: &Manifest) -> Vec<&'static str> {
        let domain = m.owner.domain.as_str();
        let urls = [
            ("endpoint.url", Some(&m.endpoint.url)),
            ("figure.url", Some(&m.figure.url)),
            ("picture", m.picture.as_ref()),
            ("a2a_card", m.a2a_card.as_ref()),
        ];
        let stands_in = self.local(domain) || (self.allow_http_localhost && self.demo_base.is_some() && domain.ends_with(".example"));
        urls.into_iter()
            .filter_map(|(what, url)| Some((what, url?)))
            .filter(|(_, url)| {
                let authority = url.split_once("://").map_or("", |(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""));
                let owned = url.starts_with("https://") && url_host(url).is_some_and(|host| is_within(host, domain));
                !(owned || (stands_in && is_local_host(authority)))
            })
            .map(|(what, _)| what)
            .collect()
    }
}

/// The entries of a well-known file. Redirects are not followed, the file has
/// to come from the domain itself. `public` fetches only from public addresses.
fn fetch_listing(url: &str, public: bool) -> Result<Vec<Listed>, String> {
    let config = Config::builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .https_only(public)
        .timeout_global(Some(FETCH_TIMEOUT))
        .build();
    let http = if public {
        ureq::Agent::with_parts(config, DefaultConnector::default(), PublicResolver::default())
    } else {
        ureq::Agent::new_with_config(config)
    };
    let response = http.get(url).call().map_err(|e| format!("cannot fetch it ({e})"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("answered {status}"));
    }
    let mut text = String::new();
    response.into_body().into_reader().take(MAX_LISTING).read_to_string(&mut text).map_err(|e| format!("cannot read it ({e})"))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("is not JSON ({e})"))?;
    let entries = value.get("manifests").and_then(|m| m.as_array()).ok_or("has no \"manifests\" list")?;
    Ok(entries
        .iter()
        .filter_map(|entry| match entry {
            serde_json::Value::String(id) => Some(Listed { id: id.clone(), sha256: None }),
            serde_json::Value::Object(o) => Some(Listed {
                id: o.get("id")?.as_str()?.to_string(),
                sha256: o.get("sha256").and_then(|h| h.as_str()).map(str::to_string),
            }),
            _ => None,
        })
        .collect())
}

/// How [`PublicResolver`] looks a `host:port` up.
type Lookup = fn(&str) -> io::Result<Vec<SocketAddr>>;

fn system_lookup(host_port: &str) -> io::Result<Vec<SocketAddr>> {
    Ok(host_port.to_socket_addrs()?.collect())
}

/// Resolves a host for the verifier and refuses it unless every address is
/// public. ureq then connects to exactly these addresses, so a second DNS
/// answer cannot point the request somewhere else.
#[derive(Clone)]
pub struct PublicResolver {
    lookup: Lookup,
}

impl Default for PublicResolver {
    fn default() -> Self {
        Self { lookup: system_lookup }
    }
}

impl fmt::Debug for PublicResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PublicResolver")
    }
}

impl PublicResolver {
    /// The public addresses of `host:port`, or why there are none.
    fn addresses(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        if !is_domain(host) {
            return Err(format!("{host} is not a public domain name"));
        }
        // The lookup runs on its own thread, so a slow name server cannot hold the request past the fetch timeout.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (lookup, host_port) = (self.lookup, format!("{host}:{port}"));
        std::thread::Builder::new()
            .name("verify-dns".into())
            .spawn(move || tx.send(lookup(&host_port)))
            .map_err(|e| format!("cannot resolve {host} ({e})"))?;
        let addrs = match rx.recv_timeout(FETCH_TIMEOUT) {
            Ok(result) => result.map_err(|e| format!("cannot resolve {host} ({e})"))?,
            Err(_) => return Err(format!("{host} did not resolve in time")),
        };
        if let Some(private) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
            return Err(format!("{host} resolves to {}, which is not a public address", private.ip()));
        }
        if addrs.is_empty() {
            return Err(format!("{host} has no address"));
        }
        Ok(addrs)
    }
}

impl Resolver for PublicResolver {
    fn resolve(&self, uri: &Uri, _config: &Config, _timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().ok_or(ureq::Error::HostNotFound)?;
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("http") { 80 } else { 443 });
        let addrs = self.addresses(host, port).map_err(|e| ureq::Error::Io(io::Error::new(io::ErrorKind::PermissionDenied, e)))?;
        let mut out = self.empty();
        for addr in addrs {
            if out.try_push(addr).is_err() {
                break;
            }
        }
        Ok(out)
    }
}

/// Whether `ip` is a global unicast address. Loopback, private, link-local,
/// shared (CGNAT, 100.64/10), unspecified, broadcast, multicast,
/// documentation, benchmarking and reserved ranges are not, nor are IPv6
/// addresses that carry an IPv4 address (mapped, compatible, NAT64, 6to4,
/// Teredo), unique local and site-local addresses.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..128).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let special = ip.is_unspecified()
        || ip.is_loopback()
        // ::/96 (IPv4-compatible) and ::ffff:0:0/96 (IPv4-mapped).
        || (s[0..5] == [0, 0, 0, 0, 0] && (s[5] == 0 || s[5] == 0xffff))
        // 64:ff9b::/96 and 64:ff9b:1::/48, NAT64.
        || (s[0] == 0x64 && s[1] == 0xff9b)
        // 100::/64, discard only.
        || (s[0] == 0x100 && s[1..4] == [0, 0, 0])
        // 2001::/32 Teredo, 2001:db8::/32 documentation, 2001:10::/28 and 2001:20::/28.
        || (s[0] == 0x2001 && (s[1] == 0 || s[1] == 0xdb8 || (0x10..0x30).contains(&s[1])))
        // 2002::/16, 6to4.
        || s[0] == 0x2002
        // fc00::/7 unique local, fe80::/10 link-local, fec0::/10 site-local, ff00::/8 multicast.
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        || (s[0] & 0xff00) == 0xff00;
    !special
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::http::{Request, Response, Server};
    use crate::validate::tests::manifest;

    /// A server whose well-known file is `body`, with `{id}` replaced by the
    /// id of the manifest made for it, and `{sha}` by the hash of `posted`.
    fn listing_server(body: &str, posted: &[u8]) -> (Server, Manifest) {
        let shared = Arc::new(Mutex::new(String::new()));
        let served = shared.clone();
        let server = Server::start(
            "127.0.0.1:0",
            Arc::new(move |req: Request| match req.path.as_str() {
                "/.well-known/meerora.json" => Response::bytes(200, "application/json", served.lock().unwrap().clone().into_bytes()),
                _ => Response::error(404, "not found"),
            }),
        )
        .unwrap();
        let m = local_manifest(&server.addr.to_string());
        *shared.lock().unwrap() = body.replace("{id}", &m.id).replace("{sha}", &crate::sha256_hex(posted));
        (server, m)
    }

    /// The test manifest moved to a local domain, URLs included.
    fn local_manifest(domain: &str) -> Manifest {
        let mut m = manifest();
        m.id = format!("{domain}/mira");
        m.owner.domain = domain.into();
        m.endpoint.url = format!("http://{domain}/a2a");
        m.figure.url = format!("http://{domain}/mira.js");
        m.picture = None;
        m.a2a_card = None;
        m
    }

    fn local() -> Verifier {
        Verifier { allow_http_localhost: true, demo_base: None }
    }

    #[test]
    fn a_listing_with_the_manifests_hash_verifies() {
        let (_server, m) = listing_server(r#"{"meerora": "0.1", "manifests": [{"id": "{id}", "sha256": "{sha}"}]}"#, b"posted bytes");
        let result = local().verify(&m, &crate::sha256_hex(b"posted bytes"));
        assert!(result.verified, "{result:?}");
        // Without the flag a localhost domain is not verified.
        assert!(!Verifier::default().verify(&m, &crate::sha256_hex(b"posted bytes")).verified);
    }

    #[test]
    fn a_copy_with_other_content_does_not_verify() {
        let (_server, m) = listing_server(r#"{"manifests": [{"id": "{id}", "sha256": "{sha}"}]}"#, b"the owner's manifest");
        let result = local().verify(&m, &crate::sha256_hex(b"a copy with another endpoint"));
        assert!(!result.verified && result.note.contains("another sha256"), "{result:?}");

        // Listing the id alone, as before, is not enough any more.
        let (_server, m) = listing_server(r#"{"manifests": ["{id}"]}"#, b"");
        let result = local().verify(&m, &crate::sha256_hex(b"anything"));
        assert!(!result.verified && result.note.contains("without the sha256"), "{result:?}");

        let (_server, m) = listing_server(r#"{"manifests": [{"id": "other.example/x", "sha256": "{sha}"}]}"#, b"x");
        let result = local().verify(&m, &crate::sha256_hex(b"x"));
        assert!(!result.verified && result.note.contains("does not list"), "{result:?}");
    }

    #[test]
    fn urls_of_a_verified_manifest_stay_on_the_owners_domain() {
        let (_server, mut m) = listing_server(r#"{"manifests": [{"id": "{id}", "sha256": "{sha}"}]}"#, b"m");
        m.endpoint.url = "https://evil.example/a2a".into();
        let result = local().verify(&m, &crate::sha256_hex(b"m"));
        assert!(!result.verified && result.note.contains("endpoint.url must be on"), "{result:?}");

        let v = Verifier::default();
        let mut m = manifest();
        assert!(v.foreign_urls(&m).is_empty(), "the brief's manifest keeps everything on meridian.example");
        m.figure.url = "https://cdn.meridian.example/mira.js".into();
        m.picture = Some("https://meridian.example.evil.org/mira.png".into());
        m.a2a_card = Some("https://evilmeridian.example/card.json".into());
        assert_eq!(v.foreign_urls(&m), ["picture", "a2a_card"]);
    }

    #[test]
    fn broken_listings_and_people_stay_unverified() {
        let (_server, m) = listing_server("not json", b"");
        let v = local().verify(&m, &crate::sha256_hex(b""));
        assert!(!v.verified && v.note.contains("not JSON"), "{v:?}");

        let mut person = manifest();
        person.kind = ManifestKind::Person;
        assert!(!local().verify(&person, "").verified);
    }

    #[test]
    fn listing_urls() {
        let v = Verifier { allow_http_localhost: false, demo_base: Some("http://127.0.0.1:7878".into()) };
        assert_eq!(v.listing_url("meridian.example").as_deref(), Some("http://127.0.0.1:7878/.well-known/meerora.json"));
        assert_eq!(v.listing_url("green.org").as_deref(), Some("https://green.org/.well-known/meerora.json"));
        assert_eq!(v.listing_url("127.0.0.1:9"), None);
        assert_eq!(v.listing_url("127.1"), None, "an IP address in short form");
        assert_eq!(v.listing_url("metadata.internal"), None);
        assert_eq!(Verifier::default().listing_url("meridian.example"), None, "only the demo answers for .example");
    }

    #[test]
    fn public_addresses_only() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "255.255.255.255"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["::1", "::", "::ffff:127.0.0.1", "::ffff:8.8.8.8", "fe80::1", "fd00::1", "64:ff9b::a00:1", "2002:7f00:1::1", "ff02::1"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "2a00:1450:4001::200e"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn a_domain_that_resolves_to_this_machine_is_never_fetched() {
        // A DNS answer an attacker controls: the name looks public, the address is not.
        let resolver = PublicResolver { lookup: |_| Ok(vec!["127.0.0.1:443".parse().unwrap(), "93.184.215.14:443".parse().unwrap()]) };
        let err = resolver.addresses("rebind.example.org", 443).unwrap_err();
        assert!(err.contains("127.0.0.1, which is not a public address"), "{err}");
        let resolver = PublicResolver { lookup: |_| Ok(vec!["[::ffff:10.0.0.1]:443".parse().unwrap()]) };
        assert!(resolver.addresses("mapped.example.org", 443).is_err());
        let resolver = PublicResolver { lookup: |_| Ok(vec!["93.184.215.14:443".parse().unwrap()]) };
        assert_eq!(resolver.addresses("public.example.org", 443).unwrap().len(), 1);
        assert!(resolver.addresses("127.0.0.1", 443).is_err(), "IP literals are refused before any lookup");

        // The whole fetch: the request never leaves, the resolver refuses it.
        let config = Config::builder().http_status_as_error(false).proxy(None).timeout_global(Some(FETCH_TIMEOUT)).build();
        let resolver = PublicResolver { lookup: |_| Ok(vec!["127.0.0.1:443".parse().unwrap()]) };
        let http = ureq::Agent::with_parts(config, DefaultConnector::default(), resolver);
        let err = http.get("https://rebind.example.org/.well-known/meerora.json").call().unwrap_err().to_string();
        assert!(err.contains("not a public address"), "{err}");
    }
}
