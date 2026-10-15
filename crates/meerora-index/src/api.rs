//! The index API, versioned under `/v1` (D25):
//!
//! | Request | Answer |
//! | --- | --- |
//! | `GET /v1/health` | `{"ok": true, "manifests": n, "version": "..."}` |
//! | `GET /v1/search?q=...` | `[IndexEntry]`, without `figure_script` |
//! | `GET /v1/manifests/{id}` | `IndexEntry` |
//! | `POST /v1/manifests` | registers or updates an organisation's manifest, answers with its `IndexEntry`, a `note` and once an `edit_secret` |
//!
//! `POST /v1/manifests` takes at most 16 KB. An organisation's manifest is
//! verified when `https://<domain>/.well-known/meerora.json` lists its id
//! with the SHA-256 of the exact posted bytes ([`crate::verify`]). The first
//! upload of an id that is not verified gets an `edit_secret` in the answer,
//! the only time it is shown. The index keeps only its SHA-256. Who stores:
//!
//! - A new id: any valid upload. Unverified uploads get the edit secret.
//! - A verified upload replaces an unverified record and drops its secret,
//!   the domain proves ownership. It replaces a verified record too.
//! - An unverified upload never replaces a verified record. It replaces an
//!   unverified record only with `Authorization: Bearer <edit secret>`.
//! - People never upload here. Their manifest comes from their account and
//!   profile (D26), so `kind: "person"` is refused.
//!
//! Creating accounts, uploading manifests and posting are limited per
//! client address ([`crate::rate`]).
//!
//! Accounts, profiles, follows, posts and the feed (D26, D27, see
//! [`crate::social_api`]). Routes marked with a key need `Authorization:
//! Bearer <token>`, the others read it when it is there:
//!
//! | Request | Answer |
//! | --- | --- |
//! | `POST /v1/accounts {"handle"}` | `{"handle", "token"}`, the only time the token is shown |
//! | `GET /v1/account` (key) | `{"handle", "profile"}` |
//! | `PUT /v1/profiles/{handle}` (key) | stores a section 5 profile without private fields and registers the person manifest |
//! | `GET /v1/profiles/{handle}` | the profile, friends fields only for friends |
//! | `PUT /v1/profiles/{handle}/figure` (key) | the personal agent's figure script |
//! | `GET /v1/profiles/{handle}/figure.js` | the figure script |
//! | `POST /v1/profiles/{handle}/photos` (key) | PNG or JPEG bytes, answers `{"id", "url"}` |
//! | `GET /v1/photos/{id}` | the photo, when the owner's profile lets the viewer see it |
//! | `GET /v1/profiles/{handle}/agent-card.json`, `POST .../a2a` | the person's agent, answering from the public profile (Q11) |
//! | `POST /v1/follows {"actor"}` (key) | follows a handle or a manifest id, answers with the relations |
//! | `DELETE /v1/follows/{actor}` (key) | ends a follow, or a friendship |
//! | `POST /v1/friends {"handle"}` (key) | asks to be friends, or accepts when they asked first |
//! | `POST /v1/friends/{handle}/accept`, `.../decline` (key) | decides a friend request |
//! | `GET /v1/relations` (key) | `Relations`: follows, followers, requests to you and by you |
//! | `POST /v1/posts {"text", "visibility"}` (key) | a public or friends post |
//! | `GET /v1/feed?limit=N&before=C` (key) | `{"posts", "next"}`, newest first |
//!
//! With `--demo` the index also serves `/.well-known/meerora.json` and the
//! demo figures and pictures under `/demo/`. Routing is one match on the
//! method and the path segments, one arm per route.

use std::sync::Arc;

use meerora_protocol::{Manifest, ManifestKind};
use serde_json::json;
use tracing::info;

use crate::demo;
use crate::http::{Handler, Request, Response};
use crate::rate::{Action, RateLimiter};
use crate::social::Social;
use crate::store::{Record, Store};
use crate::validate::{MAX_MANIFEST_BYTES, Policy, validate};
use crate::verify::Verifier;

pub struct Index {
    pub store: Store,
    pub verifier: Verifier,
    pub policy: Policy,
    /// The demo agent's URL with `--demo`. The index then serves the demo
    /// assets and the well-known file for the demo organisations.
    pub demo_agents: Option<String>,
    /// Accounts, profiles, follows and posts (D26, D27).
    pub social: Social,
    /// `http://host:port` or the public URL, for the URLs this index hands out.
    pub base: String,
    /// Requests per client and hour for accounts, uploads and posts.
    pub limiter: RateLimiter,
}

