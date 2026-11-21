//! Installing plugins by other developers: `lvim-fuzzy-plugin install <git-url>`, `update`, `remove`.
//!
//! The source is a git URL only (anything `git clone` takes as a URL: `https://`, `ssh://`, `git@host:path`,
//! `file://`); a folder you already have is installed by copying it into the plugins directory. The plugin
//! lands in `<root>/<name>/` — `<name>` is the `name` in its `plugin.toml`, else the repository's name.
//!
//! Trust: a `wasm` plugin runs in a sandbox, so it is installed on a plain yes. Every other runtime runs with
//! the user's rights, so the user is shown who made it and what it will run and has to agree; a plugin's
//! `build` command (how to build its program) needs a second yes. What was agreed to (the runtime and the
//! build command) is remembered in `<root>/.trust.toml`; an `update` that changes either asks again.
//! Everything that asks goes through [`Prompt`], so the tool can ask on a terminal and a test can answer.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Runtime, manifest};

/// Something that can ask the user a yes / no question.
pub trait Prompt {
    fn confirm(&mut self, question: &str) -> bool;
}

/// Always yes (`--yes`).
pub struct Yes;
impl Prompt for Yes {
    fn confirm(&mut self, _: &str) -> bool {
        true
    }
}

/// Always no (what a non-interactive run without `--yes` gets).
pub struct No;
impl Prompt for No {
    fn confirm(&mut self, _: &str) -> bool {
        false
    }
}

