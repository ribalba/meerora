//! Personal agents and the feed in the daemon (D26, D27, D28).
//!
//! - The account: `<data dir>/index-account.json` with mode 0600 holds the
//!   handle and the token. Nothing else ever sees the token, neither
//!   `agent.toml`, `meerora.toml`, the app nor the agents.
//! - The profile: the personal agent's `config/profile.json` keeps the full
//!   profile, private fields included. Publishing uploads the photos and
//!   the figure and pushes the profile without its private fields.
//! - Post drafts: the personal agent writes them with `draft_post`. They wait
//!   in `<data dir>/social/state.json`, outside every agent directory, until
//!   the owner approves one. Only then does it reach the index (D26).
//! - The feed and the relations: follow, befriend and friend requests go to
//!   the index, and their answers go back to the app.
//! - The news digest and its reactions, see [`crate::news`].
//!
//! Requests to the index run on their own threads and report back through
//! [`Msg::Social`].

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail};
use meerora_protocol::{
    AgentRole, Command, Event, PROFILE_FORMAT, PROTOCOL_VERSION, Post, PostDraft, Profile, Reaction, Relations, RuntimeKind, SocialAction,
    ToolCall, Visibility, decode_event, encode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::daemon::{Daemon, Msg, Outgoing};
use crate::interview::{self, InterviewJob};
use crate::runs::{cut, now_ms};
use crate::{builder, news};

/// The account file in the data directory, readable only by the user.
pub const ACCOUNT_FILE: &str = "index-account.json";
/// Drafts, the digest and the last publish, outside every agent directory.
pub const STATE_FILE: &str = "social/state.json";
/// The full profile, relative to the personal agent's directory.
pub const PROFILE_FILE: &str = "config/profile.json";
/// The longest post, the same limit as the index.
pub const POST_MAX: usize = 1000;
/// Posts `read_feed` gives an agent by default and at most.
const TOOL_FEED: u32 = 15;
const TOOL_FEED_MAX: u32 = 50;
/// The largest photo the daemon uploads, the same limit as the index.
const PHOTO_MAX: u64 = 2 * 1024 * 1024;

/// Contents of `index-account.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountFile {
    /// The index this account belongs to.
    pub index: String,
    pub handle: String,
    pub token: String,
}

/// The account for `index`, if the data directory has one for it.
pub fn read_account(data_dir: &Path, index: &str) -> Option<AccountFile> {
    let text = std::fs::read_to_string(data_dir.join(ACCOUNT_FILE)).ok()?;
    let account: AccountFile = serde_json::from_str(&text).ok()?;
    (account.index.trim_end_matches('/') == index.trim_end_matches('/')).then_some(account)
}

/// Writes the account file with mode 0600, through a temporary file.
pub fn write_account(data_dir: &Path, account: &AccountFile) -> std::io::Result<()> {
    let path = data_dir.join(ACCOUNT_FILE);
    let tmp = data_dir.join(format!("{ACCOUNT_FILE}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    file.write_all(serde_json::to_string_pretty(account).map_err(std::io::Error::other)?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)
}

/// The token for searches and requests to `index`.
pub fn token_for(data_dir: &Path, index: &str) -> Option<String> {
    read_account(data_dir, index).map(|a| a.token)
}

/// What the daemon keeps in `social/state.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SocialState {
    #[serde(default)]
    pub drafts: Vec<PostDraft>,
    #[serde(default)]
    pub digest: Option<meerora_protocol::Digest>,
    /// Unix milliseconds of the last profile publish.
    #[serde(default)]
    pub published: Option<u64>,
    #[serde(default)]
    pub seq: u64,
}

pub struct Social {
    path: PathBuf,
    pub state: SocialState,
}

impl Social {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(STATE_FILE);
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!("{} is broken and will be rewritten ({e})", path.display());
                SocialState::default()
            }),
            Err(_) => SocialState::default(),
        };
        Self { path, state }
    }

    fn save(&self) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&self.state).map_err(std::io::Error::other)?)?;
            std::fs::rename(&tmp, &self.path)
        };
        if let Err(e) = write() {
            warn!("cannot write {} ({e})", self.path.display());
        }
    }
}

/// What a thread that talked to the index reports.
pub enum Done {
    Account {
        client: u64,
        result: Result<AccountFile, String>,
    },
    Published {
        result: Result<u64, String>,
    },
    Relations {
        client: u64,
        request: String,
        result: Result<Relations, String>,
        changed: bool,
    },
    /// `client` is `None` for a page pushed to every client.
    Feed {
        client: Option<u64>,
        request: String,
        result: Result<(Vec<Post>, Option<String>), String>,
    },
    Post {
        id: String,
        result: Result<Post, String>,
    },
    ToolFeed {
        client: u64,
        id: String,
        result: Result<String, String>,
    },
}

impl Daemon {
    fn account(&self) -> Option<AccountFile> {
        read_account(&self.opts.data_dir, &self.index_url())
    }

