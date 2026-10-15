//! Accounts, profiles, photos, follows and posts of the index (D26, D27).
//!
//! The model mirrors ActivityPub so federation stays possible (D27): people
//! and organisations' agents are actors, posts are notes, and a follow is a
//! Follow activity that holds once it is accepted. A plain follow is
//! accepted at once and shows public posts. A friend request is a Follow
//! that waits for the other side's Accept, and accepting it stores two
//! accepted friend follows, one in each direction. A friendship is exactly
//! that pair, and it unlocks friends fields and friends posts.
//!
//! Actor ids: a person is their handle (`alex`), an organisation's agent is
//! its manifest id (`meridian.example/mira`). Handles never hold a `/`.
//!
//! Accounts carry only a SHA-256 hash of their token. The token is shown
//! once, when the account is created. Private profile fields never reach
//! this store, [`Social::put_profile`] drops them.
//!
//! Everything lives in memory and, with a directory, in `social.json` plus
//! one file per photo below `photos/`. Photos stay on disk and are read when
//! someone asks for one, only an index without a directory keeps them in
//! memory. Demo content is never written.
//!
//! Every account has quotas: [`PHOTOS_PER_ACCOUNT`] photos, [`POSTS_PER_DAY`]
//! posts in 24 hours, [`FOLLOWS_PER_ACCOUNT`] follows and
//! [`REQUESTS_PER_ACCOUNT`] open friend requests.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use meerora_protocol::{Profile, Visibility};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::sha256_hex;

/// The largest photo the index takes.
pub const PHOTO_MAX: usize = 2 * 1024 * 1024;
/// Photos one account may keep, used in its profile or not.
pub const PHOTOS_PER_ACCOUNT: usize = 24;
/// The largest figure script of a personal agent.
pub const FIGURE_MAX: usize = 256 * 1024;
/// The longest post, in characters.
pub const POST_MAX: usize = 1000;
/// Posts one author may publish in 24 hours.
pub const POSTS_PER_DAY: usize = 100;
/// Follows one account may hold, friends included.
pub const FOLLOWS_PER_ACCOUNT: usize = 1000;
/// Friend requests one account may have waiting at once.
pub const REQUESTS_PER_ACCOUNT: usize = 200;
const DAY_MS: u64 = 24 * 3_600_000;
/// Posts one feed page holds at most, and by default.
pub const PAGE_MAX: usize = 50;
pub const PAGE_DEFAULT: usize = 20;
/// Photos uploaded this recently survive a profile that does not use them
/// yet, because the daemon uploads the photos before the profile.
const PHOTO_GRACE_MS: u64 = 10 * 60 * 1000;

/// A refusal with its HTTP status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

fn refuse<T>(status: u16, message: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal { status, message: message.into() })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub handle: String,
    /// Lower-case hex SHA-256 of the token. The token itself is never stored.
    pub token_sha256: String,
    pub created: u64,
    /// Demo people accept friend requests at once, so the flow can be tried alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub demo: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredProfile {
    /// Public and friends fields only.
    pub profile: Profile,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub demo: bool,
}

/// An accepted follow. `friend` marks one half of a friendship.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Follow {
    /// The follower, always a handle.
    pub actor: String,
    /// Who is followed: a handle or a manifest id.
    pub object: String,
    #[serde(default)]
    pub friend: bool,
    pub at: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub demo: bool,
}

/// A friend request that waits for the other side (a pending Follow).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FriendRequest {
    pub from: String,
    pub to: String,
    pub at: u64,
}

/// A post (a note).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub seq: u64,
    /// A handle or a manifest id.
    pub author: String,
    pub text: String,
    pub visibility: Visibility,
    pub published: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub demo: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhotoMeta {
    pub owner: String,
    pub content_type: String,
    pub uploaded: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    accounts: BTreeMap<String, Account>,
    #[serde(default)]
    profiles: BTreeMap<String, StoredProfile>,
    /// Figure scripts of personal agents by handle, for their manifests.
    #[serde(default)]
    figures: BTreeMap<String, String>,
    #[serde(default)]
    follows: Vec<Follow>,
    #[serde(default)]
    requests: Vec<FriendRequest>,
    #[serde(default)]
    posts: Vec<Note>,
    #[serde(default)]
    photos: BTreeMap<String, PhotoMeta>,
    #[serde(default)]
    seq: u64,
    /// Photo bytes of an index without a directory. With a directory they
    /// are in `photos/<id>` and only read on demand.
    #[serde(skip)]
    photo_bytes: BTreeMap<String, Arc<Vec<u8>>>,
}