/// Asks on the terminal: the question, then a line from stdin.
pub struct Terminal;
impl Prompt for Terminal {
    fn confirm(&mut self, question: &str) -> bool {
        use std::io::{BufRead, Write};
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// Is this a git URL (not a path, not an option)?
pub fn is_git_url(u: &str) -> bool {
    !u.starts_with('-') && !u.starts_with('/') && !u.starts_with('.') && (u.contains("://") || (u.contains('@') && u.contains(':')))
}

/// The repository's name out of its URL: the last path part without `.git`.
pub fn repo_name(url: &str) -> String {
    let t = url.trim_end_matches('/');
    let last = t.rsplit(['/', ':']).next().unwrap_or(t);
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// A plugin directory name: letters, digits, `.`, `_`, `-`; not starting with a dot (no way out of the root).
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && !n.starts_with('.') && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn git(args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    let mut c = Command::new("git");
    c.args(args).stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(d) = dir {
        c.current_dir(d);
    }
    let o = c.output().map_err(|e| format!("git: {e} (is git installed?)"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&o.stderr);
        Err(format!("git {}: {}", args.first().unwrap_or(&""), err.lines().last().unwrap_or("failed").trim()))
    }
}

fn trust_path(root: &Path) -> PathBuf {
    root.join(".trust.toml")
}

fn read_trust(root: &Path) -> toml::Table {
    std::fs::read_to_string(trust_path(root)).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
}

fn write_trust(root: &Path, name: &str, runtime: Runtime, build: &Option<String>) {
    let mut t = read_trust(root);
    let mut e = toml::Table::new();
    e.insert("runtime".into(), runtime.name().into());
    if let Some(b) = build {
        e.insert("build".into(), b.clone().into());
    }
    t.insert(name.to_string(), toml::Value::Table(e));
    let _ = std::fs::write(trust_path(root), toml::to_string(&t).unwrap_or_default());
}

fn forget_trust(root: &Path, name: &str) {
    let mut t = read_trust(root);
    if t.remove(name).is_some() {
        let _ = std::fs::write(trust_path(root), toml::to_string(&t).unwrap_or_default());
    }
}

/// What agreeing to a plugin means, for the question.
fn describe(m: &manifest::Manifest, url: &str) -> String {
    let d = &m.def;
    let who = [d.version.clone(), d.author.clone(), d.license.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let mut s = format!("plugin `{}` from {url}\n  {}\n", d.name, if d.description.is_empty() { "(no description)" } else { &d.description });
    if !who.is_empty() {
        s.push_str(&format!("  {who}\n"));
    }
    s.push_str(&match d.runtime {
        Runtime::Wasm => "  runtime: wasm — sandboxed (sees only the directories it is given, read-only unless it says otherwise)\n".to_string(),
        r => format!("  runtime: {} — it runs with YOUR rights, like any program you install\n", r.name()),
    });
    s
}

fn run_build(dir: &Path, cmd: &str) -> Result<(), String> {
    eprintln!("  building: {cmd}");
    let st = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).env("LVIM_PLUGIN_DIR", dir).status().map_err(|e| format!("build: {e}"))?;
    if st.success() { Ok(()) } else { Err(format!("the build command failed ({st})")) }
}

/// Install the plugin at `url` into `root`. Ok = a line saying what was installed.
pub fn install(root: &Path, url: &str, name: Option<&str>, prompt: &mut dyn Prompt) -> Result<String, String> {
    if !is_git_url(url) {
        return Err(format!("`{url}` is not a git URL (https://…, ssh://…, git@host:path); to install a folder, copy it into {}", root.display()));
    }
    std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let tmp = root.join(format!(".installing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    git(&["clone", "--depth", "1", "--quiet", "--", url, &tmp.to_string_lossy()], None)?;
    let result = (|| {
        let fallback = name.map(str::to_string).unwrap_or_else(|| repo_name(url));
        let (m, errs) = manifest::read(&tmp, &fallback);
        let Some(m) = m else {
            return Err(format!("not a plugin: {}", errs.join("; ")));
        };
        if !errs.is_empty() {
            return Err(format!("the plugin.toml has problems: {}", errs.join("; ")));
        }
        let name = name.map(str::to_string).unwrap_or_else(|| m.def.name.clone());
        if !valid_name(&name) {
            return Err(format!("`{name}` is not a usable plugin name (letters, digits, . _ -)"));
        }
        let target = root.join(&name);
        if target.exists() {
            return Err(format!("`{name}` is already installed (lvim-plugin update {name}, or remove it first)"));
        }
        let mut q = describe(&m, url);
        q.push_str("  install it?");
        if !prompt.confirm(q.trim_end()) {
            return Err("not installed (not agreed)".into());
        }
        if let Some(b) = &m.def.build
            && !prompt.confirm(&format!("this plugin must be built; it will run, on your machine:\n    {b}\n  run it?"))
        {
            return Err("not installed (the build was not agreed)".into());
        }
        std::fs::rename(&tmp, &target).map_err(|e| format!("{}: {e}", target.display()))?;
        if let Some(b) = &m.def.build
            && let Err(e) = run_build(&target, b)
        {
            let _ = std::fs::remove_dir_all(&target);
            return Err(e);
        }
        write_trust(root, &name, m.def.runtime, &m.def.build);
        Ok(format!("installed `{name}` [{}] in {}", m.def.runtime.name(), target.display()))
    })();
    let _ = std::fs::remove_dir_all(&tmp); // gone already when it was moved into place
    result
}

/// The plugin directories under `root` that came from git.
fn installed(root: &Path) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join(".git").exists() && e.path().join("plugin.toml").is_file())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    v.sort();
    v
}

/// Update `name` (every git-installed plugin when `None`) with `git pull`. One line each.
pub fn update(root: &Path, name: Option<&str>, prompt: &mut dyn Prompt) -> Vec<String> {
    let mut out = Vec::new();
    let all = installed(root);
    let picked: Vec<&(String, PathBuf)> = match name {
        Some(n) => all.iter().filter(|(x, _)| x == n).collect(),
        None => all.iter().collect(),
    };
    if picked.is_empty() {
        return vec![match name {
            Some(n) => format!("{n}: not installed from git (nothing to update)"),
            None => "no plugin installed from git".into(),
        }];
    }
    for (n, dir) in picked {
        let before = manifest::read(dir, n).0.map(|m| (m.def.version, m.def.runtime, m.def.build));
        if let Err(e) = git(&["pull", "--ff-only", "--quiet"], Some(dir)) {
            out.push(format!("{n}: {e}"));
            continue;
        }
        let (m, errs) = manifest::read(dir, n);
        let Some(m) = m else {
            out.push(format!("{n}: the updated plugin.toml is broken: {}", errs.join("; ")));
            continue;
        };
        // what was agreed to: a changed runtime or build command is asked about again
        let trusted = read_trust(root);
        let same = trusted
            .get(n.as_str())
            .and_then(|t| t.as_table())
            .is_some_and(|t| t.get("runtime").and_then(|v| v.as_str()) == Some(m.def.runtime.name()) && t.get("build").and_then(|v| v.as_str()) == m.def.build.as_deref());
        if !same {
            let q = format!("{}the update changes what it runs (runtime or build command). Keep it?", describe(&m, "its repository"));
            if !prompt.confirm(&q) {
                out.push(format!("{n}: updated, but NOT trusted — it stays installed; remove it (lvim-plugin remove {n}) if you do not want it"));
                continue;
            }
        }
        if let Some(b) = &m.def.build
            && let Err(e) = run_build(dir, b)
        {
            out.push(format!("{n}: updated, but {e}"));
            continue;
        }
        write_trust(root, n, m.def.runtime, &m.def.build);
        let was = before.and_then(|b| b.0).unwrap_or_else(|| "?".into());
        out.push(format!("{n}: {was} -> {}", m.def.version.as_deref().unwrap_or("?")));
    }
    out
}

/// Remove an installed plugin (asks first). Only a directory under `root`, found by name, is ever removed.
pub fn remove(root: &Path, name: &str, prompt: &mut dyn Prompt) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!("`{name}` is not a plugin name"));
    }
    let dir = root.join(name);
    if !dir.join("plugin.toml").is_file() {
        return Err(format!("`{name}` is not installed in {}", root.display()));
    }
    if !prompt.confirm(&format!("remove the plugin `{name}` ({})?", dir.display())) {
        return Err("not removed".into());
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    forget_trust(root, name);
    Ok(format!("removed `{name}`"))
}

/// Where a plugin came from (its git remote), for a listing.
pub fn origin(dir: &Path) -> Option<String> {
    dir.join(".git").exists().then(|| git(&["config", "--get", "remote.origin.url"], Some(dir)).ok()).flatten().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lvim-plugin-inst-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn g(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    /// A plugin repository on disk: `toml` is its plugin.toml.
    fn repo(base: &Path, name: &str, toml: &str) -> (PathBuf, String) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        g(&dir, &["init", "-q"]);
        std::fs::write(dir.join("plugin.toml"), toml).unwrap();
        g(&dir, &["add", "."]);
        g(&dir, &["commit", "-q", "-m", "v"]);
        let url = format!("file://{}", dir.display());
        (dir, url)
    }

    struct Counting(usize, bool);
    impl Prompt for Counting {
        fn confirm(&mut self, _: &str) -> bool {
            self.0 += 1;
            self.1
        }
    }

    #[test]
    fn urls_and_names() {
        assert!(is_git_url("https://github.com/a/b.git") && is_git_url("git@github.com:a/b.git") && is_git_url("file:///x/y"));
        assert!(!is_git_url("/some/folder") && !is_git_url("./x") && !is_git_url("--upload-pack=evil") && !is_git_url("plain"));
        assert_eq!(repo_name("https://github.com/a/lvim-plugin-x.git"), "lvim-plugin-x");
        assert_eq!(repo_name("git@host:a/b/"), "b");
        assert!(valid_name("my-plugin_1.0") && !valid_name("../x") && !valid_name(".hidden") && !valid_name("a/b") && !valid_name(""));
    }

    #[test]
    fn installs_updates_and_removes() {
        let base = tmp("flow");
        let root = base.join("root");
        let (src, url) = repo(&base, "remote", "description = \"d\"\nruntime = \"wasm\"\nmodule = \"./x.wasm\"\nversion = \"1.0\"\n");

        // a folder is not a git URL; a git URL clones, is checked, and lands under its name
        assert!(install(&root, "/some/folder", None, &mut Yes).unwrap_err().contains("not a git URL"));
        let msg = install(&root, &url, Some("nice"), &mut Yes).unwrap();
        assert!(msg.contains("installed `nice` [wasm]"), "{msg}");
        assert!(root.join("nice/plugin.toml").is_file());
        assert!(read_trust(&root).contains_key("nice"));
        assert!(origin(&root.join("nice")).unwrap().ends_with("remote"));
        // no half-made clone left behind
        assert!(std::fs::read_dir(&root).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".installing")));
        // twice: refused
        assert!(install(&root, &url, Some("nice"), &mut Yes).unwrap_err().contains("already installed"));
        // a name that climbs out of the root is refused (before anything is moved)
        assert!(install(&root, &url, Some("../evil"), &mut Yes).unwrap_err().contains("not a usable plugin name"));
        assert!(!base.join("evil").exists());

