//! The diff of one file in the Git app, editable on its working side.
//!
//! [`Doc`] holds the base (the index or HEAD) and the working lines as a
//! buffer you edit. Every edit aligns both again with a line diff, groups
//! the changes into hunks with [`CONTEXT`] lines around them and keeps the
//! line you edit visible. A hunk becomes a patch for `git apply` with
//! [`Doc::patch`], built from the same alignment that is drawn, so what you
//! stage is what you see.

use std::ops::Range;
use std::time::Duration;

use similar::{Algorithm, DiffTag, TextDiff};

/// Unchanged lines shown around a change.
pub const CONTEXT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Equal,
    /// Only in the base.
    Delete,
    /// Only in the working text.
    Insert,
}

/// One line of the alignment, in unified order: in a changed block the
/// deleted lines come before the inserted ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub tag: Tag,
    /// Line index in the base.
    pub old: Option<usize>,
    /// Line index in the working text.
    pub new: Option<usize>,
    /// The line it replaces or was replaced by on the other side, for the
    /// word-level highlights.
    pub partner: Option<usize>,
}

/// A run of [`Doc::lines`] to show, with its changes and their context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub lines: Range<usize>,
    /// False for the context kept around the line you edit, which has no
    /// change to stage.
    pub changes: bool,
}

/// A row of the side-by-side view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair {
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// Part of a change rather than unchanged context.
    pub changed: bool,
}

/// Lines with their line breaks.
pub fn split(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_string).collect()
}

/// A line without its line break.
pub fn content(line: &str) -> &str {
    line.trim_end_matches('\n').trim_end_matches('\r')
}

/// The line break of a line, empty for a last line without one.
pub fn ending(line: &str) -> &str {
    &line[content(line).len()..]
}

pub struct Doc {
    pub base: Vec<String>,
    /// The working text you edit, line by line with line breaks.
    pub work: Vec<String>,
    /// The working text as it is on disk.
    saved: String,
    pub lines: Vec<Line>,
    pub hunks: Vec<Hunk>,
    /// The working line that stays visible, the one you edit.
    keep: Option<usize>,
    dirty: bool,
}

impl Doc {
    pub fn new(base: &str, work: &str) -> Doc {
        let mut doc = Doc {
            base: split(base),
            work: split(work),
            saved: work.to_string(),
            lines: Vec::new(),
            hunks: Vec::new(),
            keep: None,
            dirty: false,
        };
        doc.rediff();
        doc
    }

    pub fn base_text(&self) -> String {
        self.base.concat()
    }

    pub fn work_text(&self) -> String {
        self.work.concat()
    }

    /// The working text as it is on disk.
    pub fn saved(&self) -> &str {
        &self.saved
    }

    /// The buffer differs from the file on disk.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// The buffer was written to disk.
    pub fn mark_saved(&mut self) {
        self.saved = self.work_text();
        self.dirty = false;
    }

    /// Back to the text on disk.
    pub fn drop_edits(&mut self) {
        self.work = split(&self.saved);
        self.keep = None;
        self.rediff();
    }

    /// Keeps working line `line` visible while you edit it.
    pub fn keep(&mut self, line: Option<usize>) {
        if self.keep != line {
            self.keep = line;
            self.hunks = group(&self.lines, line.and_then(|n| self.lines.iter().position(|l| l.new == Some(n))));
        }
    }

    /// Aligns base and buffer again.
    pub fn rediff(&mut self) {
        let (base, work) = (self.base_text(), self.work_text());
        self.dirty = work != self.saved;
        self.lines = align(&base, &work);
        self.keep = self.keep.filter(|n| *n < self.work.len());
        self.hunks = group(&self.lines, self.keep.and_then(|n| self.lines.iter().position(|l| l.new == Some(n))));
    }

    /// Hunks that hold a change.
    #[cfg(test)]
    pub fn changes(&self) -> usize {
        self.hunks.iter().filter(|h| h.changes).count()
    }