pub struct Social {
    dir: Option<PathBuf>,
    state: RwLock<State>,
}

impl Social {
    /// Keeps nothing on disk.
    pub fn memory() -> Self {
        Self { dir: None, state: RwLock::new(State::default()) }
    }

    /// Loads `dir/social.json` and the photos below `dir/photos`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir.join("photos"))?;
        let path = dir.join("social.json");
        let mut state: State = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e),
        };
        let ids: Vec<String> = state.photos.keys().cloned().collect();
        for id in ids {
            if let Err(e) = std::fs::metadata(dir.join("photos").join(&id)) {
                warn!("photo {id} is gone ({e})");
                state.photos.remove(&id);
            }
        }
        Ok(Self { dir: Some(dir.to_path_buf()), state: RwLock::new(state) })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes everything except the demo content.
    fn save(&self, state: &State) -> Result<(), Refusal> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let mut kept = state.clone();
        let demo_people: Vec<String> = kept.accounts.values().filter(|a| a.demo).map(|a| a.handle.clone()).collect();
        kept.accounts.retain(|_, a| !a.demo);
        kept.profiles.retain(|_, p| !p.demo);
        kept.follows.retain(|f| !f.demo);
        kept.posts.retain(|p| !p.demo);
        kept.photos.retain(|_, p| !demo_people.contains(&p.owner));
        kept.figures.retain(|handle, _| !demo_people.contains(handle));
        kept.photo_bytes.clear();
        let write = || -> io::Result<()> {
            let text = serde_json::to_string_pretty(&kept).map_err(io::Error::other)?;
            let tmp = dir.join("social.json.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, dir.join("social.json"))
        };
        write().map_err(|e| Refusal { status: 500, message: format!("cannot store the change ({e})") })
    }

    // Accounts.

    /// Creates an account and returns its token, the only time it is shown.
    pub fn create_account(&self, handle: &str, demo: bool) -> Result<String, Refusal> {
        if !crate::validate::is_handle(handle) {
            return refuse(422, format!("{handle:?} is not a handle. Use 2 to 32 of a-z, 0-9, - and _, starting with a letter or digit."));
        }
        let token = new_token()?;
        let mut state = self.write();
        if state.accounts.contains_key(handle) {
            return refuse(409, format!("@{handle} is taken"));
        }
        let account = Account { handle: handle.into(), token_sha256: sha256_hex(token.as_bytes()), created: crate::now_ms(), demo };
        state.accounts.insert(handle.into(), account);
        self.save(&state)?;
        Ok(token)
    }

    /// The handle a token belongs to.
    pub fn authenticate(&self, token: &str) -> Option<String> {
        let hash = sha256_hex(token.trim().as_bytes());
        self.read().accounts.values().find(|a| a.token_sha256 == hash).map(|a| a.handle.clone())
    }

    pub fn has_account(&self, handle: &str) -> bool {
        self.read().accounts.contains_key(handle)
    }

    pub fn account(&self, handle: &str) -> Option<Account> {
        self.read().accounts.get(handle).cloned()
    }

    // Profiles, figures and photos.

    /// Stores the profile of `handle` without its private fields.
    pub fn put_profile(&self, handle: &str, profile: &Profile, demo: bool) -> Result<Profile, Refusal> {
        if !profile.handle.is_empty() && profile.handle != handle {
            return refuse(422, format!("the profile is for @{}, not @{handle}", profile.handle));
        }
        let problems = profile.problems();
        if !problems.is_empty() {
            return refuse(422, problems.join(". "));
        }
        let mut stored = profile.visible(Visibility::Friends);
        stored.handle = handle.into();
        let now = crate::now_ms();
        let mut state = self.write();
        // Photos of this account that the profile no longer uses go away.
        let used: Vec<String> = stored.photos.iter().filter_map(|p| photo_id(&p.url)).collect();
        let stale: Vec<String> = state
            .photos
            .iter()
            .filter(|(id, p)| p.owner == handle && !used.contains(id) && now.saturating_sub(p.uploaded) > PHOTO_GRACE_MS)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            state.photos.remove(id);
            state.photo_bytes.remove(id);
            if let Some(dir) = &self.dir {
                let _ = std::fs::remove_file(dir.join("photos").join(id));
            }
        }
        state.profiles.insert(handle.into(), StoredProfile { profile: stored.clone(), updated: now, demo });
        self.save(&state)?;
        Ok(stored)
    }

    /// The stored profile (public and friends fields).
    pub fn profile(&self, handle: &str) -> Option<Profile> {
        self.read().profiles.get(handle).map(|p| p.profile.clone())
    }

    /// The profile as `viewer` may see it: friends fields for the owner and
    /// friends, public fields for everyone else (D26).
    pub fn profile_for(&self, handle: &str, viewer: Option<&str>) -> Option<Profile> {
        let state = self.read();
        let profile = &state.profiles.get(handle)?.profile;
        let allowed = match viewer {
            Some(v) if v == handle || friends(&state, v, handle) => Visibility::Friends,
            _ => Visibility::Public,
        };
        Some(profile.visible(allowed))
    }

    pub fn put_figure(&self, handle: &str, script: &str) -> Result<String, Refusal> {
        if script.len() > FIGURE_MAX {
            return refuse(413, format!("the figure is larger than {} KB", FIGURE_MAX / 1024));
        }
        if !script.contains("figure(") {
            return refuse(422, "this is not a figure script");
        }
        let mut state = self.write();
        state.figures.insert(handle.into(), script.into());
        self.save(&state)?;
        Ok(sha256_hex(script.as_bytes()))
    }

    pub fn figure(&self, handle: &str) -> Option<String> {
        self.read().figures.get(handle).cloned()
    }

    /// Stores a PNG or JPEG photo of `owner` and returns its id. The same
    /// bytes of the same account get the same id. The id depends on the
    /// account too, so an upload never tells whether someone else has the photo.
    pub fn add_photo(&self, owner: &str, bytes: &[u8]) -> Result<String, Refusal> {
        if bytes.len() > PHOTO_MAX {
            return refuse(413, format!("the photo is larger than {} MB", PHOTO_MAX / 1024 / 1024));
        }
        let content_type = image_type(bytes).ok_or(Refusal { status: 415, message: "send a PNG or JPEG photo".into() })?;
        let id = sha256_hex(&[owner.as_bytes(), b"\0", bytes].concat())[..32].to_string();
        let mut state = self.write();
        if let Some(existing) = state.photos.get(&id) {
            if existing.owner != owner {
                return refuse(500, "cannot store the photo, try another one");
            }
            return Ok(id);
        }
        if state.photos.values().filter(|p| p.owner == owner).count() >= PHOTOS_PER_ACCOUNT {
            return refuse(413, format!("an account keeps at most {PHOTOS_PER_ACCOUNT} photos"));
        }
        match &self.dir {
            Some(dir) => std::fs::write(dir.join("photos").join(&id), bytes)
                .map_err(|e| Refusal { status: 500, message: format!("cannot store the photo ({e})") })?,
            None => {
                state.photo_bytes.insert(id.clone(), Arc::new(bytes.to_vec()));
            }
        }
        state.photos.insert(id.clone(), PhotoMeta { owner: owner.into(), content_type: content_type.into(), uploaded: crate::now_ms() });
        self.save(&state)?;
        Ok(id)
    }

    /// Whether `id` is a photo that `owner` uploaded.
    pub fn owns_photo(&self, owner: &str, id: &str) -> bool {
        self.read().photos.get(id).is_some_and(|p| p.owner == owner)
    }

    /// A photo with its content type, when `viewer` may see it. The owner's
    /// profile decides: public photos for everyone, friends photos for
    /// friends, photos the profile does not show only for the owner. With a
    /// directory the bytes come from disk.
    pub fn photo(&self, id: &str, viewer: Option<&str>) -> Option<(String, Vec<u8>)> {
        let (content_type, memory) = {
            let state = self.read();
            let meta = state.photos.get(id)?;
            if !may_see_photo(&state, id, meta, viewer) {
                return None;
            }
            (meta.content_type.clone(), state.photo_bytes.get(id).cloned())
        };
        let bytes = match (&self.dir, memory) {
            (Some(dir), _) => std::fs::read(dir.join("photos").join(id)).map_err(|e| warn!("cannot read photo {id} ({e})")).ok()?,
            (None, Some(bytes)) => bytes.as_ref().clone(),
            (None, None) => return None,
        };
        Some((content_type, bytes))
    }

    // Follows and friends.

    /// Follows a person or an organisation's agent. `exists` says whether an
    /// actor id names someone.
    pub fn follow(&self, actor: &str, object: &str, exists: bool) -> Result<(), Refusal> {
        if actor == object {
            return refuse(422, "you cannot follow yourself");
        }
        if !exists {
            return refuse(404, format!("nobody is called {object:?} in this index"));
        }
        let mut state = self.write();
        if state.follows.iter().any(|f| f.actor == actor && f.object == object) {
            return Ok(());
        }
        if state.follows.iter().filter(|f| f.actor == actor).count() >= FOLLOWS_PER_ACCOUNT {
            return refuse(429, format!("an account follows at most {FOLLOWS_PER_ACCOUNT} actors"));
        }
        state.follows.push(Follow { actor: actor.into(), object: object.into(), friend: false, at: crate::now_ms(), demo: false });
        self.save(&state)
    }

    /// Ends a follow. Ending a friend follow ends the friendship, and the
    /// other side keeps a plain follow.
    pub fn unfollow(&self, actor: &str, object: &str) -> Result<(), Refusal> {
        let mut state = self.write();
        let before = state.follows.len() + state.requests.len();
        let was_friend = state.follows.iter().any(|f| f.actor == actor && f.object == object && f.friend);
        state.follows.retain(|f| !(f.actor == actor && f.object == object));
        state.requests.retain(|r| !(r.from == actor && r.to == object));
        if was_friend {
            for f in state.follows.iter_mut().filter(|f| f.actor == object && f.object == actor) {
                f.friend = false;
            }
        }
        if state.follows.len() + state.requests.len() == before {
            return refuse(404, format!("you do not follow {object}"));
        }
        self.save(&state)
    }

    /// Asks `to` to be friends. When `to` asked first, this accepts.
    /// Demo people accept at once.
    pub fn befriend(&self, from: &str, to: &str) -> Result<(), Refusal> {
        if from == to {
            return refuse(422, "you cannot befriend yourself");
        }
        let mut state = self.write();
        let Some(other) = state.accounts.get(to) else {
            return refuse(404, format!("there is no account @{to}"));
        };
        let demo = other.demo;
        if friends(&state, from, to) {
            return Ok(());
        }
        let asked_back = state.requests.iter().any(|r| r.from == to && r.to == from);
        let follows = state.follows.iter().filter(|f| f.actor == from).count();
        let new_follow = !state.follows.iter().any(|f| f.actor == from && f.object == to);
        if new_follow && follows >= FOLLOWS_PER_ACCOUNT {
            return refuse(429, format!("an account follows at most {FOLLOWS_PER_ACCOUNT} actors, friends included"));
        }
        if asked_back || demo {
            make_friends(&mut state, from, to);
        } else if !state.requests.iter().any(|r| r.from == from && r.to == to) {
            if state.requests.iter().filter(|r| r.from == from).count() >= REQUESTS_PER_ACCOUNT {
                return refuse(429, format!("at most {REQUESTS_PER_ACCOUNT} friend requests can wait at once"));
            }
            state.requests.push(FriendRequest { from: from.into(), to: to.into(), at: crate::now_ms() });
        }
        self.save(&state)
    }

    /// `me` accepts the friend request of `them`.
    pub fn accept(&self, me: &str, them: &str) -> Result<(), Refusal> {
        let mut state = self.write();
        if !state.requests.iter().any(|r| r.from == them && r.to == me) {
            return refuse(404, format!("@{them} has not asked you to be friends"));
        }
        make_friends(&mut state, me, them);
        self.save(&state)
    }

    /// `me` declines the friend request of `them`.
    pub fn decline(&self, me: &str, them: &str) -> Result<(), Refusal> {
        let mut state = self.write();
        let before = state.requests.len();
        state.requests.retain(|r| !(r.from == them && r.to == me));
        if state.requests.len() == before {
            return refuse(404, format!("@{them} has not asked you to be friends"));
        }
        self.save(&state)
    }

    pub fn are_friends(&self, a: &str, b: &str) -> bool {
        friends(&self.read(), a, b)
    }

    /// (follows, followers, requests to `handle`, requests by `handle`).
    #[allow(clippy::type_complexity)]
    pub fn relations(&self, handle: &str) -> (Vec<Follow>, Vec<Follow>, Vec<FriendRequest>, Vec<FriendRequest>) {
        let state = self.read();
        let follows = state.follows.iter().filter(|f| f.actor == handle).cloned().collect();
        let followers = state.follows.iter().filter(|f| f.object == handle).cloned().collect();
        let to_me = state.requests.iter().filter(|r| r.to == handle).cloned().collect();
        let by_me = state.requests.iter().filter(|r| r.from == handle).cloned().collect();
        (follows, followers, to_me, by_me)
    }

    // Posts and the feed.

    pub fn post(&self, author: &str, text: &str, visibility: Visibility, demo: bool) -> Result<Note, Refusal> {
        self.post_at(author, text, visibility, crate::now_ms(), demo)
    }

    /// A post with a given time, for the demo.
    pub fn post_at(&self, author: &str, text: &str, visibility: Visibility, published: u64, demo: bool) -> Result<Note, Refusal> {
        let text = text.trim();
        if text.is_empty() {
            return refuse(422, "the post is empty");
        }
        if text.chars().count() > POST_MAX {
            return refuse(422, format!("a post has at most {POST_MAX} characters"));
        }
        if visibility == Visibility::Private {
            return refuse(422, "posts are public or friends, a private post would reach nobody");
        }
        if text.chars().any(|c| crate::validate::is_unsafe_char(c) && c != '\n') {
            return refuse(422, "the post contains control or bidirectional characters");
        }
        let mut state = self.write();
        let since = crate::now_ms().saturating_sub(DAY_MS);
        if !demo && state.posts.iter().filter(|p| p.author == author && p.published >= since).count() >= POSTS_PER_DAY {
            return refuse(429, format!("at most {POSTS_PER_DAY} posts in 24 hours"));
        }
        state.seq += 1;
        let seq = state.seq;
        let note = Note { id: format!("post-{seq}"), seq, author: author.into(), text: text.into(), visibility, published, demo };
        state.posts.push(note.clone());
        self.save(&state)?;
        Ok(note)
    }

    /// One page of `viewer`'s feed, newest first: their own posts and the
    /// posts of everyone they follow, friends posts only from friends (D27).
    /// `before` is the `next` cursor of the previous page.
    pub fn feed(&self, viewer: &str, before: Option<&str>, limit: usize) -> Result<(Vec<Note>, Option<String>), Refusal> {
        let before = match before.map(parse_cursor) {
            Some(None) => return refuse(400, "the cursor is not one this index gave out"),
            Some(Some(c)) => Some(c),
            None => None,
        };
        let limit = limit.clamp(1, PAGE_MAX);
        let state = self.read();
        let follows: Vec<&str> = state.follows.iter().filter(|f| f.actor == viewer).map(|f| f.object.as_str()).collect();
        let mut posts: Vec<&Note> = state
            .posts
            .iter()
            .filter(|p| {
                p.author == viewer
                    || (follows.contains(&p.author.as_str())
                        && (p.visibility == Visibility::Public
                            || (p.visibility == Visibility::Friends && friends(&state, viewer, &p.author))))
            })
            .filter(|p| before.is_none_or(|c| (p.published, p.seq) < c))
            .collect();
        posts.sort_by_key(|p| std::cmp::Reverse((p.published, p.seq)));
        let more = posts.len() > limit;
        posts.truncate(limit);
        let next = if more { posts.last().map(|p| format!("{}-{}", p.published, p.seq)) } else { None };
        Ok((posts.into_iter().cloned().collect(), next))
    }
}

