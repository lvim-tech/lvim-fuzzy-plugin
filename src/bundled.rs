//! Plugins that ship INSIDE a tool (lvim-fuzzy-plugin-preview is in `lvim-fuzzy` and in `lvc`): the first time any of those
//! tools runs it writes the plugin's `plugin.toml` into the shared plugins directory, with `exec` pointing at the
//! running program (`<this program> <arg>`, the program answers as that plugin). So the plugin is there whichever tool
//! — lfz, lvc, the Neovim picker through lfz — is run first, with nothing to install.
//!
//! The written file starts with markers: `# bundled-version: V` and `# bundled-exec: PATH`. It is rewritten only
//! when the version changed or the program it points at is gone; a `plugin.toml` WITHOUT the markers (the user's own
//! install of the same name) is never touched.

use std::path::Path;

/// Marks a `plugin.toml` as written by a tool (see the module).
pub const MARKER: &str = "# bundled-version: ";
const EXEC_MARKER: &str = "# bundled-exec: ";

/// What a tool's config says about a plugin that ships inside it: `[plugins.<name>] install = …`. Nothing is installed by itself.
///
/// - not there (or `false`): the tool does not install it — a plugin of that name that is installed otherwise (a build of its own, a
///   copy in the plugins directory) is used as it is;
/// - `true` or `"bundled"`: the tool writes the plugin's `plugin.toml` with `exec` pointing at itself;
/// - `"/path/to/program"`: the same, with that program (a build of the plugin's own, or another tool) answering as the plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    No,
    Bundled,
    Program(std::path::PathBuf),
}

/// The `install` key of `[plugins.<name>]` in `plugins` (a tool's `[plugins]` table).
pub fn install_of(plugins: &toml::Table, name: &str) -> Install {
    match plugins.get(name).and_then(toml::Value::as_table).and_then(|t| t.get("install")) {
        Some(toml::Value::Boolean(true)) => Install::Bundled,
        Some(toml::Value::String(s)) if s == "bundled" => Install::Bundled,
        Some(toml::Value::String(s)) if !s.trim().is_empty() => {
            let p = s.trim();
            let p = match p.strip_prefix("~/") {
                Some(rest) => std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
                None => std::path::PathBuf::from(p),
            };
            Install::Program(p)
        }
        _ => Install::No,
    }
}

/// Do what the config says for the plugin `name`: `ensure(root, program)` writes its manifest for `program` (the running tool for
/// `"bundled"`, the given one otherwise); nothing at all when the config says nothing.
pub fn install_configured(root: &Path, plugins: &toml::Table, name: &str, ensure: fn(&Path, &Path)) {
    match install_of(plugins, name) {
        Install::No => {}
        Install::Bundled => {
            if let Ok(me) = std::env::current_exe() {
                ensure(root, &me);
            }
        }
        Install::Program(p) => ensure(root, &p),
    }
}

/// A plugin that ships inside a tool.
pub struct Bundled<'a> {
    pub name: &'a str,
    pub version: &'a str,
    /// The plugin's `plugin.toml` as shipped: its `exec` line is replaced, its `build` line dropped.
    pub manifest: &'a str,
}