    /// The first agent with the role, as (id, directory).
    fn agent_with(&self, role: AgentRole) -> Option<(String, PathBuf)> {
        self.agents.values().find(|a| a.dir.config.role == Some(role)).map(|a| (a.dir.id.clone(), a.dir.path.clone()))
    }

    fn role_of(&self, agent: &str) -> Option<AgentRole> {
        self.agents.get(agent).and_then(|a| a.dir.config.role)
    }

    /// The personal agent's profile on this computer.
    fn own_profile(&self) -> Option<Profile> {
        let (_, dir) = self.agent_with(AgentRole::Personal)?;
        let text = std::fs::read_to_string(dir.join(PROFILE_FILE)).ok()?;
        serde_json::from_str(&text).map_err(|e| warn!("{} is broken ({e})", dir.join(PROFILE_FILE).display())).ok()
    }

    fn own_profile_event(&self, error: Option<String>) -> Event {
        Event::OwnProfile { profile: self.own_profile().map(Box::new), published: self.social.state.published, error }
    }

    fn drafts_event(&self, published: Option<Post>) -> Event {
        Event::PostDrafts { drafts: self.social.state.drafts.clone(), published }
    }

    /// The account, the own profile, the drafts and the digest.
    pub(crate) fn social_greeting(&self) -> Vec<Event> {
        vec![
            Event::Account { index: self.index_url(), handle: self.account().map(|a| a.handle), error: None },
            self.own_profile_event(None),
            self.drafts_event(None),
            Event::NewsDigest { digest: self.social.state.digest.clone() },
        ]
    }

    /// Runs `job` on its own thread and reports through [`Msg::Social`].
    fn spawn(&self, name: &str, job: impl FnOnce() -> Done + Send + 'static) {
        let tx = self.tx.clone();
        if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(move || {
            let _ = tx.send(Msg::Social(job()));
        }) {
            warn!("cannot start a thread for {name} ({e})");
        }
    }

    fn api(&self) -> Result<Api, String> {
        let account = self.account().ok_or("Create an account on the index first.")?;
        Ok(Api::new(&self.index_url(), Some(account.token)))
    }

    /// Fetches the first page of the feed and pushes it to every client.
    fn push_feed(&self) {
        let Ok(api) = self.api() else { return };
        self.spawn("feed", move || Done::Feed { client: None, request: String::new(), result: api.feed(None, 20) });
    }

    pub(crate) fn social_command(&mut self, client: u64, command: Command) {
        match command {
            Command::CreateAccount { handle } => self.create_account(client, handle),
            Command::Interview { session, agent, turns, profile } => self.interview(session, &agent, turns, profile),
            Command::SaveProfile { profile, publish } => self.save_profile(client, profile, publish),
            Command::Social { request, action } => {
                let api = match self.api() {
                    Ok(api) => api,
                    Err(e) => {
                        self.out.push(Outgoing::To(client, Event::Social { request, relations: None, error: Some(e) }));
                        return;
                    }
                };
                let changed = action != SocialAction::Refresh;
                info!("client {client} asks the index: {action:?}");
                self.spawn("social", move || Done::Relations { client, request, result: api.relations(&action), changed });
            }
            Command::ReadFeed { request, before } => match self.api() {
                Ok(api) => {
                    self.spawn("feed", move || Done::Feed { client: Some(client), request, result: api.feed(before.as_deref(), 20) })
                }
                Err(e) => self.out.push(Outgoing::To(client, Event::Feed { request, posts: Vec::new(), next: None, error: Some(e) })),
            },
            Command::DecidePost { id, approve, text, visibility } => self.decide_post(&id, approve, text, visibility),
            Command::React { digest, item, reaction } => self.react(&digest, &item, reaction),
            Command::AddTemplate { template } => self.add_template(client, template),
            other => warn!("{other:?} is not a social command"),
        }
    }

    fn create_account(&mut self, client: u64, handle: String) {
        let index = self.index_url();
        if let Some(existing) = self.account() {
            let error = format!("You have the account @{} on this index already.", existing.handle);
            self.out.push(Outgoing::To(client, Event::Account { index, handle: Some(existing.handle), error: Some(error) }));
            return;
        }
        let data_dir = self.opts.data_dir.clone();
        let handle = handle.trim().trim_start_matches('@').to_string();
        info!("client {client} creates the account @{handle} on {index}");
        self.spawn("account", move || {
            let result = Api::new(&index, None).create_account(&handle).and_then(|token| {
                let account = AccountFile { index: index.clone(), handle: handle.clone(), token };
                write_account(&data_dir, &account).map_err(|e| format!("cannot keep the token in {ACCOUNT_FILE} ({e})"))?;
                Ok(account)
            });
            Done::Account { client, result }
        });
    }

