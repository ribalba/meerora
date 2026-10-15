//! The HTTP routes for accounts, profiles, follows, posts and the feed
//! (D26, D27). [`crate::api`] routes to these, [`crate::social`] keeps the data.
//!
//! Authenticated requests send `Authorization: Bearer <token>`. Without it,
//! or with a token nobody has, the answer is 401. A valid token for someone
//! else's profile gets 403.
//!
//! Publishing a profile also registers a person manifest for the handle, so
//! people show up in the same search as organisations (D25, D26). It counts
//! as verified, because only the account can publish it. Its endpoint is a
//! small A2A agent on this index that answers from the public profile, no
//! model behind it (Q11). Search results carry a person's profile only for
//! that account-published manifest.
//!
//! A profile has at most [`PROFILE_MAX`] bytes. Its photos must be photos of
//! the same account on this index (`<base>/v1/photos/<id>`), so a picture in
//! someone's feed never points anywhere else. Links must use http or https.

use meerora_protocol::{
    Actor, IndexEntry, Manifest, ManifestEndpoint, ManifestFigure, ManifestKind, ManifestOwner, Post, Profile, Relation, Relations,
    Visibility,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use crate::api::Index;
use crate::http::{Request, Response};
use crate::rate::Action;
use crate::social::{Note, PAGE_DEFAULT, Refusal, photo_id};
use crate::store::Record;
use crate::validate::{is_unsafe_char, validate};

/// The largest profile the index takes, in bytes as sent.
pub const PROFILE_MAX: usize = 64 * 1024;

fn refusal(r: Refusal) -> Response {
    Response::error(r.status, r.message)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAccount {
    handle: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActorRef {
    actor: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandleRef {
    handle: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPost {
    text: String,
    visibility: Visibility,
}

impl Index {
    /// The handle behind the request's bearer token, if any.
    pub fn viewer(&self, req: &Request) -> Option<String> {
        let header = req.header("authorization")?;
        let token = header.strip_prefix("Bearer ").or_else(|| header.strip_prefix("bearer "))?;
        self.social.authenticate(token)
    }

    /// The viewer, or the 401 answer.
    fn require(&self, req: &Request) -> Result<String, Response> {
        match req.header("authorization") {
            None => Err(Response::error(401, "sign in: send Authorization: Bearer <token>")),
            Some(_) => self.viewer(req).ok_or_else(|| Response::error(401, "this token belongs to no account")),
        }
    }

    /// `POST /v1/accounts {"handle"}`: the token is in this answer and nowhere else.
    pub fn create_account(&self, req: &Request) -> Response {
        if let Err(e) = self.limiter.check(req.peer.map(|p| p.ip()), Action::CreateAccount, crate::now_ms()) {
            return Response::error(429, e);
        }
        let new: NewAccount = match serde_json::from_slice(&req.body) {
            Ok(n) => n,
            Err(e) => return Response::error(400, format!("send {{\"handle\": \"...\"}} ({e})")),
        };
        if self.store.get(&person_id(&new.handle)).is_some_and(|r| r.verified) {
            return Response::error(409, format!("@{} is taken", new.handle));
        }
        match self.social.create_account(&new.handle, false) {
            Ok(token) => {
                info!("account @{} created", new.handle);
                Response::json(201, &json!({ "handle": new.handle, "token": token }))
            }
            Err(r) => refusal(r),
        }
    }

    /// `GET /v1/account`: who the token belongs to.
    pub fn account(&self, req: &Request) -> Response {
        match self.require(req) {
            Ok(handle) => Response::json(200, &json!({ "handle": handle, "profile": self.social.profile(&handle).is_some() })),
            Err(r) => r,
        }
    }

    /// `PUT /v1/profiles/{handle}` with a profile in the section 5 format.
    pub fn put_profile(&self, req: &Request, handle: &str) -> Response {
        let me = match self.require(req) {
            Ok(me) => me,
            Err(r) => return r,
        };
        if me != handle {
            return Response::error(403, format!("you are @{me}, so you cannot change @{handle}"));
        }
        if req.body.len() > PROFILE_MAX {
            return Response::error(413, format!("a profile has at most {} KB", PROFILE_MAX / 1024));
        }
        let profile: Profile = match serde_json::from_slice(&req.body) {
            Ok(p) => p,
            Err(e) => return Response::error(422, format!("the body is not a profile ({e})")),
        };
        let problems = self.profile_problems(handle, &profile);
        if !problems.is_empty() {
            return Response::json(422, &json!({ "error": problems.join(". "), "problems": problems }));
        }
        let stored = match self.social.put_profile(handle, &profile, false) {
            Ok(p) => p,
            Err(r) => return refusal(r),
        };
        let manifest = match self.register_person(handle) {
            Ok(entry) => entry,
            Err(e) => return Response::json(422, &json!({ "error": e, "profile": stored })),
        };
        info!("profile @{handle} published");
        Response::json(200, &json!({ "profile": stored, "manifest": manifest }))
    }

    /// What the index refuses in a profile besides [`Profile::problems`]:
    /// hidden characters, photos that are not this account's photos on this
    /// index, links that are not web pages.
    fn profile_problems(&self, handle: &str, p: &Profile) -> Vec<String> {
        let mut problems = Vec::new();
        let texts = p
            .name
            .iter()
            .chain(&p.headline)
            .chain(&p.places)
            .chain(&p.interests)
            .map(|t| t.value.as_str())
            .chain(p.education.iter().flat_map(|e| std::iter::once(e.school.as_str()).chain(e.degree.as_deref())))
            .chain(p.work.iter().flat_map(|w| std::iter::once(w.org.as_str()).chain(w.role.as_deref())));
        if texts.into_iter().any(|t| t.chars().any(is_unsafe_char)) {
            problems.push("the profile contains control or bidirectional characters".to_string());
        }
        for photo in &p.photos {
            let own = self.own_photo(&photo.url).is_some_and(|id| self.social.owns_photo(handle, &id));
            if !own {
                problems.push(format!("photo {:?} is not a photo of @{handle} on this index, upload it first", short(&photo.url)));
            }
        }
        for link in &p.links {
            let web = link.url.starts_with("https://") || link.url.starts_with("http://");
            if !web || link.url.chars().any(|c| c.is_whitespace() || is_unsafe_char(c)) {
                problems.push(format!("link {:?} is not an http or https URL", short(&link.url)));
            }
        }
        problems
    }

    /// The photo id of a URL of this index's photo endpoint.
    fn own_photo(&self, url: &str) -> Option<String> {
        let rest = url.strip_prefix(&self.base)?.strip_prefix("/v1/photos/")?;
        photo_id(&format!("/v1/photos/{rest}")).filter(|id| id == rest)
    }

    /// A picture this index may hand out for an actor: one of its own photos,
    /// or with `--demo` a demo picture.
    fn own_picture(&self, url: &str) -> bool {
        self.own_photo(url).is_some() || (self.demo_agents.is_some() && url.starts_with(&format!("{}/demo/", self.base)))
    }

    /// `GET /v1/profiles/{handle}`, filtered for the viewer.
    pub fn get_profile(&self, req: &Request, handle: &str) -> Response {
        match self.social.profile_for(handle, self.viewer(req).as_deref()) {
            Some(p) => Response::json(200, &p),
            None => Response::error(404, format!("@{handle} has no profile")),
        }
    }

    /// `PUT /v1/profiles/{handle}/figure` with the personal agent's figure script.
    pub fn put_figure(&self, req: &Request, handle: &str) -> Response {
        match self.require(req) {
            Ok(me) if me == handle => {}
            Ok(me) => return Response::error(403, format!("you are @{me}, so you cannot change @{handle}")),
            Err(r) => return r,
        }
        let Ok(script) = std::str::from_utf8(&req.body) else {
            return Response::error(400, "the figure is not UTF-8 text");
        };
        match self.social.put_figure(handle, script) {
            Ok(sha256) => {
                // A new figure changes the manifest, when there is one.
                if self.social.profile(handle).is_some()
                    && let Err(e) = self.register_person(handle)
                {
                    return Response::error(422, e);
                }
                Response::json(200, &json!({ "sha256": sha256, "url": self.figure_url(handle) }))
            }
            Err(r) => refusal(r),
        }
    }

    pub fn get_figure(&self, handle: &str) -> Response {
        match self.social.figure(handle) {
            Some(script) => Response::bytes(200, "text/javascript; charset=utf-8", script.into_bytes()),
            None => Response::error(404, format!("@{handle} has no figure")),
        }
    }

    /// `POST /v1/profiles/{handle}/photos` with PNG or JPEG bytes.
    pub fn post_photo(&self, req: &Request, handle: &str) -> Response {
        match self.require(req) {
            Ok(me) if me == handle => {}
            Ok(me) => return Response::error(403, format!("you are @{me}, so you cannot add photos to @{handle}")),
            Err(r) => return r,
        }
        match self.social.add_photo(handle, &req.body) {
            Ok(id) => Response::json(201, &json!({ "id": id, "url": format!("{}/v1/photos/{id}", self.base) })),
            Err(r) => refusal(r),
        }
    }

    /// `GET /v1/photos/{id}`, when the owner's profile lets the viewer see it.
    pub fn get_photo(&self, req: &Request, id: &str) -> Response {
        match self.social.photo(id, self.viewer(req).as_deref()) {
            Some((content_type, bytes)) => Response::bytes(200, &content_type, bytes),
            None => Response::error(404, format!("no photo {id} for you")),
        }
    }

    /// `POST /v1/follows {"actor"}`: a handle or a manifest id.
    pub fn follow(&self, req: &Request) -> Response {
        let me = match self.require(req) {
            Ok(me) => me,
            Err(r) => return r,
        };
        let target: ActorRef = match serde_json::from_slice(&req.body) {
            Ok(t) => t,
            Err(e) => return Response::error(400, format!("send {{\"actor\": \"...\"}} ({e})")),
        };
        let exists = self.actor_exists(&target.actor);
        match self.social.follow(&me, &target.actor, exists) {
            Ok(()) => self.relations_of(&me, 201),
            Err(r) => refusal(r),
        }
    }

    /// `DELETE /v1/follows/{actor}`.
    pub fn unfollow(&self, req: &Request, actor: &str) -> Response {
        let me = match self.require(req) {
            Ok(me) => me,
            Err(r) => return r,
        };
        match self.social.unfollow(&me, actor) {
            Ok(()) => self.relations_of(&me, 200),
            Err(r) => refusal(r),
        }
    }

    /// `POST /v1/friends {"handle"}`: a friend request, or the acceptance of theirs.
    pub fn befriend(&self, req: &Request) -> Response {
        let me = match self.require(req) {
            Ok(me) => me,
            Err(r) => return r,
        };
        let target: HandleRef = match serde_json::from_slice(&req.body) {
            Ok(t) => t,
            Err(e) => return Response::error(400, format!("send {{\"handle\": \"...\"}} ({e})")),
        };
        match self.social.befriend(&me, &target.handle) {
            Ok(()) => self.relations_of(&me, 201),
            Err(r) => refusal(r),
        }
    }

    /// `POST /v1/friends/{handle}/accept` and `.../decline`.
    pub fn decide_friend(&self, req: &Request, handle: &str, accept: bool) -> Response {
        let me = match self.require(req) {
            Ok(me) => me,
            Err(r) => return r,
        };
        let result = if accept { self.social.accept(&me, handle) } else { self.social.decline(&me, handle) };
        match result {
            Ok(()) => self.relations_of(&me, 200),
            Err(r) => refusal(r),
        }
    }

    /// `GET /v1/relations`: follows, followers and friend requests.
    pub fn relations(&self, req: &Request) -> Response {
        match self.require(req) {
            Ok(me) => self.relations_of(&me, 200),
            Err(r) => r,
        }
    }

    fn relations_of(&self, handle: &str, status: u16) -> Response {
        let (follows, followers, to_me, by_me) = self.social.relations(handle);
        let relations = Relations {
            follows: follows.iter().map(|f| Relation { actor: self.actor(&f.object), friend: f.friend, since: f.at }).collect(),
            followers: followers.iter().map(|f| Relation { actor: self.actor(&f.actor), friend: f.friend, since: f.at }).collect(),
            requests: to_me.iter().map(|r| Relation { actor: self.actor(&r.from), friend: true, since: r.at }).collect(),
            pending: by_me.iter().map(|r| Relation { actor: self.actor(&r.to), friend: true, since: r.at }).collect(),
        };
        Response::json(status, &relations)
    }

    /// `POST /v1/posts {"text", "visibility"}`.
    pub fn create_post(&self, req: &Request) -> Response {
        let me = match self.require(req) {
            Ok(me) => me,
            Err(r) => return r,
        };
        if let Err(e) = self.limiter.check(req.peer.map(|p| p.ip()), Action::Post, crate::now_ms()) {
            return Response::error(429, e);
        }
        let new: NewPost = match serde_json::from_slice(&req.body) {
            Ok(n) => n,
            Err(e) => return Response::error(400, format!("send {{\"text\": \"...\", \"visibility\": \"public\"}} ({e})")),
        };
        match self.social.post(&me, &new.text, new.visibility, false) {
            Ok(note) => {
                info!("@{me} posted {} ({:?})", note.id, note.visibility);
                Response::json(201, &self.post_json(&note))
            }
            Err(r) => refusal(r),
        }
    }

    /// `GET /v1/feed?limit=N&before=CURSOR`: `{"posts": [...], "next": cursor}`.
    pub fn feed(&self, req: &Request) -> Response {
        let me = match self.require(req) {
            Ok(me) => me,
            Err(r) => return r,
        };
        let limit = req.query("limit").and_then(|l| l.parse().ok()).unwrap_or(PAGE_DEFAULT);
        match self.social.feed(&me, req.query("before").filter(|b| !b.is_empty()), limit) {
            Ok((notes, next)) => {
                let posts: Vec<Post> = notes.iter().map(|n| self.post_json(n)).collect();
                Response::json(200, &json!({ "posts": posts, "next": next }))
            }
            Err(r) => refusal(r),
        }
    }

    fn post_json(&self, note: &Note) -> Post {
        Post {
            id: note.id.clone(),
            author: self.actor(&note.author),
            text: note.text.clone(),
            visibility: note.visibility,
            published: note.published,
        }
    }

    /// A handle with an account, or the manifest id of an organisation.
    fn actor_exists(&self, id: &str) -> bool {
        if id.contains('/') {
            self.store.get(id).is_some_and(|r| r.manifest.kind == ManifestKind::Organisation)
        } else {
            self.social.has_account(id)
        }
    }

    /// Name and picture of an actor, from the public profile or the manifest.
    pub fn actor(&self, id: &str) -> Actor {
        if id.contains('/') {
            let record = self.store.get(id);
            return Actor {
                id: id.into(),
                kind: ManifestKind::Organisation,
                name: record.as_ref().map_or_else(|| id.to_string(), |r| r.manifest.name.clone()),
                picture: record.as_ref().and_then(|r| r.manifest.picture.clone()),
                owner: record.map(|r| r.manifest.owner.name),
            };
        }
        let public = self.social.profile(id).map(|p| p.visible(Visibility::Public));
        Actor {
            id: id.into(),
            kind: ManifestKind::Person,
            name: public.as_ref().and_then(|p| p.name.as_ref().map(|n| n.value.clone())).unwrap_or_else(|| id.to_string()),
            picture: public
                .as_ref()
                .and_then(|p| p.photos.first().map(|ph| ph.url.clone()))
                .or_else(|| {
                    // Without a public photo, the picture of the manifest the account published.
                    self.store.get(&person_id(id)).filter(|r| r.verified).and_then(|r| r.manifest.picture)
                })
                .filter(|url| self.own_picture(url)),
            owner: None,
        }
    }

    /// Search results for `records`, each person with the viewer's view of
    /// the profile, but only on the manifest the person's account published.
    pub fn entries(&self, records: Vec<Record>, viewer: Option<&str>) -> Vec<IndexEntry> {
        records
            .into_iter()
            .map(|record| {
                let mut entry = record.entry();
                if record.manifest.kind == ManifestKind::Person && record.verified {
                    entry.profile = self.social.profile_for(&record.manifest.owner.domain, viewer);
                }
                entry
            })
            .collect()
    }

    fn figure_url(&self, handle: &str) -> String {
        format!("{}/v1/profiles/{handle}/figure.js", self.base)
    }

    /// Registers or updates the person manifest of `handle` from its public
    /// profile and figure. Returns the stored entry.
    pub fn register_person(&self, handle: &str) -> Result<IndexEntry, String> {
        let public = self.social.profile(handle).ok_or("publish a profile first")?.visible(Visibility::Public);
        let figure = self.social.figure(handle).ok_or("upload the personal agent's figure first")?;
        let manifest = person_manifest(&self.base, handle, &public, &figure);
        if let Err(problems) = validate(&manifest, self.policy) {
            return Err(format!("the manifest made from the profile has problems: {}", problems.join(". ")));
        }
        let record = Record::new(manifest, true, format!("published by the account @{handle}"));
        self.store.put(record.clone(), true).map_err(|e| format!("cannot store the manifest ({e})"))?;
        Ok(record.entry())
    }

    /// `GET /v1/profiles/{handle}/agent-card.json`, the card of the person's agent.
    pub fn person_card(&self, handle: &str) -> Response {
        let Some(profile) = self.social.profile(handle).map(|p| p.visible(Visibility::Public)) else {
            return Response::error(404, format!("@{handle} has no profile"));
        };
        let name = public_name(&profile, handle);
        Response::json(
            200,
            &json!({
                "name": name,
                "description": format!("The personal agent of {name}. It answers from the public profile."),
                "supportedInterfaces": [{ "url": format!("{}/v1/profiles/{handle}/a2a", self.base), "protocolBinding": "JSONRPC", "protocolVersion": "1.0" }],
                "version": "1.0.0",
                "capabilities": { "streaming": false, "pushNotifications": false },
                "defaultInputModes": ["text/plain"],
                "defaultOutputModes": ["text/plain"],
                "skills": [{ "id": "profile", "name": "Public profile", "description": format!("Tells what {name} shares publicly."), "tags": ["profile"] }],
            }),
        )
    }

    /// `POST /v1/profiles/{handle}/a2a`: answers every message with the
    /// public profile. Chatting with someone's agent for real is open (Q11).
    pub fn person_rpc(&self, req: &Request, handle: &str) -> Response {
        use crate::a2a::{agent_message, code, rpc_error, rpc_result};
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => return rpc_error(Value::Null, code::PARSE_ERROR, format!("the body is not JSON ({e})")),
        };
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let Some(profile) = self.social.profile(handle).map(|p| p.visible(Visibility::Public)) else {
            return rpc_error(id, code::INVALID_PARAMS, format!("@{handle} has no profile"));
        };
        match body.get("method").and_then(Value::as_str).unwrap_or("") {
            "SendMessage" => {
                let context = body
                    .pointer("/params/message/contextId")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("ctx-{handle}-{}", crate::now_ms()), str::to_string);
                rpc_result(id, json!({ "message": agent_message(&context, None, &profile_reply(&profile, handle)) }))
            }
            "SendStreamingMessage" => rpc_error(id, code::UNSUPPORTED_OPERATION, "this agent does not stream, use SendMessage"),
            other => rpc_error(id, code::METHOD_NOT_FOUND, format!("{other:?} is not a method of this agent")),
        }
    }
}

/// The manifest id of a person's agent: the handle, a slash and the handle
/// with `-` for `_`, which agent names do not allow.
pub fn person_id(handle: &str) -> String {
    format!("{handle}/{}", handle.replace('_', "-"))
}

/// The first characters of a value for a problem message.
fn short(s: &str) -> String {
    s.chars().take(80).collect()
}

/// The public name, or the handle.
fn public_name(profile: &Profile, handle: &str) -> String {
    profile.name.as_ref().map_or_else(|| handle.to_string(), |n| n.value.clone())
}

/// The canned answer of a person's agent, made from the public profile.
fn profile_reply(p: &Profile, handle: &str) -> String {
    let name = public_name(p, handle);
    let mut lines = vec![format!("I am the personal agent of {name}. I can only tell you what {name} shares publicly so far.")];
    if let Some(h) = &p.headline {
        lines.push(h.value.clone());
    }
    let work: Vec<String> = p.work.iter().map(|w| w.role.as_ref().map_or_else(|| w.org.clone(), |r| format!("{r} at {}", w.org))).collect();
    if !work.is_empty() {
        lines.push(format!("Work: {}.", work.join(", ")));
    }
    let schools: Vec<String> = p.education.iter().map(|e| e.school.clone()).collect();
    if !schools.is_empty() {
        lines.push(format!("Education: {}.", schools.join(", ")));
    }
    let interests: Vec<String> = p.interests.iter().map(|i| i.value.clone()).collect();
    if !interests.is_empty() {
        lines.push(format!("Interests: {}.", interests.join(", ")));
    }
    lines.join("\n\n")
}

/// The person manifest for a published profile (D25, D26). Only public
/// fields go into it, because the manifest is public.
pub fn person_manifest(base: &str, handle: &str, public: &Profile, figure: &str) -> Manifest {
    let name = public_name(public, handle);
    let short: String = name.chars().take(60).collect();
    let first = name.split_whitespace().next().unwrap_or(handle).to_string();
    let mut welcome = format!("Hi, I'm {first}'s agent.");
    if let Some(h) = &public.headline {
        welcome.push(' ');
        welcome.push_str(&h.value);
    }
    let welcome: String = welcome.chars().take(500).collect();
    let mut terms: Vec<String> = Vec::new();
    let candidates = std::iter::once(name.clone())
        .chain(public.work.iter().map(|w| w.org.clone()))
        .chain(public.places.iter().map(|p| p.value.clone()))
        .chain(public.interests.iter().map(|i| i.value.clone()))
        .chain(public.education.iter().map(|e| e.school.clone()));
    for term in candidates {
        let term: String = term.trim().chars().take(40).collect();
        if !term.is_empty() && !terms.iter().any(|t| t.eq_ignore_ascii_case(&term)) && terms.len() < crate::validate::MAX_TERMS {
            terms.push(term);
        }
    }
    Manifest {
        meerora_manifest: crate::validate::FORMAT.into(),
        id: person_id(handle),
        kind: ManifestKind::Person,
        name: short.clone(),
        owner: ManifestOwner { name: short, domain: handle.into() },
        picture: public.photos.first().map(|p| p.url.clone()),
        figure: ManifestFigure { url: format!("{base}/v1/profiles/{handle}/figure.js"), sha256: crate::sha256_hex(figure.as_bytes()) },
        welcome,
        search_terms: terms,
        endpoint: ManifestEndpoint { url: format!("{base}/v1/profiles/{handle}/a2a"), protocol: crate::validate::PROTOCOL.into() },
        a2a_card: Some(format!("{base}/v1/profiles/{handle}/agent-card.json")),
        privacy: Some(format!("{first}'s agent answers from the public profile. Nobody reads what you type here.")),
        languages: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use meerora_protocol::{IndexEntry, ProfilePhoto};
    use serde_json::json;

    use crate::Demo;
    use crate::social::tests::profile;
    use crate::store::Record;

    fn http() -> ureq::Agent {
        ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build())
    }

    /// (status, JSON body) of a request with an optional token.
    fn call(method: &str, url: &str, token: Option<&str>, body: Option<serde_json::Value>) -> (u16, serde_json::Value) {
        let http = http();
        let auth = token.map(|t| format!("Bearer {t}"));
        let mut response = match (method, body) {
            ("GET", _) => {
                let mut r = http.get(url);
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                r.call()
            }
            ("DELETE", _) => {
                let mut r = http.delete(url);
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                r.call()
            }
            (m, body) => {
                let mut r = if m == "PUT" { http.put(url) } else { http.post(url) };
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                r.send_json(body.unwrap_or(json!({})))
            }
        }
        .unwrap();
        let status = response.status().as_u16();
        (status, response.body_mut().read_json().unwrap_or(serde_json::Value::Null))
    }

    fn account(base: &str, handle: &str) -> String {
        let (status, body) = call("POST", &format!("{base}/v1/accounts"), None, Some(json!({ "handle": handle })));
        assert_eq!(status, 201, "{body}");
        body["token"].as_str().unwrap().to_string()
    }

    #[test]
    fn accounts_refuse_requests_without_a_valid_token() {
        let demo = Demo::start().unwrap();
        let base = demo.index.url();
        let token = account(&base, "robin");
        assert_eq!(call("POST", &format!("{base}/v1/accounts"), None, Some(json!({ "handle": "robin" }))).0, 409);
        assert_eq!(call("POST", &format!("{base}/v1/accounts"), None, Some(json!({ "handle": "alex" }))).0, 409, "the demo person");
        assert_eq!(call("GET", &format!("{base}/v1/account"), Some(&token), None).1["handle"], "robin");
        for (method, path) in [("GET", "/v1/account"), ("GET", "/v1/feed"), ("GET", "/v1/relations"), ("POST", "/v1/posts")] {
            assert_eq!(call(method, &format!("{base}{path}"), None, Some(json!({}))).0, 401, "{path} without a token");
            assert_eq!(call(method, &format!("{base}{path}"), Some("forged"), Some(json!({}))).0, 401, "{path} with a forged token");
        }
        let other = account(&base, "kim");
        let (status, _) =
            call("PUT", &format!("{base}/v1/profiles/robin"), Some(&other), Some(serde_json::to_value(profile("robin")).unwrap()));
        assert_eq!(status, 403, "kim cannot change robin's profile");
    }

    #[test]
    fn profile_photos_must_be_this_accounts_photos_on_this_index() {
        let demo = Demo::start().unwrap();
        let base = demo.index.url();
        let token = account(&base, "robin");
        let other = account(&base, "kim");
        let upload = |token: &str| {
            http()
                .post(&format!("{base}/v1/profiles/{}/photos", if token == other { "kim" } else { "robin" }))
                .header("Authorization", &format!("Bearer {token}"))
                .send(&include_bytes!("../demo/mira.png")[..])
                .unwrap()
                .body_mut()
                .read_json::<serde_json::Value>()
                .unwrap()["url"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let kims = upload(&other);
        let put = |url: &str| {
            let mut p = profile("robin");
            p.photos = vec![ProfilePhoto { url: url.into(), visibility: meerora_protocol::Visibility::Public }];
            call("PUT", &format!("{base}/v1/profiles/robin"), Some(&token), Some(serde_json::to_value(&p).unwrap()))
        };
        // Pictures end up in other people's feeds, so they must never point at an internal service or a local file.
        for url in [
            "http://169.254.169.254/latest/meta-data",
            "file:///etc/passwd",
            "https://tracker.example/pixel.png",
            &format!("{base}/v1/photos/../../etc"),
            &kims,
        ] {
            let (status, body) = put(url);
            assert_eq!(status, 422, "{url}: {body}");
            assert!(body["error"].as_str().unwrap().contains("not a photo of @robin"), "{body}");
        }
        let mut p = profile("robin");
        p.links =
            vec![meerora_protocol::ProfileLink { url: "javascript:alert(1)".into(), visibility: meerora_protocol::Visibility::Public }];
        p.headline =
            Some(meerora_protocol::ProfileText { value: "Builds \u{202E}boats".into(), visibility: meerora_protocol::Visibility::Public });
        let (status, body) = call("PUT", &format!("{base}/v1/profiles/robin"), Some(&token), Some(serde_json::to_value(&p).unwrap()));
        assert_eq!(status, 422);
        assert_eq!(body["problems"].as_array().unwrap().len(), 2, "{body}");
    }

    #[test]
    fn profiles_attach_only_to_the_manifest_the_account_published() {
        // Seed a squatted, unverified person manifest the way the old POST /v1/manifests allowed.
        let (_, index) = crate::start_index("127.0.0.1:0", None, &crate::IndexOptions::default()).unwrap();
        index.social.create_account("robin", false).unwrap();
        index.social.put_profile("robin", &profile("robin"), false).unwrap();
        let mut squat = crate::validate::tests::manifest();
        squat.kind = meerora_protocol::ManifestKind::Person;
        squat.id = "robin/robin".into();
        squat.owner.domain = "robin".into();
        squat.picture = Some("https://evil.example/robin.png".into());
        index.store.put(Record::new(squat, false, "posted before people were refused"), false).unwrap();
        let entries = index.entries(index.store.search("robin"), None);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].profile.is_none(), "the real profile must not decorate a squatted manifest");
        assert_eq!(index.actor("robin").picture, None, "an actor's picture is only this index's own photo");
    }

    #[test]
    fn accounts_are_limited_per_address() {
        let (_, index) = crate::start_index("127.0.0.1:0", None, &crate::IndexOptions::default()).unwrap();
        let peer = Some("203.0.113.9:5000".parse().unwrap());
        let statuses: Vec<u16> = (0..=crate::rate::Action::CreateAccount.per_hour())
            .map(|i| {
                let body = serde_json::to_vec(&json!({ "handle": format!("user{i}") })).unwrap();
                index.create_account(&crate::http::Request { body, peer, ..Default::default() }).status
            })
            .collect();
        assert!(statuses[..statuses.len() - 1].iter().all(|s| *s == 201), "{statuses:?}");
        assert_eq!(*statuses.last().unwrap(), 429);
    }

    #[test]
    fn publish_search_follow_befriend_and_read_the_feed() {
        let demo = Demo::start().unwrap();
        let base = demo.index.url();
        let token = account(&base, "robin");

        // A photo, the figure, then the profile.
        let photo = http()
            .post(&format!("{base}/v1/profiles/robin/photos"))
            .header("Authorization", &format!("Bearer {token}"))
            .send(&include_bytes!("../demo/tess.png")[..])
            .unwrap()
            .body_mut()
            .read_json::<serde_json::Value>()
            .unwrap();
        let photo_url = photo["url"].as_str().unwrap().to_string();
        let figure = crate::demo::AGENTS[2].figure;
        let r = http()
            .put(&format!("{base}/v1/profiles/robin/figure"))
            .header("Authorization", &format!("Bearer {token}"))
            .send(figure)
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let mut p = profile("robin");
        p.photos = vec![ProfilePhoto { url: photo_url.clone(), visibility: meerora_protocol::Visibility::Public }];
        let (status, body) = call("PUT", &format!("{base}/v1/profiles/robin"), Some(&token), Some(serde_json::to_value(&p).unwrap()));
        assert_eq!(status, 200, "{body}");
        assert!(!body.to_string().contains("Secret hobby"), "private fields never come back");
        assert_eq!(body["manifest"]["verified"], true);

        // People show up in the same search as organisations.
        let (_, results) = call("GET", &format!("{base}/v1/search?q=Robin%20Example"), None, None);
        let results: Vec<IndexEntry> = serde_json::from_value(results).unwrap();
        let robin = results.iter().find(|e| e.manifest.id == "robin/robin").expect("robin is found by name");
        assert!(robin.verified);
        assert_eq!(robin.manifest.picture.as_deref(), Some(photo_url.as_str()));
        assert_eq!(robin.profile.as_ref().unwrap().education.len(), 1, "anonymous searches see public fields");
        assert_eq!(http().get(&photo_url).call().unwrap().status().as_u16(), 200, "the public photo is served");
        let script = http().get(&robin.manifest.figure.url).call().unwrap().body_mut().read_to_string().unwrap();
        assert_eq!(crate::sha256_hex(script.as_bytes()), robin.manifest.figure.sha256);

        // Follow Mira and befriend Alex, who accepts at once in the demo.
        let (status, relations) =
            call("POST", &format!("{base}/v1/follows"), Some(&token), Some(json!({ "actor": "meridian.example/mira" })));
        assert_eq!(status, 201, "{relations}");
        assert_eq!(relations["follows"][0]["actor"]["name"], "Mira");
        assert_eq!(call("POST", &format!("{base}/v1/follows"), Some(&token), Some(json!({ "actor": "nobody.example/x" }))).0, 404);
        let (status, relations) = call("POST", &format!("{base}/v1/friends"), Some(&token), Some(json!({ "handle": "alex" })));
        assert_eq!(status, 201);
        let relations: meerora_protocol::Relations = serde_json::from_value(relations).unwrap();
        assert!(relations.is_friend("alex") && relations.follows("meridian.example/mira"));
        let (_, results) = call("GET", &format!("{base}/v1/search?q=alex"), Some(&token), None);
        let results: Vec<IndexEntry> = serde_json::from_value(results).unwrap();
        assert_eq!(results[0].profile.as_ref().unwrap().places.len(), 1, "a friend sees Alex's friends fields");

        // The feed: newest first, with Alex's friends post and Mira's posts, but not Tess.
        let (status, post) =
            call("POST", &format!("{base}/v1/posts"), Some(&token), Some(json!({ "text": "Hello from Robin", "visibility": "public" })));
        assert_eq!(status, 201, "{post}");
        let (_, page) = call("GET", &format!("{base}/v1/feed?limit=3"), Some(&token), None);
        let texts: Vec<&str> = page["posts"].as_array().unwrap().iter().map(|p| p["text"].as_str().unwrap()).collect();
        assert_eq!(texts[0], "Hello from Robin");
        assert!(texts[1].starts_with("Friends: dinner"), "{texts:?}");
        let next = page["next"].as_str().unwrap();
        let (_, older) = call("GET", &format!("{base}/v1/feed?limit=10&before={next}"), Some(&token), None);
        let older: Vec<&str> = older["posts"].as_array().unwrap().iter().map(|p| p["author"]["id"].as_str().unwrap()).collect();
        assert_eq!(older, ["alex", "meridian.example/mira"], "Tess is not followed");

        // The person's agent answers from the public profile.
        let r = http()
            .post(&robin.manifest.endpoint.url)
            .header("A2A-Version", "1.0")
            .send_json(json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": { "message": { "role": "ROLE_USER", "parts": [{ "text": "Who are you?" }] } } }))
            .unwrap()
            .body_mut()
            .read_json::<serde_json::Value>()
            .unwrap();
        let reply = r["result"]["message"]["parts"][0]["text"].as_str().unwrap();
        assert!(reply.contains("Robin Example") && reply.contains("Rowing") && !reply.contains("Example Bay"), "{reply}");
    }
}
