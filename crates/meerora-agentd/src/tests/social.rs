//! Personal agents and the feed (D26, D27, D28) with the fake runtime and an
//! in-process demo index: the account, the interview, publishing, follow
//! and befriend, the feed, post approval, the news template, the digest and
//! its reactions.

use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::time::Duration;

use meerora_index::Demo;
use meerora_protocol::{
    AgentRole, Command, DigestItem, DraftStatus, Event, InterviewTurn, ProfilePhoto, Reaction, RuntimeConfig, RuntimeKind, Settings,
    SocialAction, ToolCall, Visibility,
};
use meerora_runtime::RunEvent;

use super::{Fake, Harness, Step, finished, write_agent};
use crate::daemon::{Daemon, Options};
use crate::interview::{self, InterviewJob};
use crate::scenario::Scenario;
use crate::social::{ACCOUNT_FILE, Api, PROFILE_FILE, read_account};
use crate::store::tests::TempDir;

const ME: &str = "name = \"Robin\"\nhome = \"desk\"\nrole = \"personal\"\n[runtime]\nkind = \"claude-cli\"\nmodel = \"haiku\"\n";
const WREN: &str = "name = \"Wren\"\nhome = \"window\"\n[runtime]\nkind = \"claude-cli\"\n";

/// A daemon with the personal agent `me` and an ordinary agent `wren`,
/// pointed at a demo index.
fn harness(demo: &Demo) -> Harness {
    let dir = TempDir::new("social");
    write_agent(&dir.0, "me", ME);
    write_agent(&dir.0, "wren", WREN);
    let index = demo.index.url();
    // Reloading reads meerora.toml again, so the index URL lives there too.
    std::fs::write(dir.0.join("meerora.toml"), format!("[index]\nurl = \"{index}\"\n")).unwrap();
    let fake = Fake::default();
    let (tx, rx) = mpsc::channel();
    let agents = meerora_protocol::load_agents(&dir.0).into_iter().map(Result::unwrap).collect();
    let settings = Settings { index: meerora_protocol::IndexSettings { url: Some(index) }, ..Settings::default() };
    let options = Options {
        data_dir: dir.0.clone(),
        socket: dir.0.join("agentd.sock"),
        settings,
        stations: crate::scenario::tests::stations(),
        factory: fake.factory(),
        bridge: None,
        git: false,
        speed: 1.0,
    };
    let daemon = Daemon::new(options, Scenario::default(), agents, tx);
    Harness { daemon, rx, fake, dir }
}

/// Creates the account `robin` and returns its token.
fn sign_up(h: &mut Harness) -> String {
    h.command(Command::CreateAccount { handle: "robin".into() });
    let events = h.until(|e| matches!(e, Event::Account { .. }));
    assert!(events.iter().any(|e| matches!(e, Event::Account { handle: Some(h), error: None, .. } if h == "robin")), "{events:?}");
    let index = h.daemon.opts.settings.index.url.clone().unwrap();
    read_account(&h.dir.0, &index).expect("the account file").token
}

fn tool(h: &mut Harness, agent: &str, call: ToolCall) -> Vec<Event> {
    h.command(Command::Tool { id: "t1".into(), agent: agent.into(), call })
}

fn answer(events: &[Event]) -> Option<(bool, String)> {
    events.iter().find_map(|e| match e {
        Event::ToolResult { id, ok, message, .. } if id == "t1" => Some((*ok, message.clone())),
        _ => None,
    })
}

fn item(title: &str) -> DigestItem {
    DigestItem {
        id: String::new(),
        title: title.into(),
        summary: "Two sentences about it.".into(),
        url: Some("https://news.example/a".into()),
        source: Some("News Example".into()),
        reaction: None,
        opened: false,
    }
}

