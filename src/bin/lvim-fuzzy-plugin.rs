//! `lvim-plugin`: the plugin tool.
//!
//! ```text
//! lvim-plugin host [--root DIR] [--tool NAME]   serve plugins over stdin / stdout (JSON lines): how a tool that
//!                                               cannot link the host (the Neovim picker) runs them
//! lvim-plugin list [--root DIR] [--tool NAME]   the plugins found, what they are, whether they load
//! lvim-plugin check DIR                         a plugin developer's check of one plugin directory
//! lvim-plugin install GIT_URL [--name N] [--yes]  install a plugin by another developer (git URLs only)
//! lvim-plugin update [NAME] [--yes]             update the plugins installed from git (all, or one)
//! lvim-plugin remove NAME [--yes]               remove an installed plugin
//! ```
//!
//! `--root` is the plugins directory every tool shares (default `$XDG_CONFIG_HOME/lvim-plugins`, else
//! `~/.config/lvim-plugins`), `--tool` the tool the plugins are for (`lvc`, `lfz`, `picker`; default: all).
//! `install`, `update` and `remove` ask before they trust, build or delete anything; `--yes` answers yes (a run
//! with no terminal and no `--yes` answers no). A `wasm` plugin is sandboxed; any other runs with your rights.
//!
//! The `host` protocol, one JSON object per line:
//!
//! ```text
//! → {"id":1,"method":"list"}                                   ← {"id":1,"result":[{name, runtime, ..., "tables":{..}}]}
//! → {"id":2,"method":"call","plugin":"x","call":"f","params":{..},"dirs":["/a"]}
//!                                                              ← {"id":2,"result":[instructions]} | {"id":2,"error":"text"}
//! → {"id":3,"method":"config","plugin":"x"}                      ← {"id":3,"result":{the plugin's effective settings}}
//! → {"method":"quit"}                                          (quit events go out, then it exits)
//!                                  ← {"event":"answer","plugin":"x","result":[..]}   what a plugin did on its own
//!                                  ← {"event":"error","text":".."}
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

use lvim_fuzzy_plugin::{Host, Out, manifest};
use serde_json::{Value, json};

/// `--root DIR` and `--tool NAME` out of the arguments; what is left over is returned too.
fn options(args: &[String]) -> (PathBuf, Option<String>, Vec<String>) {
    let (mut root, mut tool, mut rest) = (lvim_fuzzy_plugin::default_root(), None, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => root = it.next().map(PathBuf::from).unwrap_or(root),
            "--tool" => tool = it.next().cloned(),
            _ => rest.push(a.clone()),
        }
    }
    (root, tool, rest)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("usage: lvim-plugin host | list | check DIR | install GIT_URL | update [NAME] | remove NAME   (see the top of src/bin/lvim-plugin.rs)");
        std::process::exit(2);
    };
    let code = match cmd.as_str() {
        "host" => host(rest),
        "list" => list(rest),
        "check" => check(rest),
        "install" => install(rest),
        "update" => update(rest),
        "remove" => remove(rest),
        other => {
            eprintln!("lvim-plugin: unknown command `{other}` (host, list, check, install, update, remove)");
            2
        }
    };
    std::process::exit(code);
}

/// With `--tool` the plugins for that tool; without, every plugin.
fn load(root: &Path, tool: &Option<String>) -> (Vec<manifest::Manifest>, Vec<String>) {
    match tool {
        Some(t) => manifest::load(root, t),
        None => manifest::load_all(root),
    }
}