    /// Replaces the text of working line `line`, keeping its line break. Text
    /// with line breaks, as from a paste, becomes several lines. Returns the
    /// line the cursor ends on and its character column.
    pub fn set_line(&mut self, line: usize, text: &str) -> (usize, usize) {
        let Some(old) = self.work.get(line) else { return (line, 0) };
        let end = ending(old).to_string();
        let parts: Vec<&str> = text.split('\n').collect();
        let mut at = (line, text.chars().count());
        if parts.len() == 1 {
            self.work[line] = format!("{text}{end}");
        } else {
            let brk = if end.is_empty() { "\n".to_string() } else { end.clone() };
            let mut lines: Vec<String> = parts[..parts.len() - 1].iter().map(|p| format!("{}{brk}", p.trim_end_matches('\r'))).collect();
            let last = parts[parts.len() - 1];
            lines.push(format!("{last}{end}"));
            at = (line + lines.len() - 1, last.chars().count());
            self.work.splice(line..=line, lines);
        }
        self.rediff();
        at
    }

    /// Splits working line `line` at character `col`, as Enter does. Returns the new line.
    pub fn split_line(&mut self, line: usize, col: usize) -> usize {
        let Some(old) = self.work.get(line).cloned() else { return line };
        let text = content(&old);
        let at = text.char_indices().nth(col).map_or(text.len(), |(i, _)| i);
        let end = ending(&old);
        let brk = if end.is_empty() { "\n" } else { end };
        self.work[line] = format!("{}{brk}", &text[..at]);
        self.work.insert(line + 1, format!("{}{end}", &text[at..]));
        self.rediff();
        line + 1
    }

    /// Joins working line `line` with the one before, as Backspace at its
    /// start does. Returns that line and the column where they met.
    pub fn join_previous(&mut self, line: usize) -> Option<(usize, usize)> {
        if line == 0 || line >= self.work.len() {
            return None;
        }
        let this = self.work.remove(line);
        let prev = &mut self.work[line - 1];
        let col = content(prev).chars().count();
        *prev = format!("{}{this}", content(prev));
        self.rediff();
        Some((line - 1, col))
    }

    /// Joins working line `line` with the next one, as Delete at its end does.
    pub fn join_next(&mut self, line: usize) -> bool {
        if line + 1 >= self.work.len() {
            return false;
        }
        let next = self.work.remove(line + 1);
        let this = &mut self.work[line];
        *this = format!("{}{next}", content(this));
        self.rediff();
        true
    }

    /// The rows of hunk `h` side by side: unchanged lines next to each
    /// other, a deleted line next to the inserted line that replaced it.
    pub fn pairs(&self, h: usize) -> Vec<Pair> {
        let Some(hunk) = self.hunks.get(h) else { return Vec::new() };
        let lines = &self.lines[hunk.lines.clone()];
        let mut out = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            match lines[i].tag {
                Tag::Equal => {
                    out.push(Pair { old: lines[i].old, new: lines[i].new, changed: false });
                    i += 1;
                }
                _ => {
                    let deleted: Vec<usize> = lines[i..].iter().take_while(|l| l.tag == Tag::Delete).filter_map(|l| l.old).collect();
                    i += deleted.len();
                    let inserted: Vec<usize> = lines[i..].iter().take_while(|l| l.tag == Tag::Insert).filter_map(|l| l.new).collect();
                    i += inserted.len();
                    for k in 0..deleted.len().max(inserted.len()) {
                        out.push(Pair { old: deleted.get(k).copied(), new: inserted.get(k).copied(), changed: true });
                    }
                }
            }
        }
        out
    }

    /// The hunk header, `@@ -12,7 +12,9 @@`.
    pub fn header(&self, h: usize) -> String {
        let ((os, ol), (ns, nl)) = self.ranges(h);
        format!("@@ -{os},{ol} +{ns},{nl} @@")
    }

    /// Start and length of hunk `h` in base and working text, as a unified
    /// diff counts them: from 1, and the line before for an empty range.
    fn ranges(&self, h: usize) -> ((usize, usize), (usize, usize)) {
        let lines = self.hunks.get(h).map_or(&[][..], |hunk| &self.lines[hunk.lines.clone()]);
        let count = |pick: fn(&Line) -> Option<usize>| {
            let idx: Vec<usize> = lines.iter().filter_map(pick).collect();
            match idx.first() {
                Some(first) => (first + 1, idx.len()),
                None => (0, 0),
            }
        };
        let (mut old, mut new) = (count(|l| l.old), count(|l| l.new));
        // An empty side starts after the line before the hunk.
        if old.1 == 0 {
            old.0 = lines.iter().find_map(|l| l.new).map_or(0, |n| self.old_before(n));
        }
        if new.1 == 0 {
            new.0 = lines.iter().find_map(|l| l.old).map_or(0, |o| self.new_before(o));
        }
        (old, new)
    }

    /// How many base lines come before working line `new`.
    fn old_before(&self, new: usize) -> usize {
        self.lines.iter().take_while(|l| l.new != Some(new)).filter(|l| l.old.is_some()).count()
    }

    fn new_before(&self, old: usize) -> usize {
        self.lines.iter().take_while(|l| l.old != Some(old)).filter(|l| l.new.is_some()).count()
    }

    /// Hunk `h` as a patch of `path` that `git apply` takes. None for a hunk
    /// without changes and while the buffer has unsaved edits, because the
    /// patch must match the file on disk.
    pub fn patch(&self, h: usize, path: &str) -> Option<String> {
        let hunk = self.hunks.get(h).filter(|hunk| hunk.changes && !self.dirty)?;
        let quoted = quote_path(path);
        let mut out = format!("diff --git a/{quoted} b/{quoted}\n--- a/{quoted}\n+++ b/{quoted}\n{}\n", self.header(h));
        for line in &self.lines[hunk.lines.clone()] {
            let (sign, text) = match line.tag {
                Tag::Equal => (' ', &self.base[line.old?]),
                Tag::Delete => ('-', &self.base[line.old?]),
                Tag::Insert => ('+', &self.work[line.new?]),
            };
            out.push(sign);
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
        Some(out)
    }
}

