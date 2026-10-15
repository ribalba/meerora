//! Ranking for `GET /v1/search` (D25, D26). Organisations and people come
//! back in one list. A query is split into lower-case words, and each word
//! scores by where it matches: the name counts most, then the owner and the
//! search terms, then the welcome text. Results that match more of the words
//! come first.

use meerora_protocol::Manifest;

/// The most results one search returns.
pub const LIMIT: usize = 50;
/// Only the first words of a query count, so a long query costs no more
/// than a short one.
pub const MAX_QUERY_WORDS: usize = 8;
/// The longest query word, in characters.
const MAX_WORD: usize = 40;

/// Lower-case words of letters and digits.
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// How well `m` matches the query words: (words matched, score). Zero
/// matched words means no match.
pub fn score(m: &Manifest, query: &[String]) -> (usize, u32) {
    let fields: [(Vec<String>, u32, u32); 5] = [
        (words(&m.name), 12, 7),
        (words(&m.owner.name), 7, 4),
        (m.search_terms.iter().flat_map(|t| words(t)).collect(), 6, 3),
        (words(&m.owner.domain), 4, 2),
        (words(&m.welcome), 2, 1),
    ];
    let mut matched = 0;
    let mut total = 0;
    for q in query {
        let best = fields
            .iter()
            .map(|(field, exact, prefix)| {
                if field.iter().any(|w| w == q) {
                    *exact
                } else if q.chars().count() >= 2 && field.iter().any(|w| w.starts_with(q.as_str())) {
                    *prefix
                } else {
                    0
                }
            })
            .max()
            .unwrap_or(0);
        if best > 0 {
            matched += 1;
            total += best;
        }
    }
    // A search term typed as a whole phrase is a strong hint.
    let phrase = query.join(" ");
    if !phrase.is_empty() && m.search_terms.iter().any(|t| words(t).join(" ") == phrase) {
        total += 8;
    }
    (matched, total)
}

/// The words of a query that count: at most [`MAX_QUERY_WORDS`], each cut
/// to 40 characters.
pub fn query_words(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(MAX_QUERY_WORDS)
        .map(|w| w.chars().take(MAX_WORD).collect::<String>().to_lowercase())
        .collect()
}

/// Ranks `entries` for `query`. An empty query lists everything, verified
/// owners first, then by name.
pub fn rank<'a, T>(entries: impl IntoIterator<Item = &'a T>, query: &str, manifest: impl Fn(&T) -> (&Manifest, bool)) -> Vec<&'a T>
where
    T: 'a,
{
    let query = query_words(query);
    let mut hits: Vec<(usize, u32, bool, String, &T)> = entries
        .into_iter()
        .filter_map(|entry| {
            let (m, verified) = manifest(entry);
            let (matched, score) = if query.is_empty() { (1, 0) } else { score(m, &query) };
            (matched > 0).then(|| (matched, score, verified, m.name.to_lowercase(), entry))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)).then(a.3.cmp(&b.3)));
    hits.into_iter().take(LIMIT).map(|h| h.4).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::tests::manifest;
    use meerora_protocol::{ManifestKind, ManifestOwner};

    fn entries() -> Vec<(Manifest, bool)> {
        let mira = manifest();
        let mut tess = manifest();
        tess.id = "tidewater.example/tess".into();
        tess.name = "Tess".into();
        tess.owner = ManifestOwner { name: "Tidewater Ferries".into(), domain: "tidewater.example".into() };
        tess.welcome = "Ask me about timetables, tickets and bikes on board, or software for the harbour.".into();
        tess.search_terms = vec!["ferry timetable".into(), "tickets".into()];
        let mut alex = manifest();
        alex.id = "alex/alex".into();
        alex.kind = ManifestKind::Person;
        alex.name = "Alex".into();
        alex.owner = ManifestOwner { name: "Alex Example".into(), domain: "alex".into() };
        alex.welcome = "Hi, I'm Alex's agent. Alex writes software and sails on weekends.".into();
        alex.search_terms = vec!["sailing".into(), "freelance".into()];
        vec![(mira, true), (tess, false), (alex, false)]
    }

    fn names(query: &str) -> Vec<String> {
        let e = entries();
        rank(&e, query, |(m, v)| (m, *v)).into_iter().map(|(m, _)| m.name.clone()).collect()
    }

    #[test]
    fn name_beats_terms_beats_welcome() {
        assert_eq!(names("mira"), ["Mira"]);
        assert_eq!(names("MIRA"), ["Mira"], "case does not matter");
        assert_eq!(names("ferry"), ["Tess"]);
        assert_eq!(names("tick"), ["Tess"], "prefixes match");
        // "software" is a search term word of Mira and only welcome text of the others.
        assert_eq!(names("software"), ["Mira", "Alex", "Tess"]);
    }

    #[test]
    fn people_and_organisations_share_one_list() {
        assert_eq!(names("alex"), ["Alex"]);
        assert_eq!(names("example"), ["Alex", "Mira", "Tess"], "Alex Example matches the owner name, the others the domain");
        assert_eq!(names(""), ["Mira", "Alex", "Tess"], "verified first, then by name");
        assert!(names("zeppelin").is_empty());
    }

    #[test]
    fn long_queries_count_only_their_first_words() {
        let long = format!("mira {}", "zeppelin ".repeat(10_000));
        assert_eq!(query_words(&long).len(), MAX_QUERY_WORDS);
        assert_eq!(query_words(&"a".repeat(5000))[0].len(), 40);
        assert_eq!(names(&long), ["Mira"]);
        // A ninth word no longer counts.
        assert!(names(&format!("{}tess", "x ".repeat(8))).is_empty());
    }

    #[test]
    fn more_matched_words_rank_first() {
        assert_eq!(names("eu data residency")[0], "Mira");
        assert_eq!(names("tickets harbour")[0], "Tess");
    }
}
