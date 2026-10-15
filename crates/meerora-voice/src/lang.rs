//! Which language a reply is in, to pick a Piper voice, and the text of a
//! Markdown reply as it should be spoken.

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

use crate::settings::VoiceSettings;

/// Short German words that are rare in English.
const GERMAN: &[&str] = &[
    "der", "die", "das", "und", "ist", "nicht", "ich", "du", "sie", "wir", "ihr", "ein", "eine", "einen", "einem", "einer", "mit", "auf",
    "für", "von", "zu", "zum", "zur", "den", "dem", "des", "sich", "auch", "noch", "nur", "schon", "wie", "wenn", "aber", "oder", "doch",
    "bitte", "danke", "ja", "nein", "hier", "jetzt", "dann", "kann", "kannst", "muss", "wird", "wurde", "sind", "bin", "bist", "habe",
    "hast", "haben", "gibt", "mal", "keine", "kein", "über", "unter", "nach", "bei", "aus", "dass", "weil", "heute", "morgen", "gerne",
    "ob", "sehr", "mehr", "etwas", "alles", "welche", "diese", "dieser", "dieses", "mich", "mir", "dir", "dich", "uns", "euch",
];

/// Short English words that are rare in German.
const ENGLISH: &[&str] = &[
    "the", "and", "is", "are", "were", "not", "you", "your", "it", "its", "this", "that", "with", "for", "of", "to", "on", "be", "have",
    "has", "do", "does", "did", "what", "when", "can", "would", "should", "could", "there", "their", "they", "we", "i", "a", "an", "my",
    "me", "but", "or", "if", "from", "at", "by", "as", "about", "which", "how", "please", "thanks", "yes", "no", "here", "now", "then",
    "just", "all", "some", "any", "more", "been", "will", "into", "out", "up", "our", "us", "them", "these", "those", "done", "file",
];

/// The language of `text`, `de` or `en`, by counting typical short words and
/// umlauts. `None` when the text gives no clear answer.
pub fn detect(text: &str) -> Option<&'static str> {
    let (mut de, mut en) = (0usize, 0usize);
    for word in text.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()) {
        let w = word.to_lowercase();
        if w.chars().any(|c| matches!(c, 'ä' | 'ö' | 'ü' | 'ß')) {
            de += 2;
        }
        if GERMAN.contains(&w.as_str()) {
            de += 1;
        }
        if ENGLISH.contains(&w.as_str()) {
            en += 1;
        }
    }
    match de.cmp(&en) {
        std::cmp::Ordering::Greater => Some("de"),
        std::cmp::Ordering::Less => Some("en"),
        std::cmp::Ordering::Equal => None,
    }
}

/// The Piper voice for a reply of `agent`. That is its fixed voice, else the
/// voice for the detected language, else the voice for the spoken language.
pub fn choose_voice(settings: &VoiceSettings, agent: Option<&str>, text: &str) -> String {
    if let Some(voice) = agent.and_then(|a| settings.agent_voice(a)) {
        return voice.to_string();
    }
    let language = detect(text).or(settings.spoken_language()).unwrap_or("en");
    settings.voice_for_language(language)
}