/// The alignment of `base` and `work` line by line.
pub fn align(base: &str, work: &str) -> Vec<Line> {
    let diff = TextDiff::configure().algorithm(Algorithm::Myers).timeout(Duration::from_millis(200)).diff_lines(base, work);
    let mut out = Vec::new();
    for op in diff.ops() {
        let (tag, old, new) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => out.extend(old.zip(new).map(|(o, n)| Line { tag: Tag::Equal, old: Some(o), new: Some(n), partner: None })),
            DiffTag::Delete => out.extend(old.map(|o| Line { tag: Tag::Delete, old: Some(o), new: None, partner: None })),
            DiffTag::Insert => out.extend(new.map(|n| Line { tag: Tag::Insert, old: None, new: Some(n), partner: None })),
            DiffTag::Replace => {
                let (olds, news): (Vec<usize>, Vec<usize>) = (old.collect(), new.collect());
                out.extend(olds.iter().enumerate().map(|(k, o)| Line {
                    tag: Tag::Delete,
                    old: Some(*o),
                    new: None,
                    partner: news.get(k).copied(),
                }));
                out.extend(news.iter().enumerate().map(|(k, n)| Line {
                    tag: Tag::Insert,
                    old: None,
                    new: Some(*n),
                    partner: olds.get(k).copied(),
                }));
            }
        }
    }
    out
}

/// Groups the changes of `lines` into hunks with [`CONTEXT`] lines around
/// them. Hunks whose context touches are joined. `keep`, an index into
/// `lines`, stays visible even without a change near it.
pub fn group(lines: &[Line], keep: Option<usize>) -> Vec<Hunk> {
    let mut marks: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.tag != Tag::Equal).map(|(i, _)| i).collect();
    if let Some(k) = keep.filter(|k| *k < lines.len()) {
        let at = marks.partition_point(|m| *m < k);
        if marks.get(at) != Some(&k) {
            marks.insert(at, k);
        }
    }
    let mut out: Vec<Range<usize>> = Vec::new();
    for m in marks {
        let (start, end) = (m.saturating_sub(CONTEXT), (m + CONTEXT + 1).min(lines.len()));
        match out.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => out.push(start..end),
        }
    }
    out.into_iter().map(|r| Hunk { changes: lines[r.clone()].iter().any(|l| l.tag != Tag::Equal), lines: r }).collect()
}