    fn interview(&mut self, session: String, agent: &str, turns: Vec<meerora_protocol::InterviewTurn>, profile: Option<Profile>) {
        // The personal agent's own model asks, unless it has none of its own.
        let config = self
            .agents
            .get(agent)
            .and_then(|a| a.dir.config.runtime.clone())
            .filter(|r| matches!(r.kind, RuntimeKind::ClaudeCli | RuntimeKind::CodexCli | RuntimeKind::Ollama | RuntimeKind::Api))
            .or_else(|| self.opts.settings.builder.clone())
            .unwrap_or_else(builder::default_runtime);
        let job = InterviewJob { session, turns, profile, handle: self.account().map(|a| a.handle).unwrap_or_default() };
        info!("interview {} asks question {}", job.session, job.turns.len() + 1);
        let (settings, factory, tx) = (self.opts.settings.clone(), self.opts.factory.clone(), self.tx.clone());
        let spawned = std::thread::Builder::new().name("interview".into()).spawn(move || {
            interview::run(&job, &config, &settings, &factory, &mut |event| {
                let _ = tx.send(Msg::Draft(event));
            });
        });
        if let Err(e) = spawned {
            warn!("cannot start an interview thread ({e})");
        }
    }

    fn save_profile(&mut self, client: u64, mut profile: Profile, publish: bool) {
        let Some((_, dir)) = self.agent_with(AgentRole::Personal) else {
            let error = "Set up your personal agent first.".to_string();
            self.out
                .push(Outgoing::To(client, Event::OwnProfile { profile: Some(Box::new(profile)), published: None, error: Some(error) }));
            return;
        };
        let account = self.account();
        profile.meerora_profile = PROFILE_FORMAT.into();
        if let Some(a) = &account {
            profile.handle = a.handle.clone();
        }
        let problems = profile.problems();
        if !problems.is_empty() {
            let error = format!("The profile has problems: {}.", problems.join(". "));
            let published = self.social.state.published;
            self.out.push(Outgoing::To(client, Event::OwnProfile { profile: Some(Box::new(profile)), published, error: Some(error) }));
            return;
        }
        let path = dir.join(PROFILE_FILE);
        // The personal agent writes its directory, so config/ is opened without following symlinks (safefs).
        let text = serde_json::to_string_pretty(&profile).unwrap_or_default() + "\n";
        let written = meerora_protocol::safefs::replace(&dir, PROFILE_FILE, text.as_bytes());
        if let Err(e) = written {
            let error = format!("cannot write {} ({e})", path.display());
            self.out
                .push(Outgoing::To(client, Event::OwnProfile { profile: Some(Box::new(profile)), published: None, error: Some(error) }));
            return;
        }
        self.out.push(Outgoing::All(self.own_profile_event(None)));
        if !publish {
            return;
        }
        let Some(account) = account else {
            self.out.push(Outgoing::All(self.own_profile_event(Some("Create an account on the index before you publish.".into()))));
            return;
        };
        let api = Api::new(&self.index_url(), Some(account.token.clone()));
        let figure = self.agents.values().find(|a| a.dir.path == dir).map(|a| a.dir.figure_path());
        info!("publishing the profile of @{}", account.handle);
        self.spawn("publish", move || {
            let result = (|| {
                let script =
                    figure.and_then(|f| std::fs::read_to_string(f).ok()).ok_or("The personal agent has no figure script to publish.")?;
                api.publish(&account.handle, &profile, &dir, &script)?;
                Ok(now_ms())
            })();
            Done::Published { result }
        });
    }