/// The words of a Markdown reply as they should be spoken. Code blocks and
/// link targets are left out, bare URLs are said as "a link", and every
/// paragraph or list item gets its own line. Long replies stop after the sentence that passes `max_chars`.
pub fn speakable(markdown: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut line = String::new();
    let mut in_code = 0usize;
    let mut skip_image = 0usize;
    let flush = |line: &mut String, out: &mut String| {
        let text = line.split_whitespace().collect::<Vec<_>>().join(" ");
        line.clear();
        if text.is_empty() {
            return;
        }
        out.push_str(&text);
        if !text.ends_with(['.', '!', '?', ':', ';']) {
            out.push('.');
        }
        out.push('\n');
    };
    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code += 1,
            Event::End(TagEnd::CodeBlock) => in_code = in_code.saturating_sub(1),
            Event::Start(Tag::Image { .. }) => skip_image += 1,
            Event::End(TagEnd::Image) => skip_image = skip_image.saturating_sub(1),
            Event::Text(t) | Event::Code(t) if in_code == 0 && skip_image == 0 => {
                line.push_str(&without_urls(&t));
            }
            Event::SoftBreak => line.push(' '),
            Event::HardBreak => flush(&mut line, &mut out),
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::TableCell | TagEnd::BlockQuote(_)) => {
                flush(&mut line, &mut out)
            }
            _ => {}
        }
        if out.len() >= max_chars {
            break;
        }
    }
    flush(&mut line, &mut out);
    if out.len() > max_chars {
        // Cut after the last sentence end before the limit, else at a word.
        let cut = out.char_indices().take_while(|(i, _)| *i < max_chars).filter(|(_, c)| matches!(c, '.' | '!' | '?' | '\n')).last();
        let end = match cut {
            Some((i, _)) => i + 1,
            None => out.char_indices().take_while(|(i, _)| *i < max_chars).filter(|(_, c)| c.is_whitespace()).last().map_or(0, |(i, _)| i),
        };
        out.truncate(end);
    }
    out.trim().to_string()
}

/// Replaces bare URLs with "a link".
fn without_urls(text: &str) -> String {
    text.split(' ').map(|w| if w.starts_with("http://") || w.starts_with("https://") { "a link" } else { w }).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AgentVoice, DEFAULT_DE, DEFAULT_EN};

    #[test]
    fn detects_german_and_english() {
        assert_eq!(detect("Ich habe die Datei geändert und die Tests laufen jetzt."), Some("de"));
        assert_eq!(detect("I changed the file and the tests pass now."), Some("en"));
        assert_eq!(detect("Grüße"), Some("de"));
        assert_eq!(detect("OK"), None);
        assert_eq!(detect(""), None);
    }

    #[test]
    fn mixed_text_follows_the_majority() {
        assert_eq!(detect("Der Build ist fertig, alle tests sind grün."), Some("de"));
        assert_eq!(detect("The build is done, I pushed it to the main branch."), Some("en"));
    }

    #[test]
    fn voice_follows_the_language_unless_the_agent_has_one() {
        let mut s = VoiceSettings::default();
        assert_eq!(choose_voice(&s, Some("claude"), "Das ist erledigt, die Datei liegt im Ordner."), DEFAULT_DE);
        assert_eq!(choose_voice(&s, Some("claude"), "That is done, the file is in the folder."), DEFAULT_EN);
        // Unclear text uses the spoken language, then English.
        assert_eq!(choose_voice(&s, None, "OK"), DEFAULT_EN);
        s.language = "de".into();
        assert_eq!(choose_voice(&s, None, "OK"), DEFAULT_DE);
        s.agents.insert("wren".into(), AgentVoice { speak_replies: None, voice: Some("en_GB-alan-medium".into()) });
        assert_eq!(choose_voice(&s, Some("wren"), "Das ist erledigt."), "en_GB-alan-medium");
    }

    #[test]
    fn speakable_drops_code_and_link_targets() {
        let md = "# Done\n\nI fixed `main.rs`, see [the docs](https://example.org/x).\n\n```rust\nfn main() {}\n```\n\n- first item\n- second item\n\nMore at https://example.org/y";
        assert_eq!(speakable(md, 1000), "Done.\nI fixed main.rs, see the docs.\nfirst item.\nsecond item.\nMore at a link.");
    }

    #[test]
    fn speakable_cuts_long_text_at_a_sentence() {
        let md = "One sentence here. Another sentence follows. And a third one that is long.";
        assert_eq!(speakable(md, 30), "One sentence here.");
        assert_eq!(speakable("no sentence end in this long line of words", 20), "no sentence end in");
        assert_eq!(speakable("```\nonly code\n```", 100), "");
    }
}