        // update: a new version comes in; same runtime, so no question
        std::fs::write(src.join("plugin.toml"), "description = \"d\"\nruntime = \"wasm\"\nmodule = \"./x.wasm\"\nversion = \"1.1\"\n").unwrap();
        g(&src, &["add", "."]);
        g(&src, &["commit", "-q", "-m", "v2"]);
        let mut ask = Counting(0, false);
        assert_eq!(update(&root, Some("nice"), &mut ask), vec!["nice: 1.0 -> 1.1".to_string()]);
        assert_eq!(ask.0, 0);
        assert_eq!(update(&root, Some("nothing"), &mut Yes), vec!["nothing: not installed from git (nothing to update)".to_string()]);

        // the update turns it into a program: asked again, and a no leaves it untrusted but reported
        std::fs::write(src.join("plugin.toml"), "runtime = \"process\"\nexec = \"./run\"\nversion = \"2.0\"\n").unwrap();
        g(&src, &["add", "."]);
        g(&src, &["commit", "-q", "-m", "v3"]);
        let mut no = Counting(0, false);
        let out = update(&root, None, &mut no);
        assert_eq!(no.0, 1);
        assert!(out[0].contains("NOT trusted"), "{out:?}");

        // remove asks; a no keeps it, a yes removes it and the trust record
        assert_eq!(remove(&root, "nice", &mut No), Err("not removed".into()));
        assert!(root.join("nice").exists());
        assert!(remove(&root, "../x", &mut Yes).is_err());
        assert_eq!(remove(&root, "nice", &mut Yes), Ok("removed `nice`".into()));
        assert!(!root.join("nice").exists());
        assert!(!read_trust(&root).contains_key("nice"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_program_needs_a_yes_and_a_build_needs_another() {
        let base = tmp("trust");
        let root = base.join("root");
        let (_s, url) = repo(&base, "prog", "runtime = \"process\"\nexec = \"./run\"\nbuild = \"echo built > built.txt\"\n");

        // not agreed: nothing is installed
        let mut no = Counting(0, false);
        assert!(install(&root, &url, Some("prog"), &mut no).unwrap_err().contains("not agreed"));
        assert!(!root.join("prog").exists());
        // the program is agreed to, the build is not: still nothing
        struct First(usize);
        impl Prompt for First {
            fn confirm(&mut self, _: &str) -> bool {
                self.0 += 1;
                self.0 == 1
            }
        }
        let mut first = First(0);
        assert!(install(&root, &url, Some("prog"), &mut first).unwrap_err().contains("build was not agreed"));
        assert!(!root.join("prog").exists());
        // both: installed, and the build ran in the plugin's directory
        let mut yes = Counting(0, true);
        install(&root, &url, Some("prog"), &mut yes).unwrap();
        assert_eq!(yes.0, 2);
        assert!(root.join("prog/built.txt").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_failed_build_leaves_nothing() {
        let base = tmp("fail");
        let root = base.join("root");
        let (_s, url) = repo(&base, "bad", "runtime = \"process\"\nexec = \"./run\"\nbuild = \"exit 3\"\n");
        assert!(install(&root, &url, Some("bad"), &mut Yes).unwrap_err().contains("build command failed"));
        assert!(!root.join("bad").exists());
        // not a plugin at all
        let dir = base.join("plain");
        std::fs::create_dir_all(&dir).unwrap();
        g(&dir, &["init", "-q"]);
        std::fs::write(dir.join("readme"), "x").unwrap();
        g(&dir, &["add", "."]);
        g(&dir, &["commit", "-q", "-m", "x"]);
        assert!(install(&root, &format!("file://{}", dir.display()), None, &mut Yes).unwrap_err().contains("not a plugin"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