/// A path in a patch header, C-quoted the way git does when it needs to be.
fn quote_path(path: &str) -> String {
    if !path.chars().any(|c| matches!(c, '"' | '\\' | '\t' | '\n') || c.is_control()) {
        return path.to_string();
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\n";

    #[test]
    fn hunks_have_context_and_join_when_close() {
        let doc = Doc::new(BASE, "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nK\nl\nm\n");
        assert_eq!(doc.hunks.len(), 2, "b and k are far apart");
        assert_eq!(doc.header(0), "@@ -1,5 +1,5 @@");
        assert_eq!(doc.header(1), "@@ -8,6 +8,6 @@");
        let close = Doc::new(BASE, "a\nB\nc\nd\ne\nF\ng\nh\ni\nj\nk\nl\nm\n");
        assert_eq!(close.hunks.len(), 1, "contexts that touch join");
        let same = Doc::new(BASE, BASE);
        assert!(same.hunks.is_empty() && !same.dirty());
    }

    #[test]
    fn side_by_side_pairs_replacements() {
        let doc = Doc::new("x\nold one\nold two\ny\n", "x\nnew one\ny\nadded\n");
        let pairs = doc.pairs(0);
        assert_eq!(
            pairs,
            [
                Pair { old: Some(0), new: Some(0), changed: false },
                Pair { old: Some(1), new: Some(1), changed: true },
                Pair { old: Some(2), new: None, changed: true },
                Pair { old: Some(3), new: Some(2), changed: false },
                Pair { old: None, new: Some(3), changed: true },
            ]
        );
        let replaced = doc.lines.iter().find(|l| l.old == Some(1)).unwrap();
        assert_eq!((replaced.tag, replaced.partner), (Tag::Delete, Some(1)), "for the word highlights");
    }

    #[test]
    fn editing_lines_keeps_breaks_and_realigns() {
        let mut doc = Doc::new(BASE, BASE);
        doc.keep(Some(6));
        assert_eq!(doc.hunks, [Hunk { lines: 3..10, changes: false }], "the edited line stays visible without a change");
        assert_eq!(doc.set_line(6, "G"), (6, 1));
        assert!(doc.dirty() && doc.changes() == 1);
        assert_eq!(doc.work[6], "G\n");
        let next = doc.split_line(6, 1);
        assert_eq!((next, doc.work[6].as_str(), doc.work[7].as_str()), (7, "G\n", "\n"));
        assert_eq!(doc.join_previous(7), Some((6, 1)));
        assert_eq!(doc.work[6], "G\n");
        assert!(doc.join_next(6));
        assert_eq!(doc.work[6], "Gh\n");
        assert_eq!(doc.set_line(6, "one\ntwo"), (7, 3), "a paste with a line break makes two lines");
        assert_eq!((doc.work[6].as_str(), doc.work[7].as_str()), ("one\n", "two\n"));
        doc.drop_edits();
        assert_eq!(doc.work_text(), BASE);
        assert!(!doc.dirty() && doc.hunks.is_empty());
        let mut crlf = Doc::new("a\r\nb\r\n", "a\r\nb\r\n");
        crlf.set_line(0, "A");
        assert_eq!(crlf.work_text(), "A\r\nb\r\n", "Windows line breaks stay");
    }

    #[test]
    fn patches_match_what_is_shown() {
        let doc = Doc::new(BASE, "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn");
        assert_eq!(
            doc.patch(0, "x.txt").unwrap(),
            "diff --git a/x.txt b/x.txt\n--- a/x.txt\n+++ b/x.txt\n@@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n d\n e\n"
        );
        assert!(doc.patch(1, "x.txt").unwrap().ends_with(" m\n+n\n\\ No newline at end of file\n"));
        let mut edited = Doc::new(BASE, BASE);
        edited.set_line(0, "A");
        assert_eq!(edited.patch(0, "x.txt"), None, "no patch while edits are unsaved");
        edited.mark_saved();
        assert!(edited.patch(0, "x.txt").is_some());
        let new_file = Doc::new("", "one\ntwo\n");
        assert_eq!(new_file.header(0), "@@ -0,0 +1,2 @@");
        let emptied = Doc::new("one\n", "");
        assert_eq!(emptied.header(0), "@@ -1,1 +0,0 @@");
        assert_eq!(quote_path("a b.txt"), "a b.txt");
        assert_eq!(quote_path("tab\there.txt"), "\"tab\\there.txt\"");
    }
}
