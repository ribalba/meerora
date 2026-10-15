//! The sample agents in `assets/agents` must always build, stay within their
//! budget and keep roughly the heights of the mockups.

use std::path::PathBuf;

use meerora_script::{Limits, Motion, ParamKind, build_figure};

fn agent(id: &str) -> meerora_script::FigureDesc {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/agents").join(id).join("figure.js");
    let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    build_figure(&source, &format!("{id}/figure.js"), &Limits::default()).unwrap_or_else(|e| panic!("{id}: {e}"))
}

#[test]
fn sample_agents_build() {
    for (id, height) in [("rivet", 110.0), ("ember", 95.0), ("claude", 80.0), ("quill", 90.0), ("moth", 100.0), ("wren", 95.0)] {
        let f = agent(id);
        assert!(f.triangles <= f.budget && f.budget == 3000, "{id}: {} of {} triangles", f.triangles, f.budget);
        let b = f.bounds;
        assert!(b.min.y.abs() < 0.5, "{id}: feet at y = {}", b.min.y);
        assert!((b.max.y - height).abs() < 8.0, "{id}: {} cm tall, expected about {height}", b.max.y);
        let c = f.base.bounds().center();
        assert!(c.x.abs() < 6.0 && c.z.abs() < 8.0, "{id}: not standing on the origin, centre {c}");
        assert!(f.ring_anchor.y > b.max.y, "{id}: ring at {} inside the figure", f.ring_anchor.y);
        assert!(f.face_anchor.z > 8.0, "{id}: face anchor {} does not look forward", f.face_anchor);
    }
}

#[test]
fn wren_raises_a_letter() {
    let f = agent("wren");
    assert_eq!(f.params.len(), 1);
    assert_eq!(f.params[0].kind, ParamKind::Toggle { default: false });
    let letter = &f.param_parts[0];
    assert_eq!(letter.param, "letter");
    assert_eq!(letter.anim, Some(Motion::Wave { period: 1.5, angle_deg: 8.0 }));
    // The raised arm is on Wren's left side (-x) and above the shoulder.
    let b = letter.mesh.bounds();
    assert!(b.max.x < 0.0 && b.max.y > letter.pivot.y + 20.0, "letter bounds {b:?}");
    assert!(f.base.bounds().max.y < b.max.y + 20.0);
}
