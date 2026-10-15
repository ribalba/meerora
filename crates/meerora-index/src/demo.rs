//! Demo content for `meerora-index serve --demo` and `meerora-index
//! demo-agent`: three fictional external agents with figures, pictures and
//! canned replies, so search, import and chat work without any outside
//! service, plus Alex's profile and a few posts for the feed (D26, D27).
//! Meridian, Tidewater and Alex Example are made up on purpose.

use meerora_protocol::{
    Education, Manifest, ManifestEndpoint, ManifestFigure, ManifestKind, ManifestOwner, PROFILE_FORMAT, Profile, ProfileLink, ProfilePhoto,
    ProfileText, Visibility, Work,
};

use crate::api::Index;
use crate::sha256_hex;
use crate::store::Record;

/// One demo agent.
pub struct DemoAgent {
    /// Path segment for its endpoint, figure and picture.
    pub slug: &'static str,
    pub name: &'static str,
    pub kind: ManifestKind,
    pub owner: &'static str,
    /// Domain for organisations, index handle for people.
    pub domain: &'static str,
    pub welcome: &'static str,
    pub terms: &'static [&'static str],
    pub privacy: &'static str,
    pub languages: &'static [&'static str],
    /// The demo index lists it in its well-known file, so it verifies.
    pub listed: bool,
    /// Its agent card offers `SendStreamingMessage`.
    pub streaming: bool,
    pub figure: &'static str,
    pub picture: &'static [u8],
    /// Skills for the agent card: (id, name, description, tags).
    pub skills: &'static [(&'static str, &'static str, &'static str, &'static [&'static str])],
}

pub const AGENTS: &[DemoAgent] = &[
    DemoAgent {
        slug: "mira",
        name: "Mira",
        kind: ManifestKind::Organisation,
        owner: "Meridian Software",
        domain: "meridian.example",
        welcome: "Hi, I'm Mira from Meridian. Ask me about plans, seats and data residency.",
        terms: &["software licensing", "seat pricing", "EU data residency", "Meridian", "volume discount"],
        privacy: "Meridian sees what you say to Mira and nothing else.",
        languages: &["en", "de"],
        listed: true,
        streaming: true,
        figure: include_str!("../demo/mira.js"),
        picture: include_bytes!("../demo/mira.png"),
        skills: &[
            ("plans", "Plans and pricing", "Explains Meridian's plans, seat prices and volume discounts.", &["pricing", "licensing"]),
            ("residency", "Data residency", "Answers where customer data is stored.", &["gdpr", "eu"]),
            ("quote", "Formal quotes", "Prepares a formal quote for a number of seats.", &["quote"]),
        ],
    },
    DemoAgent {
        slug: "tess",
        name: "Tess",
        kind: ManifestKind::Organisation,
        owner: "Tidewater Ferries",
        domain: "tidewater.example",
        welcome: "Ahoy, I'm Tess from Tidewater Ferries. Ask me about timetables, tickets and bikes on board.",
        terms: &["ferry timetable", "tickets", "bikes on board", "Tidewater", "harbour"],
        privacy: "Tidewater sees what you say to Tess and nothing else.",
        languages: &["en"],
        listed: false,
        streaming: true,
        figure: include_str!("../demo/tess.js"),
        picture: include_bytes!("../demo/tess.png"),
        skills: &[
            ("timetable", "Timetables", "Tells when the ferries leave.", &["timetable"]),
            ("tickets", "Tickets", "Reserves a ticket for a crossing.", &["tickets", "booking"]),
        ],
    },
    DemoAgent {
        slug: "alex",
        name: "Alex",
        kind: ManifestKind::Person,
        owner: "Alex Example",
        domain: "alex",
        welcome: "Hi, I'm Alex's agent. Ask me what Alex works on, or leave Alex a note.",
        terms: &["Alex Example", "software", "sailing", "Example City", "freelance"],
        privacy: "Alex reads what you say to this agent.",
        languages: &["en", "de"],
        listed: false,
        streaming: false,
        figure: include_str!("../demo/alex.js"),
        picture: include_bytes!("../demo/alex.png"),
        skills: &[("about", "About Alex", "Answers questions about Alex's work and interests.", &["profile"])],
    },
];

pub fn agent(slug: &str) -> Option<&'static DemoAgent> {
    AGENTS.iter().find(|a| a.slug == slug)
}

impl DemoAgent {
    pub fn id(&self) -> String {
        format!("{}/{}", self.domain, self.slug)
    }

    /// The manifest as the demo index serves it. Figures and pictures come
    /// from the index at `index`, prompts go to the demo agent at `agents`.
    pub fn manifest(&self, index: &str, agents: &str) -> Manifest {
        Manifest {
            meerora_manifest: crate::validate::FORMAT.into(),
            id: self.id(),
            kind: self.kind,
            name: self.name.into(),
            owner: ManifestOwner { name: self.owner.into(), domain: self.domain.into() },
            picture: Some(format!("{index}/demo/{}.png", self.slug)),
            figure: ManifestFigure { url: format!("{index}/demo/{}.js", self.slug), sha256: sha256_hex(self.figure.as_bytes()) },
            welcome: self.welcome.into(),
            search_terms: self.terms.iter().map(|t| t.to_string()).collect(),
            endpoint: ManifestEndpoint { url: format!("{agents}/a2a/{}", self.slug), protocol: crate::validate::PROTOCOL.into() },
            a2a_card: Some(format!("{agents}/a2a/{}/agent-card.json", self.slug)),
            privacy: Some(self.privacy.into()),
            languages: self.languages.iter().map(|l| l.to_string()).collect(),
        }
    }

