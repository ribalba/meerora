//! The params an agent may set on its own figure (D24). Figures are built
//! with the script host and cached per file until the file changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use meerora_script::{Limits, ParamDef, ParamKind, build_figure};
use serde_json::Value;

/// Longest value of a `text` param.
const MAX_TEXT: usize = 120;

/// The params of one figure file, with the modification time they were read at.
type Entry = (Option<SystemTime>, Result<Vec<ParamDef>, String>);

#[derive(Default)]
pub struct FigureCache {
    entries: HashMap<PathBuf, Entry>,
}

impl FigureCache {
    /// The params declared by the figure script at `path`.
    pub fn params(&mut self, path: &Path) -> Result<&[ParamDef], String> {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let fresh = self.entries.get(path).is_some_and(|(stamp, _)| stamp.is_some() && *stamp == modified);
        if !fresh {
            self.entries.insert(path.to_path_buf(), (modified, load(path)));
        }
        match &self.entries[path].1 {
            Ok(params) => Ok(params),
            Err(e) => Err(e.clone()),
        }
    }

    /// Checks that the figure at `path` declares `name` and that `value` fits it.
    pub fn check(&mut self, path: &Path, name: &str, value: &Value) -> Result<(), String> {
        let params = self.params(path)?;
        let Some(def) = params.iter().find(|p| p.name == name) else {
            if params.is_empty() {
                return Err(format!("your figure declares no params, so {name:?} cannot be set"));
            }
            let names: Vec<String> = params.iter().map(describe).collect();
            return Err(format!("your figure has no param {name:?}. It declares these: {}", names.join(", ")));
        };
        check_value(def, value)
    }
}

fn load(path: &Path) -> Result<Vec<ParamDef>, String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("cannot read your figure {} ({e})", path.display()))?;
    build_figure(&source, &path.display().to_string(), &Limits::default())
        .map(|figure| figure.params)
        .map_err(|e| format!("your figure does not build ({e})"))
}

fn describe(def: &ParamDef) -> String {
    let kind = match &def.kind {
        ParamKind::Toggle { .. } => "true or false".to_string(),
        ParamKind::Choice { values, .. } => format!("one of {}", values.join(", ")),
        ParamKind::Number { min, max, .. } => format!("a number from {min} to {max}"),
        ParamKind::Count { .. } => "a whole number from 0".to_string(),
        ParamKind::Text { .. } => format!("a text of up to {MAX_TEXT} characters"),
    };
    format!("{} ({kind})", def.name)
}

/// Whether `value` is valid for the param.
pub fn check_value(def: &ParamDef, value: &Value) -> Result<(), String> {
    let ok = match &def.kind {
        ParamKind::Toggle { .. } => value.is_boolean(),
        ParamKind::Choice { values, .. } => value.as_str().is_some_and(|v| values.iter().any(|c| c == v)),
        ParamKind::Number { min, max, .. } => value.as_f64().is_some_and(|v| (*min..=*max).contains(&v)),
        ParamKind::Count { .. } => value.as_u64().is_some_and(|v| v <= u64::from(u32::MAX)),
        ParamKind::Text { .. } => value.as_str().is_some_and(|v| v.chars().count() <= MAX_TEXT),
    };
    if ok { Ok(()) } else { Err(format!("{value} does not fit {}", describe(def))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::TempDir;
    use serde_json::json;

    const WREN: &str = include_str!("../../../assets/agents/wren/figure.js");

    #[test]
    fn validates_against_the_figure_and_rebuilds_on_change() {
        let dir = TempDir::new("figures");
        let path = dir.0.join("figure.js");
        std::fs::write(&path, WREN).unwrap();
        let mut cache = FigureCache::default();
        assert!(cache.check(&path, "letter", &json!(true)).is_ok());
        let err = cache.check(&path, "letter", &json!("yes")).unwrap_err();
        assert!(err.contains("true or false"), "{err}");
        let err = cache.check(&path, "hat", &json!(true)).unwrap_err();
        assert!(err.contains("letter (true or false)"), "{err}");

        // A broken figure is reported once its file changes.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "export default figure({});").unwrap();
        let err = cache.check(&path, "letter", &json!(true)).unwrap_err();
        assert!(err.contains("does not build"), "{err}");
    }

    #[test]
    fn checks_every_kind() {
        let def = |kind| ParamDef { name: "p".into(), kind };
        let choice = def(ParamKind::Choice { values: vec!["a".into(), "b".into()], default: "a".into() });
        assert!(check_value(&choice, &json!("b")).is_ok());
        assert!(check_value(&choice, &json!("c")).is_err());
        let number = def(ParamKind::Number { min: 0.0, max: 1.0, default: 0.5 });
        assert!(check_value(&number, &json!(0.25)).is_ok());
        assert!(check_value(&number, &json!(2)).is_err());
        let count = def(ParamKind::Count { default: 0 });
        assert!(check_value(&count, &json!(3)).is_ok());
        assert!(check_value(&count, &json!(-1)).is_err());
        let text = def(ParamKind::Text { default: String::new() });
        assert!(check_value(&text, &json!("short")).is_ok());
        assert!(check_value(&text, &json!("x".repeat(200))).is_err());
    }
}
