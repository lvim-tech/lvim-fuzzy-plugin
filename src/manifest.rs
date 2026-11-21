//! `plugin.toml`: what is common to every host (the runtime and where it lives). The tables only a host
//! understands (`[actions.*]`, `[previewers.*]`, ...) are handed back untouched in [`Manifest::table`].
//!
//! ```toml
//! description = "what it does"
//! runtime     = "process"            # command (default) | process | native | wasm | lua
//! hosts       = ["lvim-fuzzy-commander", "lvim-fuzzy-cli"]   # optional: only for these tools (all when absent)
//! exec        = "./run"              # process: the program (sh -c, in the plugin's directory)
//! events      = ["cd", "quit"]       # process: what it wants to hear (cd cursor selection quit)
//! library     = "./libx.so"          # native
//! module      = "./x.wasm"           # wasm
//! allow_write = false                # wasm: may change files in the directories it is given
//! script      = "init.lua"           # lua (the default)
//!
//! requires    = ["ffmpeg", "a|b"]    # programs it calls ("a|b": either); the host says what is missing
//! [config]                           # the plugin's DEFAULT settings (users override them, see `config`)
//!
//! build       = "cargo build --release"   # how to build what exec / library / module points at (run by
//!                                         # `lvim-fuzzy-plugin install` after the user agrees)
//!
//! # for plugins that are shared: who made it, and which plugin API it was written against
//! api         = 1                    # this crate's [`API`]; a newer one is refused with a message
//! version     = "0.2.0"
//! author      = "Name <mail>"
//! license     = "MIT"
//! repository  = "https://github.com/someone/lvim-plugin-x"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Runtime;

/// The plugin API version this host speaks (`api` in `plugin.toml`; absent = 1). A plugin written for a
/// newer one is refused (the message says to update the tool), so a plugin never half-works.
pub const API: u32 = 1;

/// The events a `process` plugin can ask for.
pub const EVENTS: [&str; 4] = ["cd", "cursor", "selection", "quit"];

/// A loaded plugin (what every host needs to know).
#[derive(Clone, Debug)]
pub struct PluginDef {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub runtime: Runtime,
    /// `runtime = "process"`: the program (`sh -c`, in the plugin's directory).
    pub exec: Option<String>,
    /// `runtime = "native"`: the shared library, a path relative to the plugin's directory.
    pub library: Option<String>,
    /// `runtime = "wasm"`: the module, a path relative to the plugin's directory.
    pub module: Option<String>,
    /// `runtime = "wasm"`: the module may change files in the directories it is given (read-only otherwise).
    pub allow_write: bool,
    /// `runtime = "lua"`: the script, relative to the plugin's directory (default `init.lua`).
    pub script: Option<String>,
    /// The events a process wants (see [`EVENTS`]).
    pub events: Vec<String>,
    /// The tools it is for (`lvc`, `lfz`, `picker`); empty = all.
    pub hosts: Vec<String>,
    /// The plugin API it was written for ([`API`]).
    pub api: u32,
    /// Who made it (informational: `version`, `author`, `license`, `repository`).
    pub version: Option<String>,
    pub author: Option<String>,
    pub license: Option<String>,
    pub repository: Option<String>,
    /// How to build the plugin's program / library / module (`lvim-fuzzy-plugin install` runs it, with consent).
    pub build: Option<String>,
    /// Programs it calls (`"a|b"` = either one will do): a host shows what is missing on this machine.
    pub requires: Vec<String>,
    /// The plugin's DEFAULT settings (its `[config]` table); see [`crate::config`].
    pub config: toml::Table,
}

/// A plugin and the rest of its `plugin.toml` (the host's own tables).
pub struct Manifest {
    pub def: PluginDef,
    pub table: toml::Table,
}

/// The keys of `plugin.toml` this crate reads.
const COMMON: [&str; 18] = [
    "name", "description", "runtime", "hosts", "exec", "events", "library", "module", "allow_write", "script", "api",
    "version", "author", "license", "repository", "build", "requires", "config",
];

/// Every plugin under `root` (one directory each, with a `plugin.toml`), sorted by name, that is meant for
/// `host` (a plugin with no `hosts` is for all). Problems are returned as texts, the plugin skipped.
pub fn load(root: &Path, host: &str) -> (Vec<Manifest>, Vec<String>) {
    load_filtered(root, Some(host))
}

/// The plugins directory every tool shares: `$XDG_CONFIG_HOME/lvim-plugins`, else `~/.config/lvim-plugins`.
pub fn default_root() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lvim-plugins")
}