fn list(args: &[String]) -> i32 {
    let (root, tool, _) = options(args);
    let (mans, errs) = load(&root, &tool);
    println!("plugins in {}:", root.display());
    for m in &mans {
        let d = &m.def;
        let hosts = if d.hosts.is_empty() { "all tools".to_string() } else { d.hosts.join(", ") };
        println!("  {}  [{}]  {}  (for {hosts})", d.name, d.runtime.name(), d.description);
        match lvim_fuzzy_plugin::status(d) {
            Ok(s) if !s.is_empty() => println!("      {s}"),
            Ok(_) => {}
            Err(e) => eprintln!("lvim-plugin: {e}"),
        }
        for m in lvim_fuzzy_plugin::missing_requires(&d.requires, &lvim_fuzzy_plugin::have) {
            println!("      ✗ missing on this machine: {m}");
        }
        if let Some(o) = lvim_fuzzy_plugin::install::origin(&d.dir) {
            println!("      from {o}");
        }
        let who = [d.version.clone(), d.author.clone(), d.license.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        if !who.is_empty() {
            println!("      {who}");
        }
    }
    for e in &errs {
        eprintln!("lvim-plugin: {e}");
    }
    i32::from(!errs.is_empty())
}

fn check(args: &[String]) -> i32 {
    let Some(dir) = args.first() else {
        eprintln!("usage: lvim-plugin check DIR");
        return 2;
    };
    let dir = Path::new(dir);
    let (Some(parent), Some(leaf)) = (dir.canonicalize().ok().and_then(|d| d.parent().map(Path::to_path_buf)), dir.file_name()) else {
        eprintln!("lvim-plugin: {}: not a directory", dir.display());
        return 2;
    };
    let (mans, errs) = manifest::load_all(&parent);
    let mut bad = !errs.is_empty();
    for e in errs.iter().filter(|e| e.contains(&*leaf.to_string_lossy())) {
        eprintln!("lvim-plugin: {e}");
    }
    match mans.iter().find(|m| m.def.dir.file_name() == Some(leaf)) {
        None => {
            eprintln!("lvim-plugin: {}: no valid plugin.toml", dir.display());
            return 1;
        }
        Some(m) => {
            let d = &m.def;
            println!("{}  [{}]  api {}", d.name, d.runtime.name(), d.api);
            match lvim_fuzzy_plugin::status(d) {
                Ok(s) if !s.is_empty() => println!("  {s}"),
                Ok(_) => {}
                Err(e) => {
                    eprintln!("lvim-plugin: {e}");
                    bad = true;
                }
            }
            for m in lvim_fuzzy_plugin::missing_requires(&d.requires, &lvim_fuzzy_plugin::have) {
                println!("  ✗ missing on this machine: {m}");
            }
            if d.description.is_empty() {
                println!("  note: no description");
            }
            if d.runtime != lvim_fuzzy_plugin::Runtime::Wasm {
                println!("  note: a {} plugin runs with the user's rights; `wasm` is the sandboxed, portable choice", d.runtime.name());
            }
        }
    }
    i32::from(bad)
}

/// `--yes` answers every question yes; otherwise the terminal is asked, and with none the answer is no.
fn prompt(args: &[String]) -> Box<dyn lvim_fuzzy_plugin::install::Prompt> {
    use lvim_fuzzy_plugin::install::{No, Terminal, Yes};
    use std::io::IsTerminal;
    if args.iter().any(|a| a == "--yes" || a == "-y") {
        Box::new(Yes)
    } else if std::io::stdin().is_terminal() {
        Box::new(Terminal)
    } else {
        eprintln!("lvim-plugin: no terminal to ask on (use --yes to agree)");
        Box::new(No)
    }
}

fn install(args: &[String]) -> i32 {
    let (root, _, rest) = options(args);
    let mut name = None;
    let mut positional = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.next().cloned(),
            "--yes" | "-y" => {}
            _ => positional.push(a.clone()),
        }
    }
    let Some(url) = positional.first() else {
        eprintln!("usage: lvim-plugin install GIT_URL [--name N] [--yes]");
        return 2;
    };
    match lvim_fuzzy_plugin::install::install(&root, url, name.as_deref(), &mut *prompt(args)) {
        Ok(m) => {
            println!("{m}");
            0
        }
        Err(e) => {
            eprintln!("lvim-plugin: {e}");
            1
        }
    }
}

fn update(args: &[String]) -> i32 {
    let (root, _, rest) = options(args);
    let name = rest.iter().find(|a| !a.starts_with('-')).cloned();
    let lines = lvim_fuzzy_plugin::install::update(&root, name.as_deref(), &mut *prompt(args));
    let bad = lines.iter().any(|l| l.contains("NOT trusted") || l.contains("not installed") || l.contains("broken") || l.contains("git "));
    for l in &lines {
        println!("{l}");
    }
    i32::from(bad)
}

fn remove(args: &[String]) -> i32 {
    let (root, _, rest) = options(args);
    let Some(name) = rest.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("usage: lvim-plugin remove NAME [--yes]");
        return 2;
    };
    match lvim_fuzzy_plugin::install::remove(&root, name, &mut *prompt(args)) {
        Ok(m) => {
            println!("{m}");
            0
        }
        Err(e) => {
            eprintln!("lvim-plugin: {e}");
            1
        }
    }
}