/// A stored upload.
#[derive(Debug)]
pub struct Added {
    pub created: bool,
    pub record: Record,
    /// The edit secret, when this upload got one. Shown once.
    pub edit_secret: Option<String>,
}

/// Why an upload was not stored.
#[derive(Debug)]
pub struct Refused {
    pub status: u16,
    pub error: String,
    pub problems: Vec<String>,
}

impl Refused {
    fn new(status: u16, error: impl Into<String>) -> Self {
        Self { status, error: error.into(), problems: Vec::new() }
    }
}

impl Index {
    pub fn handler(self: &Arc<Self>) -> Handler {
        let index = self.clone();
        Arc::new(move |request: Request| index.route(&request))
    }

    fn route(&self, req: &Request) -> Response {
        let segments = req.segments();
        match (req.method.as_str(), segments.as_slice()) {
            ("GET", ["v1", "health"]) => {
                Response::json(200, &serde_json::json!({ "ok": true, "manifests": self.store.len(), "version": env!("CARGO_PKG_VERSION") }))
            }
            ("GET", ["v1", "search"]) => {
                let query: String = req.query("q").unwrap_or("").chars().take(200).collect();
                Response::json(200, &self.entries(self.store.search(&query), self.viewer(req).as_deref()))
            }
            ("GET", ["v1", "manifests", id @ ..]) if !id.is_empty() => match self.store.get(&id.join("/")) {
                Some(record) => Response::json(200, &self.entries(vec![record], self.viewer(req).as_deref())[0]),
                None => Response::error(404, format!("no manifest {:?}", id.join("/"))),
            },
            ("POST", ["v1", "manifests"]) => self.register(req),
            ("POST", ["v1", "accounts"]) => self.create_account(req),
            ("GET", ["v1", "account"]) => self.account(req),
            ("PUT", ["v1", "profiles", handle]) => self.put_profile(req, handle),
            ("GET", ["v1", "profiles", handle]) => self.get_profile(req, handle),
            ("PUT", ["v1", "profiles", handle, "figure"]) => self.put_figure(req, handle),
            ("GET", ["v1", "profiles", handle, "figure.js"]) => self.get_figure(handle),
            ("POST", ["v1", "profiles", handle, "photos"]) => self.post_photo(req, handle),
            ("GET", ["v1", "profiles", handle, "agent-card.json"]) => self.person_card(handle),
            ("POST", ["v1", "profiles", handle, "a2a"]) => self.person_rpc(req, handle),
            ("GET", ["v1", "photos", id]) => self.get_photo(req, id),
            ("POST", ["v1", "follows"]) => self.follow(req),
            ("DELETE", ["v1", "follows", actor @ ..]) if !actor.is_empty() => self.unfollow(req, &actor.join("/")),
            ("POST", ["v1", "friends"]) => self.befriend(req),
            ("POST", ["v1", "friends", handle, "accept"]) => self.decide_friend(req, handle, true),
            ("POST", ["v1", "friends", handle, "decline"]) => self.decide_friend(req, handle, false),
            ("GET", ["v1", "relations"]) => self.relations(req),
            ("POST", ["v1", "posts"]) => self.create_post(req),
            ("GET", ["v1", "feed"]) => self.feed(req),
            ("GET", [".well-known", "meerora.json"]) if self.demo_agents.is_some() => {
                Response::json(200, &demo::well_known(&self.base, self.demo_agents.as_deref().unwrap_or_default()))
            }
            ("GET", ["demo", file]) if self.demo_agents.is_some() => demo_asset(file),
            (
                _,
                [
                    "v1",
                    "health" | "search" | "manifests" | "accounts" | "account" | "profiles" | "photos" | "follows" | "friends"
                    | "relations" | "posts" | "feed",
                    ..,
                ],
            ) => Response::error(405, format!("{} is not allowed here", req.method)),
            _ => Response::error(404, format!("nothing at {}", req.path)),
        }
    }