/// Whether `viewer` may see the photo `id`, as its owner's profile says.
fn may_see_photo(state: &State, id: &str, meta: &PhotoMeta, viewer: Option<&str>) -> bool {
    let owner = meta.owner.as_str();
    let visibility = state
        .profiles
        .get(owner)
        .and_then(|p| p.profile.photos.iter().find(|ph| photo_id(&ph.url).as_deref() == Some(id)).map(|ph| ph.visibility));
    match (visibility, viewer) {
        (_, Some(v)) if v == owner => true,
        (Some(Visibility::Public), _) => true,
        (Some(Visibility::Friends), Some(v)) => friends(state, v, owner),
        _ => false,
    }
}

/// Whether `a` and `b` follow each other as friends.
fn friends(state: &State, a: &str, b: &str) -> bool {
    let half = |x: &str, y: &str| state.follows.iter().any(|f| f.actor == x && f.object == y && f.friend);
    half(a, b) && half(b, a)
}

/// Two accepted friend follows, one each way, and no requests left between them.
fn make_friends(state: &mut State, a: &str, b: &str) {
    state.requests.retain(|r| !((r.from == a && r.to == b) || (r.from == b && r.to == a)));
    let now = crate::now_ms();
    for (x, y) in [(a, b), (b, a)] {
        match state.follows.iter_mut().find(|f| f.actor == x && f.object == y) {
            Some(f) => f.friend = true,
            None => state.follows.push(Follow { actor: x.into(), object: y.into(), friend: true, at: now, demo: false }),
        }
    }
}