/// The manifest to write: markers, then the shipped one with `exec` = `'<exe>' <arg>` (run by `sh -c`).
pub fn render(b: &Bundled, exe: &Path, arg: &str) -> String {
    let exe_s = exe.to_string_lossy();
    // a TOML basic string: backslash and quote escaped; the program quoted for the shell
    let cmd = format!("'{}' {arg}", exe_s.replace('\'', r"'\''"));
    let exec = format!("exec        = \"{}\"", cmd.replace('\\', r"\\").replace('"', r#"\""#));
    let mut out = format!(
        "# Written by the tool that carries this plugin; rewritten when it changes. Delete the two `bundled` lines\n# to take the file over (it is then never touched again).\n{MARKER}{}\n{EXEC_MARKER}{exe_s}\n",
        b.version
    );
    for line in b.manifest.lines() {
        let key = line.trim_start().split(['=', ' ']).next().unwrap_or("");
        if key == "exec" && !line.starts_with('#') {
            out.push_str(&exec);
        } else if key == "build" && !line.starts_with('#') {
            continue;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Make sure `<root>/<name>/plugin.toml` is there for the program `exe`. Ok(true) = written now, Ok(false) = left as
/// it was (already current, or the user's own).
pub fn ensure(root: &Path, b: &Bundled, exe: &Path, arg: &str) -> Result<bool, String> {
    let dir = root.join(b.name);
    let file = dir.join("plugin.toml");
    if let Ok(old) = std::fs::read_to_string(&file) {
        let marked = old.lines().any(|l| l.starts_with(MARKER));
        if !marked {
            return Ok(false);
        }
        let same_version = old.lines().any(|l| l.strip_prefix(MARKER) == Some(b.version));
        let target_alive = old.lines().find_map(|l| l.strip_prefix(EXEC_MARKER)).is_some_and(|p| Path::new(p).is_file());
        if same_version && target_alive {
            return Ok(false);
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // written beside, then moved: two tools starting together never read half a file
    let tmp = dir.join(format!(".plugin.toml.{}", std::process::id()));
    std::fs::write(&tmp, render(b, exe, arg)).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(true)
}

/// Take away a bundled plugin that went by another name (`lvim-fuzzy-preview` became `lvim-fuzzy-plugin-preview`):
/// two plugins for one job would both answer. Only a directory this module wrote (its `plugin.toml` carries the
/// marker) is removed; the user's own install of that name is left alone. Ok(true) = removed.
pub fn retire(root: &Path, old_name: &str) -> Result<bool, String> {
    let dir = root.join(old_name);
    let Ok(text) = std::fs::read_to_string(dir.join("plugin.toml")) else { return Ok(false) };
    if !text.lines().any(|l| l.starts_with(MARKER)) {
        return Ok(false);
    }
    // only what we wrote: the manifest (and a half-written temporary of it) — a directory with anything else in it
    // is somebody's work and stays
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != "plugin.toml" && !name.starts_with(".plugin.toml.") {
            return Ok(false);
        }
        std::fs::remove_file(entry.path()).map_err(|e| format!("{}: {e}", entry.path().display()))?;
    }
    std::fs::remove_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_config_decides_whether_a_bundled_plugin_is_installed() {
        let t: toml::Table = toml::from_str(
            "[a]\ninstall = \"bundled\"\n[b]\ninstall = true\n[c]\ninstall = \"/opt/p\"\n[d]\ninstall = false\n[e]\nother = 1\n",
        )
        .unwrap();
        assert_eq!(install_of(&t, "a"), Install::Bundled);
        assert_eq!(install_of(&t, "b"), Install::Bundled);
        assert_eq!(install_of(&t, "c"), Install::Program("/opt/p".into()));
        assert_eq!(install_of(&t, "d"), Install::No);
        assert_eq!(install_of(&t, "e"), Install::No);
        assert_eq!(install_of(&t, "missing"), Install::No);
        // nothing is written for what the config does not ask for
        fn mark(_: &Path, _: &Path) {
            panic!("not asked for");
        }
        install_configured(Path::new("/nonexistent"), &t, "e", mark);
    }

    use super::*;

    const MANIFEST: &str = "description = \"x\"\nruntime = \"process\"\nexec        = \"./target/release/p\"\nbuild       = \"cargo build\"\n[preview.any]\npriority = 5\n";

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lvim-bundled-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn bundle(v: &str) -> Bundled<'_> {
        Bundled { name: "p", version: v, manifest: MANIFEST }
    }

    #[test]
    fn the_rendered_manifest_runs_the_program_and_still_parses() {
        let exe = Path::new("/opt/it's here/lvim-fuzzy");
        let t = render(&bundle("1"), exe, "--lvim-plugin preview");
        let doc: toml::Table = t.parse().unwrap();
        assert_eq!(doc["exec"].as_str().unwrap(), r"'/opt/it'\''s here/lvim-fuzzy' --lvim-plugin preview");
        assert!(doc.get("build").is_none());
        assert_eq!(doc["preview"]["any"]["priority"].as_integer(), Some(5));
        assert!(t.starts_with("# Written by"));
    }

    #[test]
    fn written_once_then_left_alone_until_the_version_or_the_program_changes() {
        let root = tmp("ensure");
        let exe = root.join("tool");
        std::fs::write(&exe, "").unwrap();
        assert_eq!(ensure(&root, &bundle("1"), &exe, "run"), Ok(true));
        assert_eq!(ensure(&root, &bundle("1"), &exe, "run"), Ok(false));
        // a new version of the plugin
        assert_eq!(ensure(&root, &bundle("2"), &exe, "run"), Ok(true));
        // the program it points at is gone, another tool runs: rewritten for that one
        let other = root.join("other");
        std::fs::write(&other, "").unwrap();
        std::fs::remove_file(&exe).unwrap();
        assert_eq!(ensure(&root, &bundle("2"), &other, "run"), Ok(true));
        let t = std::fs::read_to_string(root.join("p/plugin.toml")).unwrap();
        assert!(t.contains(&other.to_string_lossy().to_string()));
        // two tools alive: neither rewrites for itself
        assert_eq!(ensure(&root, &bundle("2"), &exe, "run"), Ok(false));
    }

    #[test]
    fn an_old_bundled_plugin_is_retired_but_not_the_users_own() {
        let root = tmp("retire");
        let exe = root.join("tool");
        std::fs::write(&exe, "").unwrap();
        let old = Bundled { name: "old", version: "1", manifest: MANIFEST };
        ensure(&root, &old, &exe, "run").unwrap();
        assert_eq!(retire(&root, "old"), Ok(true));
        assert!(!root.join("old").exists());
        assert_eq!(retire(&root, "old"), Ok(false)); // already gone
        // the user's own (no marker)
        std::fs::create_dir_all(root.join("mine")).unwrap();
        std::fs::write(root.join("mine/plugin.toml"), "runtime = \"process\"\n").unwrap();
        assert_eq!(retire(&root, "mine"), Ok(false));
        assert!(root.join("mine/plugin.toml").exists());
        // a bundled one the user added files to
        ensure(&root, &old, &exe, "run").unwrap();
        std::fs::write(root.join("old/notes.txt"), "mine").unwrap();
        assert_eq!(retire(&root, "old"), Ok(false));
        assert!(root.join("old/notes.txt").exists());
    }

    #[test]
    fn the_users_own_install_is_never_touched() {
        let root = tmp("own");
        std::fs::create_dir_all(root.join("p")).unwrap();
        std::fs::write(root.join("p/plugin.toml"), "runtime = \"process\"\nexec = \"mine\"\n").unwrap();
        assert_eq!(ensure(&root, &bundle("1"), Path::new("/x"), "run"), Ok(false));
        assert!(std::fs::read_to_string(root.join("p/plugin.toml")).unwrap().contains("mine"));
    }
}