    /// `POST /v1/manifests`, see the module documentation for who may store what.
    fn register(&self, req: &Request) -> Response {
        if req.body.len() > MAX_MANIFEST_BYTES {
            return Response::error(413, format!("a manifest has at most {} KB", MAX_MANIFEST_BYTES / 1024));
        }
        if let Err(e) = self.limiter.check(req.peer.map(|p| p.ip()), Action::Register, crate::now_ms()) {
            return Response::error(429, e);
        }
        let manifest: Manifest = match serde_json::from_slice(&req.body) {
            Ok(m) => m,
            Err(e) => return Response::error(400, format!("the body is not a manifest ({e})")),
        };
        // People publish through their account and profile (D26), so nobody can claim a handle here.
        if manifest.kind == ManifestKind::Person {
            return Response::error(
                403,
                "people publish their agent through their account: POST /v1/accounts, then PUT /v1/profiles/{handle}",
            );
        }
        let secret = req.header("authorization").and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")));
        match self.add(manifest, &crate::sha256_hex(&req.body), secret.map(str::trim), true) {
            Ok(added) => {
                let mut body = serde_json::to_value(added.record.entry()).unwrap_or_default();
                body["note"] = json!(added.record.note);
                if let Some(secret) = added.edit_secret {
                    body["edit_secret"] = json!(secret);
                }
                Response::json(if added.created { 201 } else { 200 }, &body)
            }
            Err(r) => Response::json(r.status, &json!({ "error": r.error, "problems": r.problems })),
        }
    }

    /// Validates, verifies and stores an organisation's manifest that was
    /// posted as bytes with the SHA-256 `sha256`. `secret` is the edit secret
    /// the client sent, if any.
    pub fn add(&self, manifest: Manifest, sha256: &str, secret: Option<&str>, persist: bool) -> Result<Added, Refused> {
        if let Err(problems) = validate(&manifest, self.policy) {
            return Err(Refused { status: 422, error: format!("the manifest has {} problem(s)", problems.len()), problems });
        }
        if manifest.kind != ManifestKind::Organisation {
            return Err(Refused::new(403, "people publish their agent through their account"));
        }
        // The network fetch happens before the store is locked.
        let verification = self.verifier.verify(&manifest, sha256);
        let id = manifest.id.clone();
        let mut new_secret = None;
        let decide = |old: Option<&Record>| -> Result<Record, Refused> {
            let mut record = Record::new(manifest, verification.verified, verification.note.clone());
            record.sha256 = Some(sha256.to_string());
            match old {
                Some(old) if old.manifest.kind != ManifestKind::Organisation => {
                    return Err(Refused::new(409, format!("{id} is a person's agent")));
                }
                // The domain proves who owns the id, an old secret no longer counts.
                _ if verification.verified => {}
                Some(old) if old.verified => {
                    return Err(Refused::new(409, format!("{id} is verified and this upload is not ({})", verification.note)));
                }
                Some(old) => match (&old.edit_secret_sha256, secret) {
                    (Some(hash), Some(sent)) if *hash == crate::sha256_hex(sent.as_bytes()) => {
                        record.edit_secret_sha256 = Some(hash.clone());
                    }
                    (_, None) => {
                        return Err(Refused::new(
                            409,
                            format!(
                                "{id} is registered already. Send its edit secret as Authorization: Bearer <secret>, or verify the domain ({})",
                                verification.note
                            ),
                        ));
                    }
                    _ => return Err(Refused::new(403, format!("this is not the edit secret of {id}"))),
                },
                None => {
                    let secret = crate::social::new_token().map_err(|r| Refused::new(r.status, r.message))?;
                    record.edit_secret_sha256 = Some(crate::sha256_hex(secret.as_bytes()));
                    new_secret = Some(secret);
                }
            }
            Ok(record)
        };
        let (created, record) =
            self.store.upsert(&id, persist, decide, |e| Refused::new(500, format!("cannot store the manifest ({e})")))?;
        info!("{id} {} ({})", if record.verified { "verified" } else { "unverified" }, record.note);
        Ok(Added { created, record, edit_secret: new_secret })
    }
}