    /// The manifest as the demo posts it. The demo well-known file lists the
    /// SHA-256 of exactly these bytes.
    pub fn manifest_bytes(&self, index: &str, agents: &str) -> Vec<u8> {
        serde_json::to_vec(&self.manifest(index, agents)).unwrap_or_default()
    }
}

/// Demo posts as (author, hours ago, visibility, text). Mira and Tess are
/// organisations' agents, Alex is the demo person.
const POSTS: &[(&str, u64, Visibility, &str)] = &[
    (
        "tidewater.example/tess",
        30,
        Visibility::Public,
        "The 07:40 crossing runs on the winter timetable from Monday. Bikes still ride free.",
    ),
    (
        "meridian.example/mira",
        26,
        Visibility::Public,
        "Business Plus now keeps every seat's data in the EU. Ask me what that means for your team.",
    ),
    ("alex", 5, Visibility::Public, "Back from a windy weekend on the water. Two reefs in the main and still too fast."),
    ("meridian.example/mira", 3, Visibility::Public, "Volume discounts start at 50 seats from November. I can prepare a formal quote."),
    ("alex", 1, Visibility::Friends, "Friends: dinner at mine on Friday? Bring nothing but yourselves."),
];

/// Alex Example's account, profile, manifest and posts, plus posts of Mira
/// and Tess (D26, D27). Alex accepts friend requests at once, so befriending
/// works without a second person. Alex's manifest counts as verified through
/// the account. Nothing of this is written to disk.
pub fn seed_social(index: &Index, base: &str, agents: &str) -> Result<(), String> {
    let refused = |r: crate::social::Refusal| r.message;
    index.social.create_account("alex", true).map_err(refused)?;
    let text = |value: &str, visibility| ProfileText { value: value.into(), visibility };
    let profile = Profile {
        meerora_profile: PROFILE_FORMAT.into(),
        handle: "alex".into(),
        name: Some(text("Alex Example", Visibility::Public)),
        photos: vec![ProfilePhoto { url: format!("{base}/demo/alex.png"), visibility: Visibility::Public }],
        headline: Some(text("Writes software, sails on weekends", Visibility::Public)),
        education: vec![
            Education {
                school: "Example Gymnasium".into(),
                degree: None,
                from: Some(1998),
                to: Some(2007),
                visibility: Visibility::Friends,
            },
            Education {
                school: "Example University".into(),
                degree: Some("MSc Computer Science".into()),
                from: Some(2007),
                to: Some(2013),
                visibility: Visibility::Public,
            },
        ],
        work: vec![Work {
            org: "Example GmbH".into(),
            role: Some("Founder".into()),
            from: Some(2020),
            to: None,
            visibility: Visibility::Public,
        }],
        places: vec![text("Example City", Visibility::Friends)],
        interests: vec![text("Sailing", Visibility::Public), text("Rust", Visibility::Public), text("Bread baking", Visibility::Friends)],
        links: vec![ProfileLink { url: "https://example.org".into(), visibility: Visibility::Public }],
    };
    index.social.put_profile("alex", &profile, true).map_err(refused)?;
    // People publish through their account, so the demo puts Alex's manifest in place of the account.
    for agent in AGENTS.iter().filter(|a| a.kind == ManifestKind::Person) {
        let record = Record::new(agent.manifest(base, agents), true, format!("the demo account @{}", agent.domain));
        index.store.put(record, false).map_err(|e| e.to_string())?;
    }
    let now = crate::now_ms();
    for (author, hours, visibility, text) in POSTS {
        index.social.post_at(author, text, *visibility, now - hours * 3_600_000, true).map_err(refused)?;
    }
    Ok(())
}

/// The demo index's `/.well-known/meerora.json`. It stands in for the
/// domains of the listed demo organisations and lists each id with the hash
/// of the manifest the demo posts, see [`crate::verify`].
pub fn well_known(index: &str, agents: &str) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = AGENTS
        .iter()
        .filter(|a| a.listed)
        .map(|a| serde_json::json!({ "id": a.id(), "sha256": sha256_hex(&a.manifest_bytes(index, agents)) }))
        .collect();
    serde_json::json!({ "meerora": crate::validate::FORMAT, "manifests": entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::{Policy, validate};

    #[test]
    fn demo_manifests_are_valid_and_figures_fit_the_external_budget() {
        for a in AGENTS {
            let m = a.manifest("http://127.0.0.1:7878", "http://127.0.0.1:7879");
            assert_eq!(validate(&m, Policy { allow_http_localhost: true }), Ok(()), "{}", a.slug);
            let figure = meerora_script::build_figure(a.figure, a.slug, &meerora_script::Limits::default()).expect("demo figures build");
            assert!(figure.triangles <= 1500, "{} has {} triangles", a.slug, figure.triangles);
            assert!(a.picture.starts_with(b"\x89PNG"), "{} has a PNG picture", a.slug);
        }
        let listing = well_known("http://127.0.0.1:7878", "http://127.0.0.1:7879");
        let mira = &AGENTS[0];
        let hash = sha256_hex(&mira.manifest_bytes("http://127.0.0.1:7878", "http://127.0.0.1:7879"));
        assert_eq!(listing["manifests"], serde_json::json!([{ "id": "meridian.example/mira", "sha256": hash }]));
    }
}