/// [`load`] over several directories, in order (the shared one first): a plugin name found twice is used from
/// the first directory and reported.
pub fn load_roots(roots: &[PathBuf], host: &str) -> (Vec<Manifest>, Vec<String>) {
    let (mut out, mut errs): (Vec<Manifest>, Vec<String>) = (Vec::new(), Vec::new());
    for r in roots {
        let (ms, es) = load(r, host);
        errs.extend(es);
        for m in ms {
            if out.iter().any(|o| o.def.name == m.def.name) {
                errs.push(format!("{}: plugin `{}` is already loaded from another directory; this one is ignored", m.def.dir.display(), m.def.name));
            } else {
                out.push(m);
            }
        }
    }
    (out, errs)
}

/// [`load`] for every plugin, whatever tool it names itself for (a listing, a developer's check).
pub fn load_all(root: &Path) -> (Vec<Manifest>, Vec<String>) {
    load_filtered(root, None)
}

fn load_filtered(root: &Path, host: Option<&str>) -> (Vec<Manifest>, Vec<String>) {
    let (mut out, mut errs) = (Vec::new(), Vec::new());
    let Ok(rd) = std::fs::read_dir(root) else {
        return (out, errs);
    };
    let mut dirs: BTreeMap<String, PathBuf> = BTreeMap::new();
    for e in rd.flatten() {
        if e.path().join("plugin.toml").is_file() {
            dirs.insert(e.file_name().to_string_lossy().into_owned(), e.path());
        }
    }
    for (name, dir) in dirs {
        let (m, e) = read(&dir, &name);
        errs.extend(e);
        let Some(m) = m else { continue };
        if let Some(host) = host
            && !m.def.hosts.is_empty()
            && !m.def.hosts.iter().any(|h| h == host)
        {
            continue;
        }
        out.push(m);
    }
    (out, errs)
}

