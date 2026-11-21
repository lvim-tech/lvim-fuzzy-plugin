//! Providers: what a plugin offers to a host, found by the file in front of it. A plugin's `plugin.toml` may have
//! `[preview.<id>]` tables (it can PREVIEW some files) and `[open.<id>]` tables (it knows how to OPEN some):
//!
//! ```toml
//! [preview.pdf]
//! match    = { ext = ["pdf"] }          # also: name = ["makefile"], mime = ["image/*"], kind = ["dir", "binary"]
//! priority = 50                         # higher wins; a user's plugin overrides a default by being higher
//! positions = true                      # (preview) it can page — J / K
//! ```
//!
//! A host classifies the file ([`FileInfo`]), asks the [`Registry`] for the providers of a class (`"preview"` or
//! `"open"`) that match, best first, and calls them in turn until one does not `pass`. `ext = ["*"]` and a missing
//! `match` match every file.

use std::path::Path;

use crate::Manifest;

/// What the host knows of a file before it asks anyone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// The file name, lower case.
    pub name: String,
    /// The extension, lower case, no dot ("" when there is none).
    pub ext: String,
    /// `"dir"`, `"binary"` (a NUL in the first 8 KB) or `"text"`.
    pub kind: String,
    pub mime: Option<String>,
}

impl FileInfo {
    /// Read what `path` is from the file system.
    pub fn of(path: &Path) -> FileInfo {
        let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let kind = if path.is_dir() {
            "dir"
        } else if path.is_file() && {
            use std::io::Read;
            let mut buf = [0u8; 8192];
            let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
            buf[..n].contains(&0)
        } {
            "binary"
        } else {
            "text"
        };
        FileInfo { name, ext, kind: kind.to_string(), mime: None }
    }
}

/// The `match = { ... }` of a provider: every condition that is given must hold; inside a list, any one will do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Matcher {
    pub ext: Vec<String>,
    pub name: Vec<String>,
    pub mime: Vec<String>,
    pub kind: Vec<String>,
}

impl Matcher {
    pub fn matches(&self, f: &FileInfo) -> bool {
        let any = |list: &[String], hit: &dyn Fn(&str) -> bool| list.is_empty() || list.iter().any(|x| hit(x));
        any(&self.ext, &|e| e == "*" || e.trim_start_matches('.') == f.ext)
            && any(&self.name, &|n| n == f.name)
            && any(&self.kind, &|k| k == f.kind)
            && any(&self.mime, &|m| match (&f.mime, m.strip_suffix("/*")) {
                (Some(fm), Some(prefix)) => fm.starts_with(&format!("{prefix}/")),
                (Some(fm), None) => fm == m,
                (None, _) => false,
            })
    }
}

/// One thing a plugin offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub plugin: String,
    pub id: String,
    /// `"preview"` or `"open"`.
    pub class: String,
    pub priority: i64,
    /// (preview) it can show more than one position of a file (a PDF's pages, a video's points).
    pub positions: bool,
    pub matcher: Matcher,
}

fn strings(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_ascii_lowercase()).collect())
        .unwrap_or_default()
}

/// The providers of every loaded plugin, best first.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    providers: Vec<Provider>,
}

impl Registry {
    /// Read the `[preview.*]` and `[open.*]` tables of `manifests`; mistakes are returned as texts.
    pub fn from_manifests(manifests: &[Manifest]) -> (Registry, Vec<String>) {
        let (mut providers, mut errs) = (Vec::new(), Vec::new());
        for m in manifests {
            let at = m.def.dir.join("plugin.toml").display().to_string();
            for class in ["preview", "open"] {
                let Some(t) = m.table.get(class) else { continue };
                let Some(t) = t.as_table() else {
                    errs.push(format!("{at}: [{class}] must be a table of providers: [{class}.<id>]"));
                    continue;
                };
                for (id, p) in t {
                    let Some(p) = p.as_table() else {
                        errs.push(format!("{at}: {class}.{id} must be a table"));
                        continue;
                    };
                    for k in p.keys() {
                        if !["match", "priority", "positions"].contains(&k.as_str()) {
                            errs.push(format!("{at}: {class}.{id}: unknown key `{k}` (match, priority, positions)"));
                        }
                    }
                    let mt = p.get("match").and_then(|v| v.as_table());
                    if let Some(mt) = mt {
                        for k in mt.keys() {
                            if !["ext", "name", "mime", "kind"].contains(&k.as_str()) {
                                errs.push(format!("{at}: {class}.{id}.match: unknown key `{k}` (ext, name, mime, kind)"));
                            }
                        }
                    }
                    let matcher = mt
                        .map(|t| Matcher { ext: strings(t.get("ext")), name: strings(t.get("name")), mime: strings(t.get("mime")), kind: strings(t.get("kind")) })
                        .unwrap_or_default();
                    providers.push(Provider {
                        plugin: m.def.name.clone(),
                        id: id.clone(),
                        class: class.to_string(),
                        priority: p.get("priority").and_then(|v| v.as_integer()).unwrap_or(0),
                        positions: p.get("positions").and_then(|v| v.as_bool()).unwrap_or(false),
                        matcher,
                    });
                }
            }
        }
        providers.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.plugin.cmp(&b.plugin)).then_with(|| a.id.cmp(&b.id)));
        (Registry { providers }, errs)
    }

    /// The providers of `class` that match `file`, best first.
    pub fn select(&self, class: &str, file: &FileInfo) -> Vec<&Provider> {
        self.providers.iter().filter(|p| p.class == class && p.matcher.matches(file)).collect()
    }

    pub fn all(&self) -> &[Provider] {
        &self.providers
    }
}

