//! Fuzzy matching for the command box. A match is graded from an exact
//! match down to the letters of the query appearing in order, so "te"
//! finds "Terminal" before "Settings" and "gte" finds "GNOME Text Editor".

/// How well `query` matches `text`, from about 1000 (the same text) down to
/// about 100 (scattered letters in order). None when it does not match. An
/// empty query matches everything with 0.
pub fn score(query: &str, text: &str) -> Option<f32> {
    let q: Vec<char> = query.trim().chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0.0);
    }
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    if t.is_empty() || q.len() > t.len() {
        return None;
    }
    // How much of the text the query covers, so "Term" ranks a short
    // "Terminal" above a long "Terminal emulator settings".
    let cover = q.len() as f32 / t.len() as f32;
    if q == t {
        return Some(1000.0);
    }
    if t.starts_with(&q) {
        return Some(900.0 + 60.0 * cover);
    }
    let starts = word_starts(&t);
    if let Some((n, &at)) = starts.iter().enumerate().skip(1).find(|&(_, &at)| t[at..].starts_with(&q)) {
        return Some(800.0 + 60.0 * cover - 4.0 * n.min(10) as f32 - (at as f32 * 0.2).min(10.0));
    }
    if q.len() >= 2 && initials(&q, &t, &starts) {
        return Some(700.0 + 40.0 * (q.len() as f32 / starts.len() as f32));
    }
    if let Some(at) = find(&t, &q) {
        return Some(600.0 + 40.0 * cover - (at as f32).min(40.0));
    }
    subsequence(&q, &t, &starts)
}

/// The best of `score` against several texts, the first ones counting fully
/// and the later ones (keywords, comments) at a lower weight.
pub fn best(query: &str, texts: &[&str], hints: &[&str]) -> Option<f32> {
    let main = texts.iter().filter_map(|t| score(query, t)).fold(None, |m: Option<f32>, s| Some(m.map_or(s, |m| m.max(s))));
    let hint = hints
        .iter()
        .filter_map(|t| score(query, t))
        // Only real matches count in the hints, not scattered letters.
        .filter(|s| *s >= 600.0)
        .map(|s| s * 0.5)
        .fold(None, |m: Option<f32>, s| Some(m.map_or(s, |m| m.max(s))));
    match (main, hint) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Indexes where words begin: the first letter, letters after a space or
/// punctuation, and capitals after small letters in the original text are
/// not seen here because the text is lowercase already.
fn word_starts(t: &[char]) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, c) in t.iter().enumerate() {
        let after_break = i == 0 || !t[i - 1].is_alphanumeric();
        if c.is_alphanumeric() && after_break {
            out.push(i);
        }
    }
    out
}

/// Every query letter is the first letter of a word, in order.
fn initials(q: &[char], t: &[char], starts: &[usize]) -> bool {
    let mut words = starts.iter().map(|&i| t[i]);
    q.iter().all(|c| words.any(|w| w == *c))
}

fn find(t: &[char], q: &[char]) -> Option<usize> {
    (0..=t.len() - q.len()).find(|&i| t[i..i + q.len()] == *q)
}

/// The query letters in order anywhere in the text. Runs of neighbouring
/// letters and letters at word starts score higher, gaps lower.
fn subsequence(q: &[char], t: &[char], starts: &[usize]) -> Option<f32> {
    let mut score = 400.0f32;
    let mut at = 0;
    let mut last: Option<usize> = None;
    for c in q {
        let found = (at..t.len()).find(|&i| t[i] == *c)?;
        if let Some(prev) = last {
            let gap = found - prev - 1;
            if gap == 0 {
                score += 12.0;
            } else {
                score -= (gap as f32 * 6.0).min(60.0);
            }
        } else {
            score -= (found as f32 * 3.0).min(60.0);
        }
        if starts.binary_search(&found).is_ok() {
            score += 15.0;
        }
        last = Some(found);
        at = found + 1;
    }
    Some(score.clamp(100.0, 590.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked<'a>(query: &str, items: &[&'a str]) -> Vec<&'a str> {
        let mut scored: Vec<(f32, &str)> = items.iter().filter_map(|t| score(query, t).map(|s| (s, *t))).collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored.into_iter().map(|(_, t)| t).collect()
    }

    #[test]
    fn grades_go_from_exact_to_scattered() {
        let exact = score("terminal", "Terminal").unwrap();
        let prefix = score("term", "Terminal").unwrap();
        let word = score("edit", "Text Editor").unwrap();
        let initials = score("gte", "GNOME Text Editor").unwrap();
        let inside = score("min", "Terminal").unwrap();
        let scattered = score("tml", "Terminal").unwrap();
        assert!(
            exact > prefix && prefix > word && word > initials && initials > inside && inside > scattered,
            "{:?}",
            [exact, prefix, word, initials, inside, scattered]
        );
        assert_eq!(score("", "anything"), Some(0.0));
        assert_eq!(score("xyz", "Terminal"), None);
        assert_eq!(score("terminals", "Terminal"), None);
        assert_eq!(score("TeRm", "terminal"), score("term", "Terminal"), "case does not matter");
    }

    #[test]
    fn shorter_and_earlier_matches_rank_first() {
        assert_eq!(ranked("te", &["Settings", "Text Editor", "Terminal", "Activity"]), ["Terminal", "Text Editor"]);
        assert_eq!(ranked("fire", &["Firefox Web Browser", "Firewall", "Wildfire"]), ["Firewall", "Firefox Web Browser", "Wildfire"]);
        assert_eq!(ranked("calc", &["LibreOffice Calc", "Calculator"]), ["Calculator", "LibreOffice Calc"]);
        assert_eq!(ranked("brw", &["Browser", "Brightness Window"])[0], "Browser");
    }

    #[test]
    fn hints_count_less_than_names() {
        let name = best("web", &["Web"], &[]).unwrap();
        let hint = best("web", &["Firefox"], &["Web Browser"]).unwrap();
        assert!(name > hint);
        assert!(best("wr", &["Firefox"], &["Web Browser"]).is_none(), "scattered letters in a hint do not count");
        assert!(best("wb", &["Firefox"], &["Web Browser"]).is_some(), "the initials of a hint count");
        assert_eq!(best("fox", &["Firefox", "Fox"], &[]), score("fox", "Fox"));
    }
}
