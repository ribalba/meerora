//! The news agent (D28, milestone 5): the template that writes
//! `agents/news/`, the digest it posts with `post_digest`, and the reactions
//! the user gives to digest items, which go to the agent's
//! `memory/reactions.jsonl` so it can learn from them (D21).

use std::path::Path;

use meerora_protocol::{
    AgentConfig, AgentRole, App, BudgetConfig, CapabilitiesConfig, Digest, DigestItem, OnDone, Reaction, RuntimeConfig, ScheduleEntry,
    Settings,
};
use serde::{Deserialize, Serialize};

use crate::runs::now_ms;

pub const FIGURE: &str = include_str!("../../../assets/templates/news/figure.js");
pub const INSTRUCTIONS: &str = include_str!("../../../assets/templates/news/instructions.md");
pub const SOURCES: &str = include_str!("../../../assets/templates/news/sources.txt");
/// Where the user's sources live, relative to the agent directory.
pub const SOURCES_FILE: &str = "config/sources.txt";
/// Where reactions go, relative to the agent directory (D21).
pub const REACTIONS_FILE: &str = "memory/reactions.jsonl";
/// When the news agent wakes (D23).
pub const MORNING: &str = "07:00";
/// The most items one digest holds.
pub const MAX_ITEMS: usize = 12;

/// One line of `memory/reactions.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReactionLine {
    pub at: u64,
    pub digest: String,
    pub item: String,
    pub reaction: Reaction,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The agent.toml of a new news agent. It wakes every morning, may use the
/// web and rests after its runs.
pub fn config(stations: &[String], settings: &Settings) -> AgentConfig {
    // Reading happens at the bookshelf in the demo space.
    let home = ["bookshelf", "desk", "window"]
        .iter()
        .find(|s| stations.iter().any(|t| t == *s))
        .map(|s| s.to_string())
        .or_else(|| stations.iter().find(|s| *s != "door").cloned())
        .unwrap_or_else(|| "door".into());
    let runtime = settings.builder.clone().unwrap_or_else(crate::builder::default_runtime);
    AgentConfig {
        name: "Herald".into(),
        color: "#2F4A6D".into(),
        figure: "figure.js".into(),
        home: home.clone(),
        instructions: Some(INSTRUCTIONS.trim().to_string()),
        on_done: OnDone::Rest,
        apps: vec![App::Chat, App::Browser],
        workspace: Vec::new(),
        runtime: Some(RuntimeConfig { kind: runtime.kind, model: runtime.model, url: None }),
        schedule: vec![ScheduleEntry {
            every: None,
            at: Some(MORNING.into()),
            task: "Write this morning's digest, as your instructions say.".into(),
            station: Some(home),
            quiet_hours: None,
        }],
        budget: Some(BudgetConfig { tokens_per_day: Some(400_000) }),
        capabilities: CapabilitiesConfig { system: false, granted: vec!["web".into()] },
        external: None,
        role: Some(AgentRole::News),
    }
}

/// Writes `agents/news/` (or `news-2` and so on when the name is taken) and
/// returns the directory name. Refuses a second news agent.
pub fn add(data_dir: &Path, stations: &[String], settings: &Settings) -> Result<String, String> {
    let agents = data_dir.join("agents");
    if let Some(existing) =
        meerora_protocol::load_agents(data_dir).into_iter().filter_map(Result::ok).find(|a| a.config.role == Some(AgentRole::News))
    {
        return Err(format!("{} is your news agent already", existing.config.name));
    }
    let mut id = "news".to_string();
    let mut n = 2;
    while agents.join(&id).exists() {
        id = format!("news-{n}");
        n += 1;
    }
    let dir = agents.join(&id);
    let toml = toml::to_string(&config(stations, settings)).map_err(|e| e.to_string())?;
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir.join("config"))?;
        std::fs::create_dir_all(dir.join("memory"))?;
        std::fs::write(dir.join("figure.js"), FIGURE)?;
        std::fs::write(dir.join(SOURCES_FILE), SOURCES)?;
        // agent.toml last, so the agent never appears without its figure.
        std::fs::write(dir.join("agent.toml"), format!("# Added from the news agent template (D28).\n{toml}"))
    };
    write().map_err(|e| format!("cannot write {} ({e})", dir.display()))?;
    Ok(id)
}

/// Checks a digest from `post_digest` and gives its items ids.
pub fn digest(agent: &str, title: &str, items: Vec<DigestItem>) -> Result<Digest, String> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 120 {
        return Err("the title needs 1 to 120 characters".into());
    }
    if items.is_empty() || items.len() > MAX_ITEMS {
        return Err(format!("a digest has 1 to {MAX_ITEMS} items, this one has {}", items.len()));
    }
    let created = now_ms();
    let id = format!("digest-{created}");
    let mut checked = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let n = i + 1;
        let item_title = item.title.trim().to_string();
        if item_title.is_empty() || item_title.chars().count() > 160 {
            return Err(format!("item {n} needs a title of 1 to 160 characters"));
        }
        if item.summary.chars().count() > 600 {
            return Err(format!("the summary of item {n} is longer than 600 characters"));
        }
        let url = item.url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
        if let Some(url) = &url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(format!("the link of item {n} must start with https://"));
        }
        checked.push(DigestItem {
            id: format!("{id}-{n}"),
            title: item_title,
            summary: item.summary.trim().to_string(),
            url,
            source: item.source.map(|s| s.trim().chars().take(60).collect()).filter(|s: &String| !s.is_empty()),
            reaction: None,
            opened: false,
        });
    }
    Ok(Digest { id, agent: agent.into(), title: title.into(), created, items: checked })
}