fn demo_asset(file: &str) -> Response {
    let (slug, ext) = file.rsplit_once('.').unwrap_or((file, ""));
    match (demo::agent(slug), ext) {
        (Some(a), "js") => Response::bytes(200, "text/javascript; charset=utf-8", a.figure.as_bytes().to_vec()),
        (Some(a), "png") => Response::bytes(200, "image/png", a.picture.to_vec()),
        _ => Response::error(404, format!("no demo file {file}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::http::Server;
    use crate::validate::tests::manifest;
    use meerora_protocol::IndexEntry;

    fn http() -> ureq::Agent {
        ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build())
    }

    fn index() -> Index {
        Index {
            store: Store::memory(),
            verifier: Verifier::default(),
            policy: Policy::default(),
            demo_agents: None,
            social: Social::memory(),
            base: "https://index.meerora.example".into(),
            limiter: RateLimiter::default(),
        }
    }

    fn post(base: &str, body: &[u8], secret: Option<&str>) -> (u16, serde_json::Value) {
        let mut request = http().post(&format!("{base}/v1/manifests")).header("Content-Type", "application/json");
        if let Some(s) = secret {
            request = request.header("Authorization", &format!("Bearer {s}"));
        }
        let mut r = request.send(body).unwrap();
        (r.status().as_u16(), r.body_mut().read_json().unwrap_or_default())
    }

    #[test]
    fn register_search_and_get() {
        let index = Arc::new(index());
        let server = Server::start("127.0.0.1:0", index.handler()).unwrap();
        let base = server.url();
        let http = http();
        let mira = serde_json::to_vec(&manifest()).unwrap();

        let (status, body) = post(&base, &mira, None);
        assert_eq!(status, 201);
        let entry: IndexEntry = serde_json::from_value(body.clone()).unwrap();
        assert!(!entry.verified, "meridian.example cannot be fetched");
        let secret = body["edit_secret"].as_str().expect("the first unverified upload gets an edit secret").to_string();
        // Without the secret nobody can change it, with it the owner can.
        let (status, body) = post(&base, &mira, None);
        assert_eq!(status, 409, "{body}");
        assert_eq!(post(&base, &mira, Some("not the secret")).0, 403);
        let (status, body) = post(&base, &mira, Some(&secret));
        assert_eq!(status, 200, "{body}");
        assert!(body.get("edit_secret").is_none(), "the secret is shown once");
        assert!(!index.store.get("meridian.example/mira").unwrap().edit_secret_sha256.unwrap().contains(&secret), "only its hash is kept");

        let mut broken = manifest();
        broken.search_terms = (0..6).map(|i| format!("t{i}")).collect();
        let (status, body) = post(&base, &serde_json::to_vec(&broken).unwrap(), None);
        assert_eq!(status, 422);
        assert!(body["problems"][0].as_str().unwrap().contains("6 search terms"));
        assert_eq!(post(&base, b"{\"id\": 1}", None).0, 400);

        let mut r = http.get(&format!("{base}/v1/search?q=data%20residency")).call().unwrap();
        let results: Vec<IndexEntry> = r.body_mut().read_json().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].manifest.id, "meridian.example/mira");
        assert!(results[0].figure_script.is_none());

        let mut r = http.get(&format!("{base}/v1/manifests/meridian.example/mira")).call().unwrap();
        assert_eq!(r.body_mut().read_json::<IndexEntry>().unwrap().manifest.name, "Mira");
        assert_eq!(http.get(&format!("{base}/v1/manifests/nobody/x")).call().unwrap().status().as_u16(), 404);
        assert_eq!(http.delete(&format!("{base}/v1/manifests/meridian.example/mira")).call().unwrap().status().as_u16(), 405);
        let mut r = http.get(&format!("{base}/v1/health")).call().unwrap();
        assert_eq!(r.body_mut().read_json::<serde_json::Value>().unwrap()["manifests"], 1);
        assert_eq!(http.get(&format!("{base}/demo/mira.js")).call().unwrap().status().as_u16(), 404, "no demo without --demo");
    }

    #[test]
    fn an_unverified_upload_cannot_replace_a_verified_manifest() {
        let index = index();
        let mut verified = Record::new(manifest(), true, "");
        index.store.put(verified.clone(), false).unwrap();
        let mut hijack = manifest();
        hijack.endpoint.url = "https://evil.example/a2a".into();
        let err = index.add(hijack, "00", None, false).unwrap_err();
        assert_eq!(err.status, 409);
        // An unverified record without an edit secret (stored before secrets existed) stays as it is too.
        verified.verified = false;
        index.store.put(verified, false).unwrap();
        assert_eq!(index.add(manifest(), "00", None, false).unwrap_err().status, 409);
        assert_eq!(index.add(manifest(), "00", Some("guess"), false).unwrap_err().status, 403);
    }

    /// A local "domain" whose well-known file lists what the test puts in.
    fn owner_domain() -> (Server, Arc<Mutex<String>>, Manifest) {
        let listing = Arc::new(Mutex::new(String::from("{\"manifests\": []}")));
        let served = listing.clone();
        let server = Server::start(
            "127.0.0.1:0",
            Arc::new(move |_req: Request| Response::bytes(200, "application/json", served.lock().unwrap().clone().into_bytes())),
        )
        .unwrap();
        let domain = server.addr.to_string();
        let mut m = manifest();
        m.id = format!("{domain}/mira");
        m.owner.domain = domain.clone();
        m.endpoint.url = format!("http://{domain}/a2a");
        m.figure.url = format!("http://{domain}/mira.js");
        m.picture = None;
        m.a2a_card = None;
        (server, listing, m)
    }

    fn list(listing: &Mutex<String>, id: &str, posted: &[u8]) {
        *listing.lock().unwrap() = json!({ "manifests": [{ "id": id, "sha256": crate::sha256_hex(posted) }] }).to_string();
    }

    #[test]
    fn a_copy_of_a_verified_manifest_with_another_endpoint_cannot_take_it_over() {
        let index = Arc::new(Index {
            verifier: Verifier { allow_http_localhost: true, demo_base: None },
            policy: Policy { allow_http_localhost: true },
            ..index()
        });
        let server = Server::start("127.0.0.1:0", index.handler()).unwrap();
        let (_owner, listing, mira) = owner_domain();
        let owners = serde_json::to_vec_pretty(&mira).unwrap();
        list(&listing, &mira.id, &owners);

        let (status, body) = post(&server.url(), &owners, None);
        assert_eq!((status, &body["verified"]), (201, &json!(true)), "{body}");
        assert!(body.get("edit_secret").is_none(), "a verified record needs no secret, its domain proves the owner");

        // The attack: the same manifest, verified domain and all, with the attacker's endpoint.
        let mut copy = mira.clone();
        copy.endpoint.url = "http://127.0.0.1:9/a2a".into();
        let (status, body) = post(&server.url(), &serde_json::to_vec(&copy).unwrap(), None);
        assert_eq!(status, 409, "{body}");
        assert_eq!(index.store.get(&mira.id).unwrap().manifest.endpoint.url, mira.endpoint.url, "the owner's endpoint stays");

        // The owner changes the manifest: list the new hash, then post.
        let mut update = mira.clone();
        update.welcome = "Hi again.".into();
        let bytes = serde_json::to_vec(&update).unwrap();
        list(&listing, &mira.id, &bytes);
        let (status, body) = post(&server.url(), &bytes, None);
        assert_eq!((status, &body["verified"]), (200, &json!(true)), "{body}");
        assert_eq!(index.store.get(&mira.id).unwrap().manifest.welcome, "Hi again.");
    }

    #[test]
    fn verifying_the_domain_takes_an_id_back_from_a_squatter() {
        let index = Index {
            verifier: Verifier { allow_http_localhost: true, demo_base: None },
            policy: Policy { allow_http_localhost: true },
            ..index()
        };
        let (_owner, listing, mira) = owner_domain();
        let mut squat = mira.clone();
        squat.endpoint.url = "http://127.0.0.1:9/a2a".into();
        let squatter = index.add(squat.clone(), "11", None, false).unwrap();
        let secret = squatter.edit_secret.unwrap();
        let bytes = serde_json::to_vec(&mira).unwrap();
        list(&listing, &mira.id, &bytes);
        let owner = index.add(mira.clone(), &crate::sha256_hex(&bytes), None, false).unwrap();
        assert!(owner.record.verified && owner.record.edit_secret_sha256.is_none());
        assert_eq!(index.add(squat, "11", Some(&secret), false).unwrap_err().status, 409, "the old secret is worthless now");
    }

    #[test]
    fn people_publish_only_through_their_account() {
        let index = index();
        let mut person = manifest();
        person.kind = ManifestKind::Person;
        person.owner.domain = "alex".into();
        person.id = "alex/alex".into();
        let request = Request {
            method: "POST".into(),
            path: "/v1/manifests".into(),
            body: serde_json::to_vec(&person).unwrap(),
            ..Request::default()
        };
        let response = index.route(&request);
        assert_eq!(response.status, 403);
        assert!(index.store.get("alex/alex").is_none(), "nobody can squat a handle");
        assert_eq!(index.add(person, "00", None, false).unwrap_err().status, 403);
    }

    #[test]
    fn uploads_are_small_and_limited_per_address() {
        let index = index();
        let peer = Some("203.0.113.5:4000".parse().unwrap());
        let big = Request { method: "POST".into(), path: "/v1/manifests".into(), body: vec![b' '; 17 * 1024], peer, ..Request::default() };
        assert_eq!(index.route(&big).status, 413);
        let mut tess = manifest();
        tess.id = "meridian.example/tess".into();
        let body = serde_json::to_vec(&tess).unwrap();
        let upload = Request { method: "POST".into(), path: "/v1/manifests".into(), body, peer, ..Request::default() };
        let statuses: Vec<u16> = (0..=Action::Register.per_hour()).map(|_| index.route(&upload).status).collect();
        assert_eq!(statuses[0], 201);
        assert_eq!(*statuses.last().unwrap(), 429, "{statuses:?}");
    }
}