/// `published-seq` of the last post of a page.
fn parse_cursor(cursor: &str) -> Option<(u64, u64)> {
    let (published, seq) = cursor.split_once('-')?;
    Some((published.parse().ok()?, seq.parse().ok()?))
}

/// The photo id of an index photo URL, `.../v1/photos/<id>`.
pub fn photo_id(url: &str) -> Option<String> {
    let id = url.rsplit_once("/v1/photos/")?.1;
    (id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| id.to_string())
}

/// `image/png` or `image/jpeg` from the first bytes.
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// 32 random bytes from the kernel as hex.
pub(crate) fn new_token() -> Result<String, Refusal> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| Refusal { status: 500, message: format!("cannot make a token ({e})") })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use meerora_protocol::{Education, ProfilePhoto, ProfileText};

    pub fn profile(handle: &str) -> Profile {
        let text = |value: &str, visibility| ProfileText { value: value.into(), visibility };
        Profile {
            handle: handle.into(),
            name: Some(text("Robin Example", Visibility::Public)),
            headline: Some(text("Builds boats", Visibility::Public)),
            education: vec![
                Education {
                    school: "Example School".into(),
                    degree: None,
                    from: Some(2001),
                    to: Some(2009),
                    visibility: Visibility::Friends,
                },
                Education {
                    school: "Example University".into(),
                    degree: Some("BSc".into()),
                    from: Some(2009),
                    to: Some(2012),
                    visibility: Visibility::Public,
                },
            ],
            places: vec![text("Example Bay", Visibility::Friends)],
            interests: vec![text("Rowing", Visibility::Public), text("Secret hobby", Visibility::Private)],
            ..Profile { meerora_profile: meerora_protocol::PROFILE_FORMAT.into(), ..Profile::default() }
        }
    }

    #[test]
    fn tokens_are_stored_as_hashes_and_authenticate() {
        let dir = std::env::temp_dir().join(format!("meerora-index-social-{}-{}", std::process::id(), crate::now_ms()));
        let social = Social::open(&dir).unwrap();
        let token = social.create_account("robin", false).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(social.authenticate(&token).as_deref(), Some("robin"));
        assert_eq!(social.authenticate("not the token"), None);
        assert_eq!(social.create_account("robin", false).unwrap_err().status, 409);
        assert_eq!(social.create_account("Robin Hood", false).unwrap_err().status, 422);
        let stored = std::fs::read_to_string(dir.join("social.json")).unwrap();
        assert!(!stored.contains(&token), "the token is never written");
        assert!(stored.contains(&sha256_hex(token.as_bytes())));
        // A restart keeps the account.
        let again = Social::open(&dir).unwrap();
        assert_eq!(again.authenticate(&token).as_deref(), Some("robin"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn private_fields_never_reach_the_store_and_viewers_see_their_level() {
        let social = Social::memory();
        for h in ["robin", "kim", "sam"] {
            social.create_account(h, false).unwrap();
        }
        let stored = social.put_profile("robin", &profile("robin"), false).unwrap();
        assert_eq!(stored.interests.len(), 1, "the private interest is dropped");
        assert!(!serde_json::to_string(&social.profile("robin")).unwrap().contains("Secret hobby"));

        let public = social.profile_for("robin", None).unwrap();
        assert_eq!((public.education.len(), public.places.len()), (1, 0));
        let stranger = social.profile_for("robin", Some("sam")).unwrap();
        assert_eq!(stranger, public, "a stranger with an account sees the public fields");
        assert_eq!(social.profile_for("robin", Some("robin")).unwrap().places.len(), 1, "the owner sees friends fields");

        // A plain follow unlocks nothing.
        social.follow("kim", "robin", true).unwrap();
        assert_eq!(social.profile_for("robin", Some("kim")).unwrap(), public);
        social.befriend("kim", "robin").unwrap();
        assert_eq!(social.profile_for("robin", Some("kim")).unwrap(), public, "a request is not a friendship");
        social.accept("robin", "kim").unwrap();
        let friend = social.profile_for("robin", Some("kim")).unwrap();
        assert_eq!((friend.education.len(), friend.places.len(), friend.interests.len()), (2, 1, 1));

        let mut wrong = profile("kim");
        wrong.handle = "kim".into();
        assert_eq!(social.put_profile("robin", &wrong, false).unwrap_err().status, 422);
    }

    #[test]
    fn follows_requests_and_friendships() {
        let social = Social::memory();
        for h in ["robin", "kim", "sam"] {
            social.create_account(h, false).unwrap();
        }
        assert_eq!(social.follow("robin", "robin", true).unwrap_err().status, 422);
        assert_eq!(social.follow("robin", "nobody", false).unwrap_err().status, 404);
        social.follow("robin", "meridian.example/mira", true).unwrap();
        social.follow("robin", "meridian.example/mira", true).unwrap();
        assert_eq!(social.relations("robin").0.len(), 1, "following twice is one follow");

        // Two accepted friend follows make a friendship.
        social.befriend("robin", "kim").unwrap();
        let (_, _, to_kim, _) = social.relations("kim");
        assert_eq!(to_kim.len(), 1);
        assert!(!social.are_friends("robin", "kim"));
        social.accept("kim", "robin").unwrap();
        assert!(social.are_friends("robin", "kim") && social.are_friends("kim", "robin"));
        let (follows, followers, to_robin, by_robin) = social.relations("robin");
        assert!(follows.iter().any(|f| f.object == "kim" && f.friend));
        assert!(followers.iter().any(|f| f.actor == "kim" && f.friend));
        assert!(to_robin.is_empty() && by_robin.is_empty());
        assert_eq!(social.accept("kim", "robin").unwrap_err().status, 404, "nothing left to accept");

        // Asking back accepts, declining removes the request.
        social.befriend("sam", "robin").unwrap();
        social.befriend("robin", "sam").unwrap();
        assert!(social.are_friends("robin", "sam"));
        social.befriend("kim", "sam").unwrap();
        social.decline("sam", "kim").unwrap();
        assert!(!social.are_friends("kim", "sam"));
        assert_eq!(social.decline("sam", "kim").unwrap_err().status, 404);

        // Unfollowing a friend ends the friendship, the other side keeps a plain follow.
        social.unfollow("robin", "kim").unwrap();
        assert!(!social.are_friends("robin", "kim"));
        assert!(social.relations("kim").0.iter().any(|f| f.object == "robin" && !f.friend));
        assert_eq!(social.unfollow("robin", "kim").unwrap_err().status, 404);
        assert_eq!(social.befriend("robin", "nobody").unwrap_err().status, 404);
    }

    #[test]
    fn demo_people_accept_at_once() {
        let social = Social::memory();
        social.create_account("robin", false).unwrap();
        social.create_account("alex", true).unwrap();
        social.befriend("robin", "alex").unwrap();
        assert!(social.are_friends("robin", "alex"));
    }

    #[test]
    fn the_feed_is_newest_first_pages_and_hides_friends_posts_from_followers() {
        let social = Social::memory();
        for h in ["robin", "kim", "sam"] {
            social.create_account(h, false).unwrap();
        }
        social.post_at("meridian.example/mira", "Mira 1", Visibility::Public, 1_000, false).unwrap();
        social.post_at("kim", "Kim public", Visibility::Public, 2_000, false).unwrap();
        social.post_at("kim", "Kim friends", Visibility::Friends, 3_000, false).unwrap();
        social.post_at("sam", "Sam unfollowed", Visibility::Public, 4_000, false).unwrap();
        social.post_at("robin", "Robin own", Visibility::Friends, 5_000, false).unwrap();
        social.post_at("meridian.example/mira", "Mira 2", Visibility::Public, 5_000, false).unwrap();
        assert_eq!(social.post("robin", "secret", Visibility::Private, false).unwrap_err().status, 422);
        assert_eq!(social.post("robin", "  ", Visibility::Public, false).unwrap_err().status, 422);

        social.follow("robin", "meridian.example/mira", true).unwrap();
        social.follow("robin", "kim", true).unwrap();
        let texts = |notes: &[Note]| notes.iter().map(|n| n.text.clone()).collect::<Vec<_>>();
        let (page, next) = social.feed("robin", None, 50).unwrap();
        assert_eq!(texts(&page), ["Mira 2", "Robin own", "Kim public", "Mira 1"], "a follower sees no friends posts");
        assert!(next.is_none());

        social.befriend("robin", "kim").unwrap();
        social.accept("kim", "robin").unwrap();
        let (first, next) = social.feed("robin", None, 2).unwrap();
        assert_eq!(texts(&first), ["Mira 2", "Robin own"], "equal times keep the later post first");
        let (second, next) = social.feed("robin", next.as_deref(), 2).unwrap();
        assert_eq!(texts(&second), ["Kim friends", "Kim public"]);
        let (third, next) = social.feed("robin", next.as_deref(), 2).unwrap();
        assert_eq!((texts(&third), next), (vec!["Mira 1".to_string()], None));
        assert_eq!(social.feed("robin", Some("garbage"), 2).unwrap_err().status, 400);
    }

    #[test]
    fn photos_stay_on_disk_and_load_on_demand() {
        let dir = std::env::temp_dir().join(format!("meerora-index-photos-{}-{}", std::process::id(), crate::now_ms()));
        let social = Social::open(&dir).unwrap();
        social.create_account("robin", false).unwrap();
        let png = include_bytes!("../demo/mira.png");
        let id = social.add_photo("robin", png).unwrap();
        assert!(social.read().photo_bytes.is_empty(), "no photo bytes in memory");
        assert!(social.owns_photo("robin", &id) && !social.owns_photo("kim", &id));
        assert_eq!(social.photo(&id, Some("robin")).unwrap().1, png.to_vec());
        let again = Social::open(&dir).unwrap();
        assert!(again.read().photo_bytes.is_empty(), "a restart loads no photos either");
        assert_eq!(again.photo(&id, Some("robin")).unwrap().1, png.to_vec());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn accounts_have_quotas_for_posts_follows_and_requests() {
        let social = Social::memory();
        social.create_account("robin", false).unwrap();
        for i in 0..POSTS_PER_DAY {
            social.post("robin", &format!("post {i}"), Visibility::Public, false).unwrap();
        }
        assert_eq!(social.post("robin", "one too many", Visibility::Public, false).unwrap_err().status, 429);
        // Older posts do not count.
        social.post_at("kim", "old", Visibility::Public, 1_000, false).unwrap();

        for i in 0..FOLLOWS_PER_ACCOUNT {
            social.follow("robin", &format!("org{i}.example/a"), true).unwrap();
        }
        assert_eq!(social.follow("robin", "more.example/a", true).unwrap_err().status, 429);
        social.follow("robin", "org0.example/a", true).unwrap();

        let social = Social::memory();
        for i in 0..=REQUESTS_PER_ACCOUNT {
            social.create_account(&format!("p{i}"), false).unwrap();
        }
        social.create_account("sam", false).unwrap();
        for i in 0..REQUESTS_PER_ACCOUNT {
            social.befriend("sam", &format!("p{i}")).unwrap();
        }
        assert_eq!(social.befriend("sam", &format!("p{REQUESTS_PER_ACCOUNT}")).unwrap_err().status, 429);
        assert_eq!(social.post("sam", "Hi \u{202E}evil", Visibility::Public, false).unwrap_err().status, 422);
        social.post("sam", "two\nlines", Visibility::Public, false).unwrap();
    }

    #[test]
    fn photos_follow_the_profile_visibility() {
        let social = Social::memory();
        for h in ["robin", "kim"] {
            social.create_account(h, false).unwrap();
        }
        let png = include_bytes!("../demo/mira.png");
        let id = social.add_photo("robin", png).unwrap();
        assert_eq!(social.add_photo("robin", png).unwrap(), id, "the same bytes keep their id");
        let kims = social.add_photo("kim", png).unwrap();
        assert_ne!(kims, id, "another account gets its own id, and learns nothing about robin's photos");
        assert_eq!(social.add_photo("robin", b"GIF89a").unwrap_err().status, 415);
        assert!(social.photo(&id, None).is_none(), "not in the profile yet, so only the owner sees it");
        assert!(social.photo(&id, Some("robin")).is_some());

        let mut p = profile("robin");
        p.photos = vec![ProfilePhoto { url: format!("http://127.0.0.1:1/v1/photos/{id}"), visibility: Visibility::Friends }];
        social.put_profile("robin", &p, false).unwrap();
        assert!(social.photo(&id, None).is_none() && social.photo(&id, Some("kim")).is_none());
        social.befriend("kim", "robin").unwrap();
        social.accept("robin", "kim").unwrap();
        assert_eq!(social.photo(&id, Some("kim")).map(|(t, _)| t).as_deref(), Some("image/png"));
        p.photos[0].visibility = Visibility::Public;
        social.put_profile("robin", &p, false).unwrap();
        assert!(social.photo(&id, None).is_some());
    }
}