/// Records a reaction on `digest` and appends it to the reactions file in
/// `agent_dir`. More and less replace each other, opened stays.
pub fn react(digest: &mut Digest, agent_dir: &Path, item: &str, reaction: Reaction) -> Result<(), String> {
    let digest_id = digest.id.clone();
    let entry = digest.items.iter_mut().find(|i| i.id == item).ok_or_else(|| format!("the digest has no item {item}"))?;
    match reaction {
        Reaction::More | Reaction::Less => entry.reaction = Some(reaction),
        Reaction::Opened => entry.opened = true,
        Reaction::Skipped => {}
    }
    let line = ReactionLine {
        at: now_ms(),
        digest: digest_id,
        item: item.into(),
        reaction,
        title: entry.title.clone(),
        source: entry.source.clone(),
        url: entry.url.clone(),
    };
    append(agent_dir, &[line])
}

/// Items of an old digest that nobody opened or rated, as skipped.
pub fn skipped(old: &Digest) -> Vec<ReactionLine> {
    let at = now_ms();
    old.items
        .iter()
        .filter(|i| i.reaction.is_none() && !i.opened)
        .map(|i| ReactionLine {
            at,
            digest: old.id.clone(),
            item: i.id.clone(),
            reaction: Reaction::Skipped,
            title: i.title.clone(),
            source: i.source.clone(),
            url: i.url.clone(),
        })
        .collect()
}

pub fn append(agent_dir: &Path, lines: &[ReactionLine]) -> Result<(), String> {
    if lines.is_empty() {
        return Ok(());
    }
    let path = agent_dir.join(REACTIONS_FILE);
    let write = || -> std::io::Result<()> {
        let mut text = String::new();
        for line in lines {
            text.push_str(&serde_json::to_string(line).map_err(std::io::Error::other)?);
            text.push('\n');
        }
        // The news agent writes memory/, so a symlink there must not lead the line elsewhere (safefs).
        meerora_protocol::safefs::append(agent_dir, REACTIONS_FILE, text.as_bytes())
    };
    write().map_err(|e| format!("cannot write {} ({e})", path.display()))
}

/// The lines of the reactions file.
#[cfg(test)]
pub fn read_reactions(agent_dir: &Path) -> Vec<ReactionLine> {
    std::fs::read_to_string(agent_dir.join(REACTIONS_FILE))
        .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::TempDir;

    fn item(title: &str) -> DigestItem {
        DigestItem {
            id: String::new(),
            title: title.into(),
            summary: "Two sentences.".into(),
            url: Some("https://news.example/a".into()),
            source: Some("News Example".into()),
            reaction: None,
            opened: false,
        }
    }

    #[test]
    fn the_template_is_a_scheduled_news_agent_with_web() {
        let data = TempDir::new("news-template");
        let stations = crate::scenario::tests::stations();
        let id = add(&data.0, &stations, &Settings::default()).unwrap();
        assert_eq!(id, "news");
        let dir = meerora_protocol::load_agent(&data.0.join("agents/news")).unwrap();
        let c = &dir.config;
        assert_eq!(c.role, Some(AgentRole::News));
        assert!(c.capabilities.has("web") && !c.capabilities.has("shell"));
        assert_eq!(c.schedule[0].at.as_deref(), Some(MORNING));
        assert!(crate::schedule::check_entry(&c.schedule[0]).is_ok());
        assert_eq!(c.on_done, OnDone::Rest);
        assert!(c.instructions.as_deref().unwrap().contains("memory/reactions.jsonl"));
        assert!(stations.contains(&c.home));
        assert!(data.0.join("agents/news/config/sources.txt").is_file());
        let figure = meerora_script::build_figure(FIGURE, "figure.js", &meerora_script::Limits::default()).unwrap();
        assert!(figure.params.iter().any(|p| p.name == "fresh"));
        assert!(add(&data.0, &stations, &Settings::default()).unwrap_err().contains("already"));
    }

    #[test]
    fn digests_are_checked_and_reactions_land_in_memory() {
        let data = TempDir::new("news-react");
        assert!(digest("news", "", vec![item("a")]).is_err());
        assert!(digest("news", "Today", Vec::new()).is_err());
        let mut bad = item("a");
        bad.url = Some("ftp://x".into());
        assert!(digest("news", "Today", vec![bad]).unwrap_err().contains("https://"));

        let mut d = digest("news", "Wednesday", vec![item("Ferry strike"), item("Rust 2.0"), item("Weather")]).unwrap();
        assert_eq!(d.items[0].id, format!("{}-1", d.id));
        let (first, second) = (d.items[0].id.clone(), d.items[1].id.clone());
        react(&mut d, &data.0, &first, Reaction::More).unwrap();
        react(&mut d, &data.0, &second, Reaction::Opened).unwrap();
        react(&mut d, &data.0, &second, Reaction::Less).unwrap();
        assert!(react(&mut d, &data.0, "nothing", Reaction::More).is_err());
        assert_eq!(d.items[1].reaction, Some(Reaction::Less));
        assert!(d.items[1].opened);
        append(&data.0, &skipped(&d)).unwrap();
        let lines = read_reactions(&data.0);
        let got: Vec<(&str, Reaction)> = lines.iter().map(|l| (l.title.as_str(), l.reaction)).collect();
        assert_eq!(
            got,
            [
                ("Ferry strike", Reaction::More),
                ("Rust 2.0", Reaction::Opened),
                ("Rust 2.0", Reaction::Less),
                ("Weather", Reaction::Skipped)
            ]
        );
        assert_eq!(lines[0].source.as_deref(), Some("News Example"));
    }
}