    fn decide_post(&mut self, id: &str, approve: bool, text: Option<String>, visibility: Option<Visibility>) {
        let Some(i) = self.social.state.drafts.iter().position(|d| d.id == id) else {
            warn!("no post draft {id}");
            return;
        };
        if !approve {
            let draft = self.social.state.drafts.remove(i);
            info!("post draft {} of {} discarded", draft.id, draft.agent);
            self.social.save();
            self.out.push(Outgoing::All(self.drafts_event(None)));
            return;
        }
        let draft = &mut self.social.state.drafts[i];
        let text = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| draft.text.clone());
        let visibility = visibility.unwrap_or(draft.visibility);
        draft.text = text.clone();
        draft.visibility = visibility;
        let error = check_post(&text, visibility).err().or_else(|| self.api().err());
        if let Some(e) = error {
            self.social.state.drafts[i].error = Some(e);
            self.social.save();
            self.out.push(Outgoing::All(self.drafts_event(None)));
            return;
        }
        let api = self.api().expect("checked above");
        let id = id.to_string();
        info!("post draft {id} approved, it goes out");
        self.spawn("post", move || Done::Post { id, result: api.post(&text, visibility) });
    }

    fn react(&mut self, digest: &str, item: &str, reaction: Reaction) {
        let Some(current) = self.social.state.digest.as_mut().filter(|d| d.id == digest) else {
            warn!("a reaction to digest {digest}, which is not the current one");
            return;
        };
        let agent = current.agent.clone();
        let Some(dir) = self.agents.get(&agent).map(|a| a.dir.path.clone()) else {
            warn!("the news agent {agent} is gone, so the reaction is not kept");
            return;
        };
        if let Err(e) = news::react(current, &dir, item, reaction) {
            warn!("{e}");
            return;
        }
        self.social.save();
        self.out.push(Outgoing::All(Event::NewsDigest { digest: self.social.state.digest.clone() }));
        // The user saw the digest, so the newspaper goes down.
        self.set_param(&agent, "fresh", Value::Bool(false));
    }

    /// Sets a figure param of an agent when its figure declares it (D24).
    fn set_param(&mut self, agent: &str, name: &str, value: Value) {
        let Some(path) = self.agents.get(agent).map(|a| a.dir.figure_path()) else { return };
        if self.figures.check(&path, name, &value).is_ok() && self.engine.agent(agent).is_none_or(|s| s.params.get(name) != Some(&value)) {
            self.emit(Event::Param { agent: agent.into(), name: name.into(), value });
            self.persist(agent);
        }
    }

    fn add_template(&mut self, client: u64, template: String) {
        let result = match template.as_str() {
            "news" => news::add(&self.opts.data_dir, &self.opts.stations, &self.opts.settings),
            other => Err(format!("there is no template {other:?}, only news")),
        };
        match result {
            Ok(agent) => {
                info!("the {template} template moved into agents/{agent}");
                self.out.push(Outgoing::To(client, Event::TemplateAdded { template, agent: Some(agent), error: None }));
                // The same path as Command::ReloadAgents.
                self.handle_command(client, Command::ReloadAgents);
            }
            Err(e) => self.out.push(Outgoing::To(client, Event::TemplateAdded { template, agent: None, error: Some(e) })),
        }
    }

    /// `draft_post`, `read_feed` and `post_digest` from an agent's bridge.
    pub(crate) fn social_tool(&mut self, client: u64, id: String, agent: &str, call: ToolCall) {
        let role = self.role_of(agent);
        let answer = |ok: bool, message: String| Outgoing::To(client, Event::ToolResult { id: id.clone(), ok, message, image: None });
        match call {
            ToolCall::DraftPost { text, visibility } => {
                if role != Some(AgentRole::Personal) {
                    self.out.push(answer(false, "Only the personal agent drafts posts.".into()));
                    return;
                }
                let text = text.trim().to_string();
                if let Err(e) = check_post(&text, visibility) {
                    self.out.push(answer(false, e));
                    return;
                }
                self.social.state.seq += 1;
                let draft = PostDraft {
                    id: format!("draft-{}-{}", now_ms(), self.social.state.seq),
                    agent: agent.into(),
                    text: text.clone(),
                    visibility,
                    created: now_ms(),
                    error: None,
                };
                info!("{agent} drafted post {}", draft.id);
                self.social.state.drafts.push(draft);
                self.social.save();
                self.out.push(Outgoing::All(self.drafts_event(None)));
                self.post_system(agent, format!("A post waits for your approval in the feed: \"{}\"", cut(&text, 90)));
                self.out.push(answer(
                    true,
                    "The draft waits for your owner's approval in the feed. Nothing is published until they approve it.".into(),
                ));
            }
            ToolCall::ReadFeed { limit } => {
                if !matches!(role, Some(AgentRole::Personal | AgentRole::News)) {
                    self.out.push(answer(false, "Only the personal agent and the news agent read the feed.".into()));
                    return;
                }
                let api = match self.api() {
                    Ok(api) => api,
                    Err(e) => {
                        self.out.push(answer(false, format!("The feed cannot be read. {e}")));
                        return;
                    }
                };
                let handle = self.account().map(|a| a.handle).unwrap_or_default();
                let limit = limit.unwrap_or(TOOL_FEED).clamp(1, TOOL_FEED_MAX);
                self.spawn("read-feed", move || Done::ToolFeed {
                    client,
                    id,
                    result: api.feed(None, limit as usize).map(|(posts, _)| feed_text(&posts, &handle)),
                });
            }
            ToolCall::PostDigest { title, items } => {
                if role != Some(AgentRole::News) {
                    self.out.push(answer(false, "Only the news agent posts digests.".into()));
                    return;
                }
                let n = items.len();
                match news::digest(agent, &title, items) {
                    Ok(digest) => {
                        // Items of the last digest that nobody touched count as skipped (D28).
                        if let Some(old) = self.social.state.digest.take()
                            && let Some(dir) = self.agents.get(&old.agent).map(|a| a.dir.path.clone())
                            && let Err(e) = news::append(&dir, &news::skipped(&old))
                        {
                            warn!("{e}");
                        }
                        info!("{agent} posted the digest {} with {n} items", digest.id);
                        self.social.state.digest = Some(digest);
                        self.social.save();
                        self.out.push(Outgoing::All(Event::NewsDigest { digest: self.social.state.digest.clone() }));
                        self.set_param(agent, "fresh", Value::Bool(true));
                        self.out.push(answer(true, format!("The digest with {n} items is pinned at the top of the feed.")));
                    }
                    Err(e) => self.out.push(answer(false, format!("The digest was not stored: {e}."))),
                }
            }
            other => self.out.push(answer(false, format!("{other:?} is not a social tool"))),
        }
    }

    pub(crate) fn on_social(&mut self, done: Done) {
        match done {
            Done::Account { client, result } => match result {
                Ok(account) => {
                    info!("account @{} on {} is ready", account.handle, account.index);
                    self.out.push(Outgoing::All(Event::Account {
                        index: account.index,
                        handle: Some(account.handle.clone()),
                        error: None,
                    }));
                    // The local profile takes the handle.
                    if let Some(mut profile) = self.own_profile()
                        && profile.handle != account.handle
                        && let Some((_, dir)) = self.agent_with(AgentRole::Personal)
                    {
                        profile.handle = account.handle;
                        let text = serde_json::to_string_pretty(&profile).unwrap_or_default() + "\n";
                        let _ = meerora_protocol::safefs::replace(&dir, PROFILE_FILE, text.as_bytes());
                        self.out.push(Outgoing::All(self.own_profile_event(None)));
                    }
                }
                Err(e) => {
                    warn!("the account was not created ({e})");
                    self.out.push(Outgoing::To(client, Event::Account { index: self.index_url(), handle: None, error: Some(e) }));
                }
            },
            Done::Published { result } => match result {
                Ok(at) => {
                    info!("the profile is published");
                    self.social.state.published = Some(at);
                    self.social.save();
                    self.out.push(Outgoing::All(self.own_profile_event(None)));
                }
                Err(e) => {
                    warn!("publishing the profile failed ({e})");
                    self.out.push(Outgoing::All(self.own_profile_event(Some(e))));
                }
            },
            Done::Relations { client, request, result, changed } => {
                let ok = result.is_ok();
                let (relations, error) = split(result);
                self.out.push(Outgoing::To(client, Event::Social { request, relations, error }));
                if ok && changed {
                    self.push_feed();
                }
            }
            Done::Feed { client, request, result } => {
                let ((posts, next), error) = match result {
                    Ok(page) => (page, None),
                    Err(e) => ((Vec::new(), None), Some(e)),
                };
                let event = Event::Feed { request, posts, next, error };
                self.out.push(match client {
                    Some(c) => Outgoing::To(c, event),
                    None => Outgoing::All(event),
                });
            }
            Done::Post { id, result } => match result {
                Ok(post) => {
                    info!("post draft {id} went out as {}", post.id);
                    self.social.state.drafts.retain(|d| d.id != id);
                    self.social.save();
                    self.out.push(Outgoing::All(self.drafts_event(Some(post))));
                    self.push_feed();
                }
                Err(e) => {
                    warn!("post draft {id} did not go out ({e})");
                    if let Some(d) = self.social.state.drafts.iter_mut().find(|d| d.id == id) {
                        d.error = Some(e);
                    }
                    self.social.save();
                    self.out.push(Outgoing::All(self.drafts_event(None)));
                }
            },
            Done::ToolFeed { client, id, result } => {
                let (ok, message) = match result {
                    Ok(text) => (true, text),
                    Err(e) => (false, format!("The feed cannot be read. {e}")),
                };
                self.out.push(Outgoing::To(client, Event::ToolResult { id, ok, message, image: None }));
            }
        }
    }
}

