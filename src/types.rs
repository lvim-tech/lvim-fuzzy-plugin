//! File TYPES for plugins that choose an engine by what a file is (lvim-fuzzy-plugin-preview, lvim-fuzzy-plugin-open):
//! the plugin's `[config.types.<group>]` tables say which files a group is (`ext`, `name`, `mime`, `kind`) and which
//! `engine` it gets. One implementation, so every such plugin reads the same configuration the same way.
//!
//! A group matches when every key it gives holds (inside a list, any one will do): `ext` (no dot, `"*"` = any),
//! `name` (the file name), `mime` (`"image/*"`, `"application/pdf"`), `kind` (`"dir"`, `"binary"` = a NUL in the first
//! 8 KB, `"text"`). The most specific group wins: one with `ext` / `name` / `mime`, then one with `kind`, then the
//! higher `priority`, then the name.

use std::path::Path;

use serde_json::Value;

use crate::FileInfo;

/// One `[config.types.<group>]` table: which files it is and the engine it gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub ext: Option<Vec<String>>,
    pub names: Option<Vec<String>>,
    pub mime: Option<Vec<String>>,
    pub kind: Option<Vec<String>>,
    pub priority: i64,
    pub engine: String,
}

impl Group {
    /// The order groups are tried in: one that names files (`ext` / `name` / `mime`) first, then one with a `kind`,
    /// then the higher `priority`, then the name.
    fn rank(&self) -> (bool, bool, i64) {
        (self.ext.is_some() || self.names.is_some() || self.mime.is_some(), self.kind.is_some(), self.priority)
    }

    /// What the group matches, as one line: `ext png, jpg · mime image/*`; `(everything)` when it has no key.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (label, v) in [("ext", &self.ext), ("name", &self.names), ("mime", &self.mime), ("kind", &self.kind)] {
            if let Some(v) = v {
                parts.push(format!("{label} {}", v.join(", ")));
            }
        }
        if parts.is_empty() { "(everything)".to_string() } else { parts.join(" · ") }
    }
}