/// Is `prog` on the PATH (or, with a `/` in it, a file that exists)?
pub fn have(prog: &str) -> bool {
    if prog.contains('/') {
        return Path::new(prog).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

/// What a plugin `requires` that is not installed: one line each (`"a|b"` is missing only when neither is).
/// `have` says whether a program is installed (the real one is [`have`]; a test gives its own).
pub fn missing_requires(requires: &[String], have: &dyn Fn(&str) -> bool) -> Vec<String> {
    requires
        .iter()
        .filter(|r| !r.split('|').any(|p| have(p.trim())))
        .map(|r| r.split('|').map(str::trim).collect::<Vec<_>>().join(" or "))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn manifest(name: &str, toml: &str) -> Manifest {
        let mut table: toml::Table = toml::from_str(toml).unwrap();
        table.remove("name");
        Manifest {
            def: crate::PluginDef {
                name: name.into(),
                description: String::new(),
                dir: PathBuf::from("/x").join(name),
                runtime: crate::Runtime::Process,
                exec: None,
                library: None,
                module: None,
                allow_write: false,
                script: None,
                events: vec![],
                hosts: vec![],
                api: 1,
                version: None,
                author: None,
                license: None,
                repository: None,
                build: None,
                requires: vec![],
                config: Default::default(),
            },
            table,
        }
    }

    fn file(name: &str, kind: &str) -> FileInfo {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default();
        FileInfo { name: name.to_ascii_lowercase(), ext, kind: kind.into(), mime: None }
    }

    #[test]
    fn providers_are_found_by_the_file_best_first() {
        let a = manifest(
            "first-party",
            "[preview.pdf]\nmatch = { ext = [\"pdf\"] }\npriority = 50\npositions = true\n[preview.dirs]\nmatch = { kind = [\"dir\"] }\n[preview.any]\npriority = -5\n[open.default]\nmatch = { ext = [\"*\"] }\n",
        );
        let b = manifest("mine", "[preview.pdf]\nmatch = { ext = [\".PDF\"] }\npriority = 80\n[preview.make]\nmatch = { name = [\"Makefile\"] }\n");
        let (r, errs) = Registry::from_manifests(&[a, b]);
        assert!(errs.is_empty(), "{errs:?}");
        // a PDF: the user's plugin first (80), then the default (50), then the catch-all (-5)
        let ids: Vec<String> = r.select("preview", &file("a.pdf", "binary")).iter().map(|p| format!("{}/{}", p.plugin, p.id)).collect();
        assert_eq!(ids, vec!["mine/pdf", "first-party/pdf", "first-party/any"]);
        assert!(r.select("preview", &file("a.pdf", "binary"))[1].positions);
        // a directory, a Makefile, and the open class
        assert_eq!(r.select("preview", &file("src", "dir")).iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["dirs", "any"]);
        assert_eq!(r.select("preview", &file("makefile", "text")).iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["make", "any"]);
        assert_eq!(r.select("open", &file("x.png", "binary")).len(), 1);
    }

    #[test]
    fn the_match_rules() {
        let m = Matcher { ext: vec!["png".into()], mime: vec!["image/*".into()], ..Default::default() };
        let mut f = file("a.png", "binary");
        assert!(!m.matches(&f)); // a mime is asked for and the file has none
        f.mime = Some("image/png".into());
        assert!(m.matches(&f));
        f.mime = Some("text/plain".into());
        assert!(!m.matches(&f));
        assert!(Matcher::default().matches(&file("anything", "text"))); // no conditions: every file
    }

    #[test]
    fn mistakes_are_said() {
        let (_, errs) = Registry::from_manifests(&[manifest("p", "preview = 3\n[open.x]\nbogus = 1\nmatch = { color = [\"red\"] }\n")]);
        assert_eq!(errs.len(), 3, "{errs:?}");
    }

    #[test]
    fn what_is_missing_from_requires() {
        let have = |p: &str| p == "ffmpeg" || p == "pdfinfo";
        let req = vec!["ffmpegthumbnailer|ffmpeg".to_string(), "pdftoppm".to_string(), "pdfinfo".to_string(), "mutool|gs".to_string()];
        assert_eq!(missing_requires(&req, &have), vec!["pdftoppm".to_string(), "mutool or gs".to_string()]);
        assert!(missing_requires(&[], &have).is_empty());
    }

    #[test]
    fn a_file_is_classified() {
        let dir = std::env::temp_dir().join(format!("lvim-plugin-fi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("A.TXT"), "hi").unwrap();
        std::fs::write(dir.join("b.bin"), [0u8, 1]).unwrap();
        assert_eq!(FileInfo::of(&dir.join("A.TXT")), FileInfo { name: "a.txt".into(), ext: "txt".into(), kind: "text".into(), mime: None });
        assert_eq!(FileInfo::of(&dir.join("b.bin")).kind, "binary");
        assert_eq!(FileInfo::of(&dir).kind, "dir");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