/// One plugin directory: its `plugin.toml` read and checked. `None` when the plugin cannot be used at all
/// (no file, bad TOML, unknown runtime, a newer API); smaller mistakes are returned as texts and the plugin
/// is still read. `fallback_name` is the name when `plugin.toml` has none.
pub fn read(dir: &Path, fallback_name: &str) -> (Option<Manifest>, Vec<String>) {
    let mut errs: Vec<String> = Vec::new();
    let at = dir.join("plugin.toml").display().to_string();
    let text = match std::fs::read_to_string(dir.join("plugin.toml")) {
        Ok(t) => t,
        Err(e) => {
            errs.push(format!("{at}: {e}"));
            return (None, errs);
        }
    };
    let mut table = match toml::from_str::<toml::Table>(&text) {
        Ok(t) => t,
        Err(e) => {
            errs.push(format!("{at}: {e}"));
            return (None, errs);
        }
    };
    let runtime = match table.get("runtime") {
        None => Runtime::Command,
        Some(v) => match v.as_str().and_then(Runtime::parse) {
            Some(r) => r,
            None => {
                errs.push(format!("{at}: runtime: command, process, native, wasm or lua"));
                return (None, errs);
            }
        },
    };
    let s = |k: &str| table.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let list = |k: &str, errs: &mut Vec<String>, ok: &dyn Fn(&str) -> bool, what: &str| -> Vec<String> {
        let mut v = Vec::new();
        for e in table.get(k).and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
            match e.as_str() {
                Some(e) if ok(e) => v.push(e.to_string()),
                _ => errs.push(format!("{at}: {k}: {what}")),
            }
        }
        v
    };
    let api = match table.get("api") {
        None => 1,
        Some(v) => match v.as_integer().and_then(|n| u32::try_from(n).ok()) {
            Some(n) => n,
            None => {
                errs.push(format!("{at}: api: a whole number (the plugin API version, {API})"));
                return (None, errs);
            }
        },
    };
    if api > API {
        errs.push(format!("{at}: written for plugin API {api}, this tool speaks {API} (update the tool)"));
        return (None, errs);
    }
    let (exec, library, module) = (s("exec"), s("library"), s("module"));
    for (r, key, val) in [(Runtime::Process, "exec", &exec), (Runtime::Native, "library", &library), (Runtime::Wasm, "module", &module)] {
        if runtime == r && val.is_none() {
            let hint = match r {
                Runtime::Process => "the program to start",
                Runtime::Native => "the .so, relative to the plugin",
                _ => "the .wasm, relative to the plugin",
            };
            errs.push(format!("{at}: runtime = \"{}\" needs `{key}` ({hint})", r.name()));
        }
    }
    if runtime != Runtime::Process && (table.contains_key("exec") || table.contains_key("events")) {
        errs.push(format!("{at}: `exec` and `events` are for runtime = \"process\""));
    }
    if runtime != Runtime::Native && table.contains_key("library") {
        errs.push(format!("{at}: `library` is for runtime = \"native\""));
    }
    if runtime != Runtime::Wasm && (table.contains_key("module") || table.contains_key("allow_write")) {
        errs.push(format!("{at}: `module` and `allow_write` are for runtime = \"wasm\""));
    }
    if runtime != Runtime::Lua && table.contains_key("script") {
        errs.push(format!("{at}: `script` is for runtime = \"lua\""));
    }
    let events = list("events", &mut errs, &|e| EVENTS.contains(&e), &format!("a list of {}", EVENTS.join(", ")));
    let hosts = list("hosts", &mut errs, &|_| true, "a list of tool names (lvim-fuzzy-commander, lvim-fuzzy-cli, lvim-fuzzy-picker)");
    let def = PluginDef {
        name: s("name").unwrap_or_else(|| fallback_name.to_string()),
        description: s("description").unwrap_or_default(),
        dir: dir.to_path_buf(),
        runtime,
        exec,
        library,
        module,
        allow_write: table.get("allow_write").and_then(|v| v.as_bool()).unwrap_or(false),
        script: (runtime == Runtime::Lua).then(|| s("script").unwrap_or_else(|| "init.lua".into())),
        events,
        hosts,
        api,
        version: s("version"),
        author: s("author"),
        license: s("license"),
        repository: s("repository"),
        build: s("build"),
        requires: list("requires", &mut errs, &|_| true, "a list of program names (\"a|b\" for either)"),
        config: table.get("config").and_then(|v| v.as_table()).cloned().unwrap_or_default(),
    };
    if table.get("config").is_some_and(|v| !v.is_table()) {
        errs.push(format!("{at}: config must be a table"));
    }
    for k in COMMON {
        table.remove(k);
    }
    (Some(Manifest { def, table }), errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str, plugins: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("lvim-plugin-{name}-{}", std::process::id()));
        for (n, toml) in plugins {
            std::fs::create_dir_all(root.join(n)).unwrap();
            std::fs::write(root.join(n).join("plugin.toml"), toml).unwrap();
        }
        root
    }

    #[test]
    fn reads_the_common_keys_and_hands_back_the_rest() {
        let r = root(
            "keys",
            &[(
                "p",
                "description = \"d\"\nruntime = \"process\"\nexec = \"./run\"\nevents = [\"cd\"]\nhosts = [\"lvim-fuzzy-commander\"]\napi = 1\nversion = \"0.2\"\nauthor = \"A\"\nlicense = \"MIT\"\n[actions.x]\ncall = \"f\"\n",
            )],
        );
        let (m, errs) = load(&r, "lvim-fuzzy-commander");
        assert!(errs.is_empty(), "{errs:?}");
        let d = &m[0].def;
        assert_eq!((d.name.as_str(), d.runtime, d.exec.as_deref()), ("p", Runtime::Process, Some("./run")));
        assert_eq!((d.version.as_deref(), d.author.as_deref(), d.license.as_deref()), (Some("0.2"), Some("A"), Some("MIT")));
        assert_eq!(d.events, vec!["cd".to_string()]);
        // the host's own table is left alone; the common keys are gone from it
        assert!(m[0].table.contains_key("actions"));
        assert!(!m[0].table.contains_key("runtime"));
        let _ = std::fs::remove_file(r.join("p/plugin.toml"));
        let _ = std::fs::remove_dir(r.join("p"));
        let _ = std::fs::remove_dir(&r);
    }

    #[test]
    fn hosts_filter_and_api_and_mistakes() {
        let r = root(
            "filter",
            &[
                ("all", "description = \"for every tool\"\n"),
                ("only_lfz", "hosts = [\"lvim-fuzzy-cli\"]\n"),
                ("future", "api = 99\n"),
                ("noexec", "runtime = \"process\"\n"),
                ("strayexec", "exec = \"x\"\n"),
                ("badrt", "runtime = \"java\"\n"),
                ("badev", "runtime = \"process\"\nexec = \"x\"\nevents = [\"boom\"]\n"),
            ],
        );
        let (m, errs) = load(&r, "lvim-fuzzy-commander");
        let names: Vec<&str> = m.iter().map(|x| x.def.name.as_str()).collect();
        assert!(names.contains(&"all") && !names.contains(&"only_lfz"), "{names:?}");
        assert!(errs.iter().any(|e| e.contains("future") && e.contains("plugin API 99") && e.contains("update the tool")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("needs `exec`")));
        assert!(errs.iter().any(|e| e.contains("`exec` and `events` are for runtime")));
        assert!(errs.iter().any(|e| e.contains("runtime: command, process")));
        assert!(errs.iter().any(|e| e.contains("events: a list of cd, cursor")));
        // a listing sees the plugin made for another tool too
        let (all, _) = load_all(&r);
        assert!(all.iter().any(|x| x.def.name == "only_lfz"));
        for (n, _) in [("all", 0), ("only_lfz", 0), ("future", 0), ("noexec", 0), ("strayexec", 0), ("badrt", 0), ("badev", 0)] {
            let _ = std::fs::remove_file(r.join(n).join("plugin.toml"));
            let _ = std::fs::remove_dir(r.join(n));
        }
        let _ = std::fs::remove_dir(&r);
    }
}