/// The `[config.types.*]` groups of a plugin's configuration, best first (the order [`engine_for`] tries them in).
pub fn groups(cfg: &Value) -> Vec<Group> {
    let list = |g: &Value, k: &str| -> Option<Vec<String>> {
        g.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim_start_matches('.').to_ascii_lowercase()).collect())
    };
    let mut out: Vec<Group> = cfg["types"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, g)| {
            Some(Group {
                name: name.clone(),
                ext: list(g, "ext"),
                names: list(g, "name"),
                mime: list(g, "mime"),
                kind: list(g, "kind"),
                priority: g["priority"].as_i64().unwrap_or(0),
                engine: g["engine"].as_str()?.to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.rank().cmp(&a.rank()).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The type group `path` belongs to and its engine, or None (the file is not there, or no group matches).
pub fn engine_for(path: &Path, cfg: &Value) -> Option<(String, String)> {
    if !path.exists() {
        return None;
    }
    let all = groups(cfg);
    let info = FileInfo::of(path);
    // the MIME type is worked out only when a group asks for one
    let mime = all.iter().any(|g| g.mime.is_some()).then(|| mime_of(path, &info.ext));
    all.into_iter()
        .find(|g| {
            g.ext.as_ref().is_none_or(|v| v.iter().any(|e| e == "*" || *e == info.ext))
                && g.names.as_ref().is_none_or(|v| v.contains(&info.name))
                && g.kind.as_ref().is_none_or(|v| v.iter().any(|k| *k == info.kind))
                && g.mime.as_ref().is_none_or(|v| mime.as_deref().is_some_and(|m| v.iter().any(|p| mime_matches(p, m))))
        })
        .map(|g| (g.name, g.engine))
}

/// The effective type → engine map as text, one group a line, best first: what it matches, the engine, and — for an
/// engine that runs a program — whether that program is installed. `builtin` are the engines that need no
/// definition, each with the program it runs, if any (`("system", Some("xdg-open"))`); `have` says whether a
/// program is installed. A group naming an engine that exists nowhere is marked.
/// The engines a plugin says need no definition (`builtin_engines` of its `[config]`: `[name, program or ""]` each).
pub fn builtin_engines(cfg: &Value) -> Vec<(String, Option<String>)> {
    cfg["builtin_engines"]
        .as_array()
        .map(|a| a.iter().filter_map(|e| Some((e.get(0)?.as_str()?.to_string(), e.get(1).and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_string)))).collect())
        .unwrap_or_default()
}

pub fn list(cfg: &Value, builtin: &[(&str, Option<&str>)], have: &dyn Fn(&str) -> bool) -> String {
    let all = groups(cfg);
    let name_w = all.iter().map(|g| g.name.chars().count()).max().unwrap_or(0);
    let match_w = all.iter().map(|g| g.describe().chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for g in &all {
        let (what, program) = if let Some((_, p)) = builtin.iter().find(|(n, _)| *n == g.engine) {
            (g.engine.clone(), p.map(str::to_string))
        } else if let Some(e) = cfg["engines"][g.engine.as_str()].as_object() {
            let cmd = e.get("command").and_then(Value::as_str).unwrap_or("");
            (format!("{} (command: {cmd})", g.engine), cmd.split_whitespace().next().map(str::to_string))
        } else {
            (format!("{} (no such engine)", g.engine), None)
        };
        let mark = match &program {
            _ if cfg["engines"][g.engine.as_str()].is_null() && !builtin.iter().any(|(n, _)| *n == g.engine) => "  ✗ not defined",
            Some(p) if have(p) => "  ✓",
            Some(_) => "  ✗ not installed",
            None => "",
        };
        out.push_str(&format!("{:name_w$}  {:match_w$}  → {what}{mark}\n", g.name, g.describe()));
    }
    if out.is_empty() {
        out.push_str("(no types: nothing is matched)\n");
    }
    out
}

/// `image/*` and `application/pdf` against a MIME type.
pub fn mime_matches(pattern: &str, mime: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => mime.split('/').next() == Some(prefix),
        None => pattern == mime,
    }
}

/// The MIME type of a file: from its extension when that is known, else asked of `file --mime-type`.
pub fn mime_of(path: &Path, ext: &str) -> String {
    let known = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "ico" => "image/vnd.microsoft.icon",
        "svg" => "image/svg+xml",
        "heic" | "heif" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    std::process::Command::new("file")
        .args(["-b", "--mime-type", "--"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// `'quoted'` for `sh -c`.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A command with the file in it: `{}` replaced by the quoted path, or the quoted path appended.
pub fn expand(cmd: &str, path: &Path) -> String {
    let q = quote(&path.to_string_lossy());
    if cmd.contains("{}") { cmd.replace("{}", &q) } else { format!("{cmd} {q}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lvim-types-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_most_specific_group_wins() {
        let d = tmp("pick");
        let (rs, png, bin) = (d.join("a.rs"), d.join("p.png"), d.join("b.dat"));
        std::fs::write(&rs, "fn main() {}").unwrap();
        std::fs::write(&png, [0x89, b'P', 0, 1]).unwrap();
        std::fs::write(&bin, [1u8, 0, 2]).unwrap();
        let cfg = json!({"types": {
            "anything": {"engine": "any", "priority": -10},
            "text": {"kind": ["text"], "engine": "t"},
            "binary": {"kind": ["binary"], "engine": "b"},
            "rust": {"ext": ["rs"], "engine": "r"},
            "images": {"mime": ["image/*"], "engine": "i"},
        }});
        assert_eq!(engine_for(&rs, &cfg).unwrap().1, "r"); // ext beats kind
        assert_eq!(engine_for(&png, &cfg).unwrap().1, "i"); // mime beats kind (png has a NUL: binary)
        assert_eq!(engine_for(&bin, &cfg).unwrap().1, "b"); // kind beats the catch-all
        let only_any = json!({"types": {"anything": {"engine": "any"}}});
        assert_eq!(engine_for(&rs, &only_any).unwrap().1, "any");
        // priority, then name, between two equal groups
        let two = json!({"types": {"a": {"ext": ["rs"], "engine": "x"}, "b": {"ext": ["rs"], "engine": "y", "priority": 5}}});
        assert_eq!(engine_for(&rs, &two).unwrap().1, "y");
        // not there, no types, no match
        assert!(engine_for(&d.join("none.rs"), &cfg).is_none());
        assert!(engine_for(&rs, &json!({})).is_none());
        assert!(engine_for(&rs, &json!({"types": {"x": {"ext": ["py"], "engine": "p"}}})).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_map_is_listed_with_what_is_missing() {
        let cfg = json!({
            "types": {
                "other": {"engine": "system"},
                "text": {"kind": ["text"], "engine": "editor"},
                "pics": {"mime": ["image/*"], "ext": ["png"], "engine": "imv"},
                "pdf": {"ext": [".PDF"], "engine": "reader"},
                "odd": {"name": ["makefile"], "engine": "ghost"},
            },
            "engines": {"imv": {"kind": "command", "command": "imv {}"}, "reader": {"kind": "command", "command": "zathura"}},
        });
        let have = |p: &str| p == "imv" || p == "xdg-open";
        let out = list(&cfg, &[("editor", None), ("system", Some("xdg-open"))], &have);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        // best first: the groups that name files, then kind, then the catch-all
        assert!(lines[0].starts_with("odd") && lines[0].contains("name makefile") && lines[0].contains("✗ not defined"), "{out}");
        assert!(lines[1].starts_with("pdf") && lines[1].contains("ext pdf") && lines[1].contains("✗ not installed"), "{out}");
        assert!(lines[2].starts_with("pics") && lines[2].contains("ext png · mime image/*") && lines[2].contains("(command: imv {})  ✓"), "{out}");
        assert!(lines[3].starts_with("text") && lines[3].contains("→ editor") && !lines[3].contains('✗'), "{out}");
        assert!(lines[4].starts_with("other") && lines[4].contains("(everything)") && lines[4].contains("→ system  ✓"), "{out}");
        assert!(list(&json!({}), &[], &have).contains("no types"));
    }

    #[test]
    fn mime_and_commands() {
        assert!(mime_matches("image/*", "image/png") && !mime_matches("image/*", "video/mp4") && mime_matches("application/pdf", "application/pdf"));
        assert_eq!(expand("v {}", Path::new("/a b/it's")), "v '/a b/it'\\''s'");
        assert_eq!(expand("v", Path::new("/x")), "v '/x'");
    }
}