#[test]
fn the_account_token_stays_in_a_private_file() {
    let demo = Demo::start().unwrap();
    let mut h = harness(&demo);
    let greeting = h.daemon.greeting();
    assert!(greeting.iter().any(|e| matches!(e, Event::Account { handle: None, .. })), "no account yet");
    let token = sign_up(&mut h);
    let path = h.dir.0.join(ACCOUNT_FILE);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    for file in ["meerora.toml", "agents/me/agent.toml"] {
        assert!(!std::fs::read_to_string(h.dir.0.join(file)).unwrap().contains(&token), "{file} never holds the token");
    }
    let greeting = h.daemon.greeting();
    assert!(greeting.iter().any(|e| matches!(e, Event::Account { handle: Some(h), .. } if h == "robin")));
    assert!(!format!("{greeting:?}").contains(&token), "the token never leaves the daemon");

    let events = h.command(Command::CreateAccount { handle: "robin2".into() });
    assert!(events.iter().any(|e| matches!(e, Event::Account { error: Some(e), .. } if e.contains("already"))));
}

#[test]
fn the_interview_retries_until_the_profile_is_valid() {
    let fake = Fake::default();
    // No visibility on the name: the format check refuses it.
    fake.script(vec![
        Step::Send(RunEvent::Session { id: "i1".into() }),
        finished(r#"{"question": "Where do you work?", "profile": {"name": {"value": "Robin"}}}"#),
    ]);
    fake.script(vec![finished(
        r#"Here: {"question": "Where do you work?", "done": false, "profile": {"name": {"value": "Robin Example", "visibility": "public"}}}"#,
    )]);
    let job = InterviewJob {
        session: "s-1".into(),
        turns: vec![InterviewTurn { question: "What is your name?".into(), answer: "Robin Example".into() }],
        profile: None,
        handle: "robin".into(),
    };
    let mut events = Vec::new();
    let config = RuntimeConfig { kind: RuntimeKind::ClaudeCli, model: Some("haiku".into()), url: None };
    interview::run(&job, &config, &Settings::default(), &fake.factory(), &mut |e| events.push(e));
    let statuses: Vec<DraftStatus> =
        events.iter().filter_map(|e| if let Event::InterviewStep { status, .. } = e { Some(*status) } else { None }).collect();
    assert_eq!(statuses, [DraftStatus::Working, DraftStatus::Working, DraftStatus::Ready]);
    let Event::InterviewStep { message, .. } = &events[1] else { unreachable!() };
    assert!(message.contains("does not fit"), "{message}");
    let Event::InterviewStep { question, profile, done, .. } = &events[2] else { unreachable!() };
    assert_eq!((question.as_deref(), *done), (Some("Where do you work?"), false));
    let profile = profile.as_ref().unwrap();
    assert_eq!((profile.handle.as_str(), profile.name.as_ref().unwrap().value.as_str()), ("robin", "Robin Example"));

    let requests = fake.requests();
    assert!(requests[0].prompt.contains("Answer: Robin Example") && requests[0].system.contains("\"visibility\""));
    assert!(requests[0].mcp.is_none() && !requests[0].permissions.write, "the interviewer gets no tools");
    assert_eq!(requests[1].session.as_deref(), Some("i1"), "the retry continues the conversation");
    assert!(requests[1].prompt.contains("does not fit"));

    // Three broken answers fail the step.
    let fake = Fake::default();
    for _ in 0..3 {
        fake.script(vec![finished("I would rather chat.")]);
    }
    let mut events = Vec::new();
    interview::run(&job, &config, &Settings::default(), &fake.factory(), &mut |e| events.push(e));
    assert!(matches!(events.last(), Some(Event::InterviewStep { status: DraftStatus::Failed, .. })), "{events:?}");
}

#[test]
fn the_daemon_runs_the_interview_with_the_personal_agents_model() {
    let demo = Demo::start().unwrap();
    let mut h = harness(&demo);
    h.fake.script(vec![finished(r#"{"question": "What is your name?", "done": false, "profile": {}}"#)]);
    h.command(Command::Interview { session: "s-2".into(), agent: "me".into(), turns: Vec::new(), profile: None });
    let events = h.until(|e| matches!(e, Event::InterviewStep { status: DraftStatus::Ready, .. }));
    assert!(events.iter().any(|e| matches!(e, Event::InterviewStep { question: Some(q), .. } if q == "What is your name?")));
    assert_eq!(h.fake.requests()[0].model.as_deref(), Some("haiku"));
}

#[test]
fn publish_follow_befriend_and_read_the_feed() {
    let demo = Demo::start().unwrap();
    let mut h = harness(&demo);
    // Saving needs no account, publishing does.
    let mut profile: meerora_protocol::Profile = serde_json::from_str(
        r#"{"name": {"value": "Robin Example", "visibility": "public"}, "places": [{"value": "Example Bay", "visibility": "friends"}],
            "interests": [{"value": "A private thing", "visibility": "private"}]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(h.agent_dir("me").join("photos")).unwrap();
    std::fs::write(h.agent_dir("me").join("photos/1.png"), include_bytes!("../../../meerora-index/demo/tess.png")).unwrap();
    profile.photos = vec![ProfilePhoto { url: "photos/1.png".into(), visibility: Visibility::Public }];
    let events = h.command(Command::SaveProfile { profile: profile.clone(), publish: true });
    assert!(events.iter().any(|e| matches!(e, Event::OwnProfile { error: Some(e), .. } if e.contains("account"))), "{events:?}");
    let local = std::fs::read_to_string(h.agent_dir("me").join(PROFILE_FILE)).unwrap();
    assert!(local.contains("A private thing"), "the local profile keeps private fields");

    let token = sign_up(&mut h);
    h.command(Command::SaveProfile { profile, publish: true });
    let events = h.until(|e| matches!(e, Event::OwnProfile { published: Some(_), .. } | Event::OwnProfile { error: Some(_), .. }));
    assert!(events.iter().any(|e| matches!(e, Event::OwnProfile { published: Some(_), error: None, .. })), "{events:?}");

    // The person is found by name, with the photo on the index and without the private interest.
    let found = crate::index::search_as(&demo.index.url(), "Robin Example", None).unwrap();
    let robin = found.iter().find(|e| e.manifest.id == "robin/robin").expect("found by name");
    assert!(robin.verified && robin.manifest.picture.as_deref().is_some_and(|p| p.contains("/v1/photos/")));
    let public = robin.profile.as_ref().unwrap();
    assert!(public.places.is_empty() && public.interests.is_empty());
    assert!(robin.figure_script.is_some(), "the personal agent's figure passed its checksum");

    // Follow Mira, befriend Alex (the demo accepts at once), read the feed.
    h.command(Command::Social { request: "r1".into(), action: SocialAction::Follow { actor: "meridian.example/mira".into() } });
    let events = h.until(|e| matches!(e, Event::Social { .. }));
    let Some(Event::Social { relations: Some(r), .. }) = events.iter().find(|e| matches!(e, Event::Social { .. })) else {
        panic!("{events:?}")
    };
    assert!(r.follows("meridian.example/mira"));
    // A change pushes the first feed page to every client.
    let events = h.until(|e| matches!(e, Event::Feed { request, .. } if request.is_empty()));
    let Some(Event::Feed { posts, .. }) = events.last() else { unreachable!() };
    assert!(posts.iter().any(|p| p.author.id == "meridian.example/mira"));
    h.command(Command::Social { request: "r2".into(), action: SocialAction::Befriend { handle: "alex".into() } });
    let events = h.until(|e| matches!(e, Event::Social { .. }));
    let Some(Event::Social { relations: Some(r), .. }) = events.iter().find(|e| matches!(e, Event::Social { .. })) else {
        panic!("{events:?}")
    };
    assert!(r.is_friend("alex"));

    h.command(Command::ReadFeed { request: "f1".into(), before: None });
    let events = h.until(|e| matches!(e, Event::Feed { request, .. } if request == "f1"));
    let Some(Event::Feed { posts, error: None, .. }) = events.last() else { panic!("{events:?}") };
    assert!(posts.iter().any(|p| p.text.starts_with("Friends: dinner")), "friends see friends posts");
    assert!(posts.windows(2).all(|w| w[0].published >= w[1].published), "newest first");
    assert!(!posts.iter().any(|p| p.author.id == "tidewater.example/tess"), "Tess is not followed");
    assert!(!token.is_empty());
}

#[test]
fn a_draft_reaches_the_index_only_after_approve() {
    let demo = Demo::start().unwrap();
    let mut h = harness(&demo);
    let token = sign_up(&mut h);
    let api = Api::new(&demo.index.url(), Some(token));
    let own = |api: &Api| api.feed(None, 50).unwrap().0.into_iter().filter(|p| p.author.id == "robin").count();

    let events = tool(&mut h, "wren", ToolCall::DraftPost { text: "Hi".into(), visibility: Visibility::Public });
    assert!(answer(&events).is_some_and(|(ok, m)| !ok && m.contains("Only the personal agent")));
    let events = tool(&mut h, "me", ToolCall::DraftPost { text: "Secret".into(), visibility: Visibility::Private });
    assert!(answer(&events).is_some_and(|(ok, _)| !ok));

    let events =
        tool(&mut h, "me", ToolCall::DraftPost { text: "Robin sailed across the bay today.".into(), visibility: Visibility::Public });
    assert!(answer(&events).is_some_and(|(ok, m)| ok && m.contains("approval")));
    let Some(Event::PostDrafts { drafts, .. }) = events.iter().find(|e| matches!(e, Event::PostDrafts { .. })) else {
        panic!("{events:?}")
    };
    let id = drafts[0].id.clone();
    assert!(h.dir.0.join(crate::social::STATE_FILE).is_file(), "drafts wait outside every agent directory");
    assert!(h.daemon.greeting().iter().any(|e| matches!(e, Event::PostDrafts { drafts, .. } if drafts.len() == 1)));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(own(&api), 0, "nothing reaches the index before approve");

    // A second draft is discarded and never goes out.
    tool(&mut h, "me", ToolCall::DraftPost { text: "Discard me".into(), visibility: Visibility::Friends });
    let second = h.daemon.social.state.drafts[1].id.clone();
    let events = h.command(Command::DecidePost { id: second, approve: false, text: None, visibility: None });
    assert!(events.iter().any(|e| matches!(e, Event::PostDrafts { drafts, published: None } if drafts.len() == 1)));

    // Approve with the owner's edit.
    let edited = "Robin sailed across the whole bay today.".to_string();
    h.command(Command::DecidePost { id, approve: true, text: Some(edited.clone()), visibility: Some(Visibility::Friends) });
    let events = h.until(|e| matches!(e, Event::PostDrafts { published: Some(_), .. }));
    let Some(Event::PostDrafts { drafts, published: Some(post) }) = events.last() else { panic!("{events:?}") };
    assert!(drafts.is_empty());
    assert_eq!((post.text.as_str(), post.visibility), (edited.as_str(), Visibility::Friends));
    assert_eq!(own(&api), 1);
    assert!(!api.feed(None, 50).unwrap().0.iter().any(|p| p.text == "Discard me"));
}

#[test]
fn the_news_agent_template_digest_and_reactions() {
    let demo = Demo::start().unwrap();
    let mut h = harness(&demo);
    let events = h.command(Command::AddTemplate { template: "news".into() });
    assert!(events.iter().any(|e| matches!(e, Event::TemplateAdded { agent: Some(a), error: None, .. } if a == "news")), "{events:?}");
    assert!(events.contains(&Event::AgentSpawned { agent: "news".into(), name: "Herald".into(), system: false }));
    assert!(events.iter().any(|e| matches!(e, Event::NextWake { agent, at: Some(_), .. } if agent == "news")), "it wakes every morning");
    let config = &h.daemon.agents["news"].dir.config;
    assert_eq!(config.role, Some(AgentRole::News));
    assert!(config.capabilities.has("web"));
    let events = h.command(Command::AddTemplate { template: "news".into() });
    assert!(events.iter().any(|e| matches!(e, Event::TemplateAdded { error: Some(_), .. })), "only one news agent");

    // Only the news agent posts digests.
    let digest = || ToolCall::PostDigest { title: "Wednesday".into(), items: vec![item("Ferry strike"), item("Rust 2.0"), item("Rain")] };
    assert!(answer(&tool(&mut h, "wren", digest())).is_some_and(|(ok, _)| !ok));
    let events = tool(&mut h, "news", digest());
    assert!(answer(&events).is_some_and(|(ok, m)| ok && m.contains("pinned")), "{events:?}");
    assert!(events.contains(&Event::Param { agent: "news".into(), name: "fresh".into(), value: serde_json::Value::Bool(true) }));
    let first = h.daemon.social.state.digest.clone().unwrap();
    assert!(h.daemon.greeting().iter().any(|e| matches!(e, Event::NewsDigest { digest: Some(d) } if d.id == first.id)));

    // Reactions go to the agent's memory, and the newspaper goes down.
    let events = h.command(Command::React { digest: first.id.clone(), item: first.items[0].id.clone(), reaction: Reaction::More });
    assert!(events.contains(&Event::Param { agent: "news".into(), name: "fresh".into(), value: serde_json::Value::Bool(false) }));
    h.command(Command::React { digest: first.id.clone(), item: first.items[1].id.clone(), reaction: Reaction::Opened });
    h.command(Command::React { digest: first.id.clone(), item: first.items[1].id.clone(), reaction: Reaction::Less });
    let Some(Event::NewsDigest { digest: Some(d) }) =
        h.command(Command::React { digest: first.id.clone(), item: first.items[0].id.clone(), reaction: Reaction::More }).pop()
    else {
        panic!("the digest is sent again")
    };
    assert_eq!((d.items[1].reaction, d.items[1].opened), (Some(Reaction::Less), true));

    // The next digest records the untouched item as skipped.
    tool(&mut h, "news", ToolCall::PostDigest { title: "Thursday".into(), items: vec![item("Snow")] });
    let lines = crate::news::read_reactions(&h.agent_dir("news"));
    let got: Vec<(&str, Reaction)> = lines.iter().map(|l| (l.title.as_str(), l.reaction)).collect();
    assert_eq!(
        got,
        [
            ("Ferry strike", Reaction::More),
            ("Rust 2.0", Reaction::Opened),
            ("Rust 2.0", Reaction::Less),
            ("Ferry strike", Reaction::More),
            ("Rain", Reaction::Skipped)
        ]
    );
    assert!(h.agent_dir("news").join("memory/reactions.jsonl").is_file());

    // read_feed needs the account and answers off the daemon thread.
    let events = tool(&mut h, "news", ToolCall::ReadFeed { limit: None });
    assert!(answer(&events).is_some_and(|(ok, m)| !ok && m.contains("account")), "{events:?}");
    sign_up(&mut h);
    h.command(Command::Social { request: "r".into(), action: SocialAction::Follow { actor: "meridian.example/mira".into() } });
    h.until(|e| matches!(e, Event::Feed { request, .. } if request.is_empty()));
    assert!(tool(&mut h, "news", ToolCall::ReadFeed { limit: Some(5) }).is_empty());
    let events = h.until(|e| matches!(e, Event::ToolResult { .. }));
    let (ok, text) = answer(&events).unwrap();
    assert!(ok && text.contains("Mira of Meridian Software") && text.contains("Volume discounts"), "{text}");
    assert!(answer(&tool(&mut h, "wren", ToolCall::ReadFeed { limit: None })).is_some_and(|(ok, _)| !ok));
}

#[test]
fn the_personal_and_the_news_agent_get_their_tools_explained() {
    let me = crate::scenario::tests::agent("me", ME);
    let prompt = crate::runs::system_prompt(&me, &["desk".into()]);
    assert!(prompt.contains("draft_post") && prompt.contains("approval"));
    let wren = crate::scenario::tests::agent("wren", WREN);
    assert!(!crate::runs::system_prompt(&wren, &["desk".into()]).contains("draft_post"));
}