fn to_json_text(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

fn host(args: &[String]) -> i32 {
    let (root, tool, rest) = options(args);
    // `--plugins JSON`: the asking tool's own settings per plugin (the `[plugins.<name>]` of its config), as a JSON object
    let mut own = toml::Table::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--plugins" {
            match it.next().map(|v| serde_json::from_str::<toml::Table>(v)) {
                Some(Ok(t)) => own = t,
                _ => {
                    eprintln!("lvim-plugin: --plugins takes a JSON object: {{\"<plugin>\": {{...}}}}");
                    return 2;
                }
            }
        }
    }
    let (mans, errs) = load(&root, &tool);
    for e in &errs {
        eprintln!("lvim-plugin: {e}");
    }
    let listing: Value = mans
        .iter()
        .map(|m| {
            let d = &m.def;
            json!({
                "name": d.name, "description": d.description, "runtime": d.runtime.name(), "hosts": d.hosts,
                "version": d.version, "author": d.author, "license": d.license, "dir": d.dir,
                "tables": serde_json::to_value(&m.table).unwrap_or(Value::Null),
            })
        })
        .collect();
    let (wake_tx, wake_rx) = channel::<()>();
    let (line_tx, line_rx) = channel::<String>();
    {
        let wake = wake_tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for l in std::io::stdin().lock().lines() {
                let Ok(l) = l else { break };
                if line_tx.send(l).is_err() {
                    break;
                }
                let _ = wake.send(());
            }
            // stdin closed: the same as `quit`
            let _ = line_tx.send(r#"{"method":"quit"}"#.to_string());
            let _ = wake.send(());
        });
    }
    let mut host = Host::new(mans.into_iter().map(|m| m.def).collect(), wake_tx);
    // each plugin's settings: its defaults, the shared override file, then this tool's own
    for e in host.configure_all(&root, &own) {
        eprintln!("lvim-plugin: {e}");
    }
    let mut ids: HashMap<u64, Value> = HashMap::new();
    let say = |v: Value| {
        use std::io::Write;
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    };
    loop {
        let _ = wake_rx.recv_timeout(Duration::from_millis(250));
        while let Ok(line) = line_rx.try_recv() {
            let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            match req["method"].as_str() {
                Some("list") => say(json!({"id": id, "result": listing})),
                // the plugin's effective settings (defaults + the shared override + this tool's): what a stand-alone job of it needs
                Some("config") => match req["plugin"].as_str() {
                    Some(p) => say(json!({"id": id, "result": host.config_of(p)})),
                    None => say(json!({"id": id, "error": "config needs `plugin`"})),
                },
                Some("call") => {
                    let (Some(plugin), Some(call)) = (req["plugin"].as_str(), req["call"].as_str()) else {
                        say(json!({"id": id, "error": "call needs `plugin` and `call`"}));
                        continue;
                    };
                    let dirs: Vec<PathBuf> =
                        req["dirs"].as_array().map(|a| a.iter().filter_map(|d| d.as_str().map(PathBuf::from)).collect()).unwrap_or_default();
                    match host.call(plugin, call, req["params"].clone(), &dirs) {
                        Ok(hid) => {
                            ids.insert(hid, id);
                        }
                        Err(e) => say(json!({"id": id, "error": e})),
                    }
                }
                Some("quit") => {
                    host.stop(|ev| json!({"event": ev}));
                    return 0;
                }
                _ => {}
            }
        }
        for out in host.poll() {
            match out {
                Out::Answer { id: Some(h), text, .. } => {
                    if let Some(id) = ids.remove(&h) {
                        say(json!({"id": id, "result": to_json_text(&text)}));
                    }
                }
                Out::Answer { id: None, plugin, text, .. } => say(json!({"event": "answer", "plugin": plugin, "result": to_json_text(&text)})),
                Out::Failed { id: Some(h), text } => {
                    if let Some(id) = ids.remove(&h) {
                        say(json!({"id": id, "error": text}));
                    }
                }
                Out::Failed { id: None, text } => say(json!({"event": "error", "text": text})),
            }
        }
    }
}