fn split<T>(result: Result<T, String>) -> (Option<T>, Option<String>) {
    match result {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    }
}

/// A post the index will take.
pub fn check_post(text: &str, visibility: Visibility) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("The post is empty.".into());
    }
    if text.chars().count() > POST_MAX {
        return Err(format!("A post has at most {POST_MAX} characters, this one has {}.", text.chars().count()));
    }
    if visibility == Visibility::Private {
        return Err("A post is public or friends. A private post would reach nobody.".into());
    }
    Ok(())
}

/// The feed as text for an agent, newest first.
pub fn feed_text(posts: &[Post], handle: &str) -> String {
    if posts.is_empty() {
        return "The feed is empty. Your owner follows nobody yet, or nobody posted.".into();
    }
    let mut text = format!("The newest {} posts of the feed, newest first:\n", posts.len());
    for p in posts {
        let when = jiff::Timestamp::from_millisecond(p.published as i64)
            .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let who = match (&p.author.owner, p.author.id == handle) {
            (_, true) => format!("{} (your owner)", p.author.name),
            (Some(owner), _) => format!("{} of {owner}", p.author.name),
            (None, _) => p.author.name.clone(),
        };
        let visibility = if p.visibility == Visibility::Friends { "friends only" } else { "public" };
        text.push_str(&format!("\n- {when} · {who} · {visibility}\n  {}\n", p.text.replace('\n', "\n  ")));
    }
    text
}

/// The extra part of the system prompt for the personal and the news agent.
pub fn prompt_addition(role: Option<AgentRole>) -> Option<&'static str> {
    match role? {
        AgentRole::Personal => Some(
            "You are also your owner's personal agent: you represent them in the meerora index, where others find their \
profile, follow them and befriend them. Their full profile is in config/profile.json. You have two more tools:
- draft_post drafts a post about your owner. Every draft waits for your owner's approval in their feed, and nothing is \
published until they approve it. Use public for things anyone may read and friends for personal news. Never use anything the \
profile marks private.
- read_feed reads the newest posts of the people and organisations your owner follows.",
        ),
        AgentRole::News => Some(
            "You are also the news agent. You have two more tools:
- read_feed reads the newest posts of the people and organisations the user follows.
- post_digest stores your digest on this computer and pins it at the top of the user's feed. Call it once per digest.
The user's reactions to earlier digests are in memory/reactions.jsonl, one JSON object per line.",
        ),
        // The helper's part is in helper.rs.
        AgentRole::Helper => None,
    }
}

