//! BUILDER.md is embedded into the builder's prompt. Its example scripts must
//! build and the guide must stay short.

use meerora_script::{Limits, build_figure};

const GUIDE: &str = include_str!("../BUILDER.md");

fn js_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        match (&mut current, line.trim_end()) {
            (None, "```js") => current = Some(String::new()),
            (Some(block), "```") => {
                blocks.push(std::mem::take(block));
                current = None;
            }
            (Some(block), _) => {
                block.push_str(line);
                block.push('\n');
            }
            (None, _) => {}
        }
    }
    assert!(current.is_none(), "unclosed ```js block");
    blocks
}

#[test]
fn guide_examples_build() {
    let figures: Vec<String> = js_blocks(GUIDE).into_iter().filter(|b| b.contains("export default figure(")).collect();
    assert!(!figures.is_empty(), "BUILDER.md has no complete example");
    for (i, source) in figures.iter().enumerate() {
        let f = build_figure(source, &format!("builder-example-{i}.js"), &Limits::default()).unwrap_or_else(|e| panic!("example {i}: {e}"));
        assert!(f.triangles <= f.budget);
        assert!(f.bounds.min.y.abs() < 0.5 && f.ring_anchor.y > f.bounds.max.y);
        assert!(!f.params.is_empty(), "example {i} should show a param");
    }
}

#[test]
fn guide_stays_short() {
    let lines = GUIDE.lines().count();
    assert!(lines <= 250, "BUILDER.md has {lines} lines");
    assert!(!GUIDE.contains('\u{2014}'), "BUILDER.md uses an em dash");
}