/// A small client for the index API.
pub struct Api {
    base: String,
    token: Option<String>,
    http: ureq::Agent,
}

impl Api {
    pub fn new(base: &str, token: Option<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(4)))
            .timeout_global(Some(Duration::from_secs(20)))
            .build();
        Self { base: base.trim_end_matches('/').to_string(), token, http: ureq::Agent::new_with_config(config) }
    }

    fn auth(&self) -> Option<String> {
        self.token.as_ref().map(|t| format!("Bearer {t}"))
    }

    /// Sends a request and returns the JSON answer of a 2xx status.
    fn send(&self, method: &str, path: &str, body: Option<(Vec<u8>, &str)>) -> Result<Value, String> {
        let url = format!("{}{path}", self.base);
        let auth = self.auth();
        let result = match (method, body) {
            ("GET" | "DELETE", _) => {
                let mut r = if method == "GET" { self.http.get(&url) } else { self.http.delete(&url) };
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                r.call()
            }
            (_, body) => {
                let mut r = if method == "PUT" { self.http.put(&url) } else { self.http.post(&url) };
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                let (bytes, content_type) = body.unwrap_or_else(|| (b"{}".to_vec(), "application/json"));
                r.content_type(content_type).send(&bytes[..])
            }
        };
        let response = result.map_err(|e| match e {
            ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => self.unreachable(),
            ureq::Error::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => self.unreachable(),
            ureq::Error::Timeout(_) => format!("The index at {} did not answer in time.", self.base),
            other => format!("The request to {url} failed ({other})."),
        })?;
        let status = response.status().as_u16();
        let mut text = String::new();
        response
            .into_body()
            .into_reader()
            .take(4 * 1024 * 1024)
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read {url} ({e})"))?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(value);
        }
        let message = value.get("error").and_then(Value::as_str).map_or_else(|| text.chars().take(160).collect(), str::to_string);
        Err(format!("The index answered {status}: {message}"))
    }

    fn unreachable(&self) -> String {
        format!(
            "The meerora index at {} does not answer. Start one with meerora-index serve, or set [index] url in meerora.toml.",
            self.base
        )
    }

    fn json(&self, method: &str, path: &str, body: Value) -> Result<Value, String> {
        self.send(method, path, Some((body.to_string().into_bytes(), "application/json")))
    }

    pub fn create_account(&self, handle: &str) -> Result<String, String> {
        let answer = self.json("POST", "/v1/accounts", json!({ "handle": handle }))?;
        answer.get("token").and_then(Value::as_str).map(str::to_string).ok_or_else(|| "The index sent no token.".into())
    }

    pub fn relations(&self, action: &SocialAction) -> Result<Relations, String> {
        let answer = match action {
            SocialAction::Refresh => self.send("GET", "/v1/relations", None)?,
            SocialAction::Follow { actor } => self.json("POST", "/v1/follows", json!({ "actor": actor }))?,
            SocialAction::Unfollow { actor } => self.send("DELETE", &format!("/v1/follows/{}", path_part(actor)), None)?,
            SocialAction::Befriend { handle } => self.json("POST", "/v1/friends", json!({ "handle": handle }))?,
            SocialAction::Accept { handle } => self.send("POST", &format!("/v1/friends/{}/accept", path_part(handle)), None)?,
            SocialAction::Decline { handle } => self.send("POST", &format!("/v1/friends/{}/decline", path_part(handle)), None)?,
        };
        serde_json::from_value(answer).map_err(|e| format!("The index sent relations meerora cannot read ({e})."))
    }

    pub fn feed(&self, before: Option<&str>, limit: usize) -> Result<(Vec<Post>, Option<String>), String> {
        let mut path = format!("/v1/feed?limit={limit}");
        if let Some(before) = before {
            path.push_str(&format!("&before={}", path_part(before)));
        }
        let answer = self.send("GET", &path, None)?;
        let posts: Vec<Post> = serde_json::from_value(answer.get("posts").cloned().unwrap_or(Value::Array(Vec::new())))
            .map_err(|e| format!("The index sent a feed meerora cannot read ({e})."))?;
        Ok((posts, answer.get("next").and_then(Value::as_str).map(str::to_string)))
    }

    pub fn post(&self, text: &str, visibility: Visibility) -> Result<Post, String> {
        let answer = self.json("POST", "/v1/posts", json!({ "text": text, "visibility": visibility }))?;
        serde_json::from_value(answer).map_err(|e| format!("The index sent a post meerora cannot read ({e})."))
    }

    /// Uploads the photos and the figure, then the profile without its
    /// private fields. Photos given as paths are read from `agent_dir`.
    pub fn publish(&self, handle: &str, profile: &Profile, agent_dir: &Path, figure: &str) -> Result<(), String> {
        let mut public = profile.visible(Visibility::Friends);
        public.handle = handle.into();
        for photo in &mut public.photos {
            if photo.url.starts_with("https://") || photo.url.starts_with("http://") {
                continue;
            }
            let path = local_photo(agent_dir, &photo.url)?;
            let size = std::fs::metadata(&path).map_err(|e| format!("cannot read {} ({e})", path.display()))?.len();
            if size > PHOTO_MAX {
                return Err(format!("{} is larger than 2 MB", path.display()));
            }
            let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {} ({e})", path.display()))?;
            let content_type = if bytes.starts_with(b"\x89PNG") { "image/png" } else { "image/jpeg" };
            let answer = self.send("POST", &format!("/v1/profiles/{handle}/photos"), Some((bytes, content_type)))?;
            photo.url = answer.get("url").and_then(Value::as_str).ok_or("The index sent no photo URL.")?.to_string();
        }
        self.send("PUT", &format!("/v1/profiles/{handle}/figure"), Some((figure.as_bytes().to_vec(), "text/javascript")))?;
        self.json("PUT", &format!("/v1/profiles/{handle}"), serde_json::to_value(&public).map_err(|e| e.to_string())?)?;
        Ok(())
    }
}

/// A photo path of the profile, which must stay inside the agent directory.
pub fn local_photo(agent_dir: &Path, path: &str) -> Result<PathBuf, String> {
    let rel = Path::new(path);
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return Err(format!("the photo {path:?} is not inside the personal agent's directory"));
    }
    Ok(agent_dir.join(rel))
}

/// Escapes what a path segment must not hold. Manifest ids keep their `/`.
fn path_part(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

const CLI_HELP: &str = "social takes one of:
  account [HANDLE]        show the account, or create it
  publish PROFILE.json    save the personal agent's profile and publish it
  relations               follows, followers and friend requests
  follow ACTOR            follow a handle or a manifest id, unfollow ACTOR ends it
  befriend HANDLE         ask to be friends, accept HANDLE and decline HANDLE answer a request
  feed                    the newest posts
  drafts                  post drafts that wait for your approval
  approve ID              publish a draft, discard ID drops it
  news                    add the news agent";

/// How long the command line waits for the daemon's answer.
const CLI_TIMEOUT: Duration = Duration::from_secs(60);

/// `meerora-agentd social ...`.
pub fn run_cli(socket: &Path, args: &[String], out: &mut dyn Write) -> anyhow::Result<()> {
    let arg = |i: usize| args.get(i).map(String::as_str);
    let request = format!("cli-{}", now_ms());
    let social = |action: SocialAction| Command::Social { request: request.clone(), action };
    let is_reply = |e: &Event| matches!(e, Event::Social { request: r, .. } if *r == request);
    match (arg(0), arg(1), args.len()) {
        (Some("account"), None, 1) => {
            let event = ask(socket, None, |e| matches!(e, Event::Account { .. }))?;
            if let Event::Account { index, handle, .. } = event {
                match handle {
                    Some(h) => writeln!(out, "@{h} on {index}")?,
                    None => writeln!(out, "no account on {index}. Create one with: meerora-agentd social account HANDLE")?,
                }
            }
        }
        (Some("account"), Some(handle), 2) => {
            let event = ask(socket, Some(Command::CreateAccount { handle: handle.into() }), |e| {
                matches!(e, Event::Account { handle: Some(_), .. } | Event::Account { error: Some(_), .. }) && !greeting_account(e)
            })?;
            match event {
                Event::Account { error: Some(e), .. } => bail!("{e}"),
                Event::Account { index, handle: Some(h), .. } => {
                    writeln!(out, "created @{h} on {index}. The token stays in {ACCOUNT_FILE}.")?
                }
                _ => {}
            }
        }
        (Some("publish"), Some(file), 2) => {
            let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {file}"))?;
            let profile: Profile = serde_json::from_str(&text).with_context(|| format!("{file} is not a profile"))?;
            let started = now_ms();
            let event = ask(socket, Some(Command::SaveProfile { profile, publish: true }), |e| match e {
                Event::OwnProfile { published: Some(at), .. } => *at >= started,
                Event::OwnProfile { error: Some(_), .. } => true,
                _ => false,
            })?;
            match event {
                Event::OwnProfile { error: Some(e), .. } => bail!("{e}"),
                Event::OwnProfile { profile: Some(p), .. } => writeln!(out, "published the profile of @{}", p.handle)?,
                _ => {}
            }
        }
        (Some(verb @ ("relations" | "follow" | "unfollow" | "befriend" | "accept" | "decline")), who, n) => {
            let action = match (verb, who) {
                ("relations", None) if n == 1 => SocialAction::Refresh,
                ("follow", Some(a)) if n == 2 => SocialAction::Follow { actor: a.into() },
                ("unfollow", Some(a)) if n == 2 => SocialAction::Unfollow { actor: a.into() },
                ("befriend", Some(h)) if n == 2 => SocialAction::Befriend { handle: h.into() },
                ("accept", Some(h)) if n == 2 => SocialAction::Accept { handle: h.into() },
                ("decline", Some(h)) if n == 2 => SocialAction::Decline { handle: h.into() },
                _ => bail!("{CLI_HELP}"),
            };
            let event = ask(socket, Some(social(action)), is_reply)?;
            let Event::Social { relations, error, .. } = event else { unreachable!("ask checked the event") };
            if let Some(e) = error {
                bail!("{e}");
            }
            print_relations(out, &relations.unwrap_or_default())?;
        }
        (Some("feed"), None, 1) => {
            let event = ask(
                socket,
                Some(Command::ReadFeed { request: request.clone(), before: None }),
                |e| matches!(e, Event::Feed { request: r, .. } if *r == request),
            )?;
            let Event::Feed { posts, error, .. } = event else { unreachable!("ask checked the event") };
            if let Some(e) = error {
                bail!("{e}");
            }
            write!(out, "{}", feed_text(&posts, ""))?;
        }
        (Some("drafts"), None, 1) => {
            let event = ask(socket, None, |e| matches!(e, Event::PostDrafts { .. }))?;
            let Event::PostDrafts { drafts, .. } = event else { unreachable!("ask checked the event") };
            if drafts.is_empty() {
                writeln!(out, "no drafts wait for you")?;
            }
            for d in drafts {
                writeln!(out, "{}  {:?} by {}: {}", d.id, d.visibility, d.agent, d.text)?;
            }
        }
        (Some(verb @ ("approve" | "discard")), Some(id), 2) => {
            let approve = verb == "approve";
            let command = Command::DecidePost { id: id.into(), approve, text: None, visibility: None };
            let event = ask(socket, Some(command), |e| match e {
                Event::PostDrafts { published: Some(_), .. } => true,
                Event::PostDrafts { drafts, .. } => drafts.iter().find(|d| d.id == id).is_none_or(|d| d.error.is_some()),
                _ => false,
            })?;
            match event {
                Event::PostDrafts { published: Some(p), .. } => writeln!(out, "published {} ({:?}): {}", p.id, p.visibility, p.text)?,
                Event::PostDrafts { drafts, .. } => match drafts.iter().find(|d| d.id == id).and_then(|d| d.error.clone()) {
                    Some(e) => bail!("{e}"),
                    None => writeln!(out, "discarded {id}")?,
                },
                _ => {}
            }
        }
        (Some("news"), None, 1) => {
            let event = ask(socket, Some(Command::AddTemplate { template: "news".into() }), |e| matches!(e, Event::TemplateAdded { .. }))?;
            match event {
                Event::TemplateAdded { error: Some(e), .. } => bail!("{e}"),
                Event::TemplateAdded { agent: Some(a), .. } => writeln!(out, "the news agent moved into agents/{a}")?,
                _ => {}
            }
        }
        _ => bail!("{CLI_HELP}"),
    }
    Ok(())
}

/// True for the greeting's account event, which is not an answer.
fn greeting_account(e: &Event) -> bool {
    matches!(e, Event::Account { error: None, handle: None, .. })
}

fn print_relations(out: &mut dyn Write, r: &Relations) -> std::io::Result<()> {
    let line = |out: &mut dyn Write, what: &str, list: &[meerora_protocol::Relation]| -> std::io::Result<()> {
        let names: Vec<String> =
            list.iter().map(|r| format!("{} ({}){}", r.actor.name, r.actor.id, if r.friend { " friend" } else { "" })).collect();
        writeln!(out, "{what:<10} {}", if names.is_empty() { "none".to_string() } else { names.join(", ") })
    };
    line(out, "follows", &r.follows)?;
    line(out, "followers", &r.followers)?;
    line(out, "requests", &r.requests)?;
    line(out, "pending", &r.pending)
}

/// Says hello, sends `command` and returns the first event `answer` accepts.
/// The greeting counts, so `None` reads a part of it.
fn ask(socket: &Path, command: Option<Command>, answer: impl Fn(&Event) -> bool) -> anyhow::Result<Event> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("cannot connect to {}. Is meerora-agentd running?", socket.display()))?;
    stream.set_read_timeout(Some(CLI_TIMEOUT))?;
    let hello = Command::Hello { version: PROTOCOL_VERSION, client: format!("meerora-agentd {} (social)", env!("CARGO_PKG_VERSION")) };
    let mut lines = encode(&hello);
    if let Some(c) = &command {
        lines.push_str(&encode(c));
    }
    stream.write_all(lines.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut greeted = command.is_none();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("no answer from the daemon")? == 0 {
            bail!("the daemon closed the connection");
        }
        let Ok(event) = decode_event(&line) else { continue };
        // Answers come after the greeting, which ends with the digest.
        if !greeted {
            greeted = matches!(event, Event::NewsDigest { .. });
            if greeted || !matches!(event, Event::Social { .. } | Event::Feed { .. } | Event::TemplateAdded { .. }) {
                continue;
            }
        }
        if answer(&event) {
            return Ok(event);
        }
    }
}
