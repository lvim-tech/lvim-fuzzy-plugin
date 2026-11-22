//! [`Host`]: starts plugins on their first call, sends them calls, collects their answers.
//!
//! The tool (lvc, lfz, the picker's helper) owns one `Host` and does three things with it: [`Host::call`]
//! when something is to run, [`Host::poll`] in its loop (answers, deaths and time-outs come out as [`Out`]),
//! and — for the processes that asked — [`Host::pump_events`] / [`Host::stop`]. `wake` is a channel the tool
//! sleeps on: every answer sends `()` on it, so nothing waits for a tick.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::procplug::{self, Msg, Proc};
use crate::{PluginDef, Runtime};

/// How long a call may take to answer before it is reported as timed out.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// What came out of the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// A plugin answered: `text` is a JSON array of instructions. `id` is the call's id (from
    /// [`Host::call`]); `None` for what a plugin did on its own (a Lua `run` callback). `call` is the function
    /// that was called (empty when `id` is `None`).
    Answer { id: Option<u64>, plugin: String, call: String, text: String },
    /// A call failed (an error answer, a death, a time-out): a one-line text naming the plugin.
    Failed { id: Option<u64>, text: String },
}

struct Pending {
    plugin: String,
    call: String,
    until: Instant,
}

pub struct Host {
    defs: Vec<PluginDef>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    wake: Sender<()>,
    procs: HashMap<String, Proc>,
    #[cfg(feature = "native")]
    natives: HashMap<String, std::sync::Arc<std::sync::Mutex<crate::native::Lib>>>,
    #[cfg(feature = "wasm")]
    wasms: HashMap<String, std::sync::Arc<crate::wasm::Wasm>>,
    #[cfg(feature = "lua")]
    luas: HashMap<String, crate::luaplug::LuaPlugin>,
    pending: HashMap<u64, Pending>,
    next_req: u64,
    next_gen: u64,
    /// What each process was last told (`cd` / `cursor` / `selection` events send on a change).
    ev_last: HashMap<String, (String, String, String)>,
    /// Each plugin's settings, the layers merged (see [`crate::config`]); sent in every call as `ctx.config`.
    configs: HashMap<String, Value>,
}

impl Host {
    pub fn new(defs: Vec<PluginDef>, wake: Sender<()>) -> Host {
        let (tx, rx) = channel();
        Host {
            defs,
            tx,
            rx,
            wake,
            procs: HashMap::new(),
            #[cfg(feature = "native")]
            natives: HashMap::new(),
            #[cfg(feature = "wasm")]
            wasms: HashMap::new(),
            #[cfg(feature = "lua")]
            luas: HashMap::new(),
            pending: HashMap::new(),
            next_req: 1,
            next_gen: 1,
            ev_last: HashMap::new(),
            configs: HashMap::new(),
        }
    }

    /// Work out `plugin`'s settings: its defaults, the shared override file `<root>/<plugin>.toml`, then THIS
    /// tool's own override (`host_override`, from the tool's own config). The result goes to the plugin in every
    /// call as `ctx.config`. Err = the shared file is broken (the plugin then gets defaults and the host's own).
    pub fn configure(&mut self, plugin: &str, root: &std::path::Path, host_override: &toml::Table) -> Result<(), String> {
        let def = self.def(plugin).ok_or_else(|| format!("{plugin}: no such plugin"))?;
        let (shared, err) = match crate::config::shared_override(root, plugin) {
            Ok(t) => (t, None),
            Err(e) => (toml::Table::new(), Some(e)),
        };
        let v = crate::config::effective(&def.config, &shared, host_override);
        self.configs.insert(plugin.to_string(), v);
        err.map_or(Ok(()), Err)
    }

    /// [`Host::configure`] for every plugin; `host_plugins` is the tool's `[plugins]` table (a table per plugin).
    /// Returns the problems (broken override files).
    pub fn configure_all(&mut self, root: &std::path::Path, host_plugins: &toml::Table) -> Vec<String> {
        let names: Vec<String> = self.defs.iter().map(|d| d.name.clone()).collect();
        let empty = toml::Table::new();
        let mut errs = Vec::new();
        for n in names {
            let own = host_plugins.get(&n).and_then(|v| v.as_table()).unwrap_or(&empty);
            if let Err(e) = self.configure(&n, root, own) {
                errs.push(e);
            }
        }
        errs
    }

    /// The settings `plugin` is called with (its defaults until [`Host::configure`] is called).
    pub fn config_of(&self, plugin: &str) -> Value {
        self.configs
            .get(plugin)
            .cloned()
            .or_else(|| self.def(plugin).and_then(|d| serde_json::to_value(&d.config).ok()))
            .unwrap_or(Value::Null)
    }

    pub fn defs(&self) -> &[PluginDef] {
        &self.defs
    }

    pub fn def(&self, plugin: &str) -> Option<&PluginDef> {
        self.defs.iter().find(|p| p.name == plugin)
    }

    pub fn runtime_of(&self, plugin: &str) -> Option<Runtime> {
        self.def(plugin).map(|p| p.runtime)
    }

    /// Call function `call` of `plugin` with `ctx` (the host's context JSON, sent as it is). `dirs` are the
    /// directories a `wasm` module may see. Ok = the call's id: its answer comes out of [`Host::poll`].
    /// Err = the call could not even start (a one-line text naming the plugin).
    pub fn call(&mut self, plugin: &str, call: &str, ctx: Value, dirs: &[PathBuf]) -> Result<u64, String> {
        let def = self.def(plugin).ok_or_else(|| format!("{plugin}: no such plugin"))?.clone();
        let mut ctx = ctx;
        if let Some(o) = ctx.as_object_mut() {
            o.insert("config".into(), self.config_of(plugin));
        }
        let ctx = ctx;
        let id = self.next_req;
        match def.runtime {
            Runtime::Process => self.call_process(&def, id, &ctx)?,
            Runtime::Native => self.call_native(&def, id, call, &ctx)?,
            Runtime::Wasm => self.call_wasm(&def, id, call, &ctx, dirs)?,
            Runtime::Lua => self.call_lua(&def, id, call, ctx)?,
            Runtime::Command => return Err(format!("{plugin}: a `command` plugin has nothing to call")),
        }
        self.next_req += 1;
        self.pending.insert(id, Pending { plugin: plugin.to_string(), call: call.to_string(), until: Instant::now() + TIMEOUT });
        Ok(id)
    }

    /// Answers, deaths and time-outs since the last look.
    pub fn poll(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Keepalive { plugin, generation, id, secs } => {
                    // a call that waits for a person (a passphrase prompt) is given the time it asked for, from now
                    if self.procs.get(&plugin).is_some_and(|p| p.generation == generation)
                        && let Some(p) = self.pending.get_mut(&id)
                    {
                        p.until = Instant::now() + Duration::from_secs(secs.min(3600));
                    }
                }
                Msg::Reply { plugin, generation, id, result } => {
                    // a late answer of an older start, or to a call given up on, is dropped
                    if self.procs.get(&plugin).is_none_or(|p| p.generation != generation) {
                        continue;
                    }
                    self.answer(&mut out, id, plugin, result);
                }
                Msg::Done { plugin, id, result } => self.answer(&mut out, id, plugin, result),
                Msg::Async { plugin, result } => match result {
                    Ok(text) => out.push(Out::Answer { id: None, plugin, call: String::new(), text }),
                    Err(e) => out.push(Out::Failed { id: None, text: format!("{plugin}: {e}") }),
                },
                Msg::Died { plugin, generation } => {
                    if self.procs.get(&plugin).is_some_and(|p| p.generation == generation) {
                        self.procs.remove(&plugin);
                        let gone: Vec<u64> = self.pending.iter().filter(|(_, p)| p.plugin == plugin).map(|(i, _)| *i).collect();
                        for i in &gone {
                            self.pending.remove(i);
                            out.push(Out::Failed {
                                id: Some(*i),
                                text: format!("{plugin}: the process ended (it starts again on the next call)"),
                            });
                        }
                    }
                }
            }
        }
        let now = Instant::now();
        let late: Vec<u64> = self.pending.iter().filter(|(_, p)| now > p.until).map(|(i, _)| *i).collect();
        for i in late {
            if let Some(p) = self.pending.remove(&i) {
                out.push(Out::Failed { id: Some(i), text: format!("{}: {} timed out", p.plugin, p.call) });
            }
        }
        out
    }

    fn answer(&mut self, out: &mut Vec<Out>, id: u64, plugin: String, result: Result<String, String>) {
        let Some(p) = self.pending.remove(&id) else { return };
        match result {
            Ok(text) => out.push(Out::Answer { id: Some(id), plugin: p.plugin, call: p.call, text }),
            Err(e) => out.push(Out::Failed { id: Some(id), text: format!("{plugin}: {e}") }),
        }
    }

    /// `cd` / `cursor` / `selection` events to the running processes that asked for them, when the three
    /// strings (the directory, the file under the cursor, the marks) changed since they were last told.
    /// `params(event)` builds the event's JSON (the host's context plus `"event"`).
    pub fn pump_events(&mut self, now: &(String, String, String), params: impl Fn(&str) -> Value) {
        let names: Vec<String> = self.procs.keys().cloned().collect();
        for name in names {
            let Some(def) = self.defs.iter().find(|p| p.name == name) else { continue };
            if def.events.is_empty() {
                continue;
            }
            let Some(was) = self.ev_last.insert(name.clone(), now.clone()) else { continue }; // the first look
            let fire = |ev: &str, changed: bool| changed && def.events.iter().any(|e| e == ev);
            for ev in ["cd", "cursor", "selection"] {
                let changed = match ev {
                    "cd" => was.0 != now.0,
                    "cursor" => was.1 != now.1 || was.0 != now.0,
                    _ => was.2 != now.2,
                };
                if fire(ev, changed)
                    && let Some(p) = self.procs.get(&name)
                {
                    p.send(procplug::event(&params(ev)));
                }
            }
        }
    }

    /// At the end: `quit` to the processes that asked for it, then every plugin is closed.
    pub fn stop(&mut self, params: impl Fn(&str) -> Value) {
        for (name, p) in std::mem::take(&mut self.procs) {
            if self.defs.iter().any(|d| d.name == name && d.events.iter().any(|e| e == "quit")) {
                p.send(procplug::event(&params("quit")));
            }
            p.shutdown();
        }
    }

    // ── the runtimes ───────────────────────────────────────────────────────────────────────────────

    fn call_process(&mut self, def: &PluginDef, id: u64, ctx: &Value) -> Result<(), String> {
        if let Some(p) = self.procs.get_mut(&def.name)
            && p.exited()
        {
            self.procs.remove(&def.name);
        }
        if !self.procs.contains_key(&def.name) {
            let exec = def.exec.clone().ok_or_else(|| format!("{}: no `exec`", def.name))?;
            let generation = self.next_gen;
            self.next_gen += 1;
            let p = Proc::spawn(&def.name, &def.dir, &exec, generation, self.tx.clone(), self.wake.clone())
                .map_err(|e| format!("{}: cannot start `{exec}`: {e}", def.name))?;
            self.procs.insert(def.name.clone(), p);
            self.ev_last.remove(&def.name);
        }
        if self.procs[&def.name].send(procplug::request(id, ctx)) {
            Ok(())
        } else {
            self.procs.remove(&def.name);
            Err(format!("{}: the process is gone (it starts again on the next call)", def.name))
        }
    }

    #[cfg(feature = "native")]
    fn call_native(&mut self, def: &PluginDef, id: u64, call: &str, ctx: &Value) -> Result<(), String> {
        use std::sync::{Arc, Mutex};
        let lib = match self.natives.get(&def.name) {
            Some(l) => l.clone(),
            None => {
                let rel = def.library.clone().ok_or_else(|| format!("{}: no `library`", def.name))?;
                let lib = crate::native::Lib::open(&def.dir.join(rel)).map_err(|e| format!("{}: {e}", def.name))?;
                let lib = Arc::new(Mutex::new(lib));
                self.natives.insert(def.name.clone(), lib.clone());
                lib
            }
        };
        let (tx, wake, plugin, call, ctx) = (self.tx.clone(), self.wake.clone(), def.name.clone(), call.to_string(), ctx.to_string());
        // a roomy stack: native plugin code may be deep (the default 2 MB is small)
        std::thread::Builder::new()
            .stack_size(16 << 20)
            .spawn(move || {
                let result = match lib.lock() {
                    Ok(l) => l.call(&call, &ctx),
                    Err(_) => Err("an earlier call crashed in the plugin".to_string()),
                };
                let _ = tx.send(Msg::Done { plugin, id, result });
                let _ = wake.send(());
            })
            .map(|_| ())
            .map_err(|_| format!("{}: cannot start a thread for the plugin", def.name))
    }

    #[cfg(not(feature = "native"))]
    fn call_native(&mut self, def: &PluginDef, _: u64, _: &str, _: &Value) -> Result<(), String> {
        Err(format!("{}: this build has no `native` runtime", def.name))
    }

    #[cfg(feature = "wasm")]
    fn call_wasm(&mut self, def: &PluginDef, id: u64, call: &str, ctx: &Value, dirs: &[PathBuf]) -> Result<(), String> {
        use std::sync::Arc;
        let module = match self.wasms.get(&def.name) {
            Some(m) => m.clone(),
            None => {
                let rel = def.module.clone().ok_or_else(|| format!("{}: no `module`", def.name))?;
                let m = Arc::new(crate::wasm::Wasm::open(&def.dir.join(rel)).map_err(|e| format!("{}: {e}", def.name))?);
                self.wasms.insert(def.name.clone(), m.clone());
                m
            }
        };
        let (tx, wake, plugin, call, ctx, dirs, write) =
            (self.tx.clone(), self.wake.clone(), def.name.clone(), call.to_string(), ctx.to_string(), dirs.to_vec(), def.allow_write);
        // a big stack: the interpreter and WASI nest deeply (the default 2 MB overflowed in an optimized build)
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                let result = module.call(&call, &ctx, &dirs, write);
                let _ = tx.send(Msg::Done { plugin, id, result });
                let _ = wake.send(());
            })
            .map(|_| ())
            .map_err(|_| format!("{}: cannot start a thread for the plugin", def.name))
    }

    #[cfg(not(feature = "wasm"))]
    fn call_wasm(&mut self, def: &PluginDef, _: u64, _: &str, _: &Value, _: &[PathBuf]) -> Result<(), String> {
        Err(format!("{}: this build has no `wasm` runtime", def.name))
    }

    #[cfg(feature = "lua")]
    fn call_lua(&mut self, def: &PluginDef, id: u64, call: &str, ctx: Value) -> Result<(), String> {
        if !self.luas.contains_key(&def.name) {
            let script = def.dir.join(def.script.clone().unwrap_or_else(|| "init.lua".into()));
            let l = crate::luaplug::LuaPlugin::start(&def.name, &def.dir, &script, self.tx.clone(), self.wake.clone())
                .map_err(|e| format!("{}: {e}", def.name))?;
            self.luas.insert(def.name.clone(), l);
        }
        if self.luas[&def.name].call(id, call, ctx) {
            Ok(())
        } else {
            self.luas.remove(&def.name);
            Err(format!("{}: the Lua thread is gone (it starts again on the next call)", def.name))
        }
    }

    #[cfg(not(feature = "lua"))]
    fn call_lua(&mut self, def: &PluginDef, _: u64, _: &str, _: Value) -> Result<(), String> {
        Err(format!("{}: this build has no `lua` runtime", def.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugin(root: &std::path::Path, name: &str, toml: &str, files: &[(&str, &str)]) -> PluginDef {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), toml).unwrap();
        for (f, c) in files {
            std::fs::write(dir.join(f), c).unwrap();
        }
        crate::manifest::load_all(root).0.into_iter().find(|m| m.def.name == name).unwrap().def
    }

    fn wait(host: &mut Host, rx: &Receiver<()>) -> Vec<Out> {
        for _ in 0..80 {
            let _ = rx.recv_timeout(Duration::from_millis(250));
            let o = host.poll();
            if !o.is_empty() {
                return o;
            }
        }
        panic!("no answer");
    }

    #[test]
    fn calls_a_process_and_a_lua_plugin_through_one_host() {
        let root = std::env::temp_dir().join(format!("lvim-plugin-host-{}", std::process::id()));
        // a shell "process": answers each request with a message naming the call; `die` ends it
        let echo = "while read l; do case \"$l\" in *die*) exit 0;; esac; id=$(printf %s \"$l\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p'); [ -n \"$id\" ] && printf '{\"id\":%s,\"result\":[{\"op\":\"message\",\"text\":\"pong\"}]}\\n' \"$id\"; done";
        let p = plugin(&root, "sh", &format!("runtime = \"process\"\nexec = {echo:?}\n"), &[]);
        let l = plugin(&root, "lu", "runtime = \"lua\"\n", &[("init.lua", "function hi(ctx) lvim.notify('lua ' .. ctx.dir) end")]);
        let (wake, rx) = channel();
        let mut host = Host::new(vec![p, l], wake);

        let a = host.call("sh", "x", json!({"call": "x"}), &[]).unwrap();
        let out = wait(&mut host, &rx);
        assert_eq!(out, vec![Out::Answer { id: Some(a), plugin: "sh".into(), call: "x".into(), text: r#"[{"op":"message","text":"pong"}]"#.into() }]);

        let b = host.call("lu", "hi", json!({"call": "hi", "dir": "/d"}), &[]).unwrap();
        assert_ne!(a, b);
        let out = wait(&mut host, &rx);
        assert_eq!(out, vec![Out::Answer { id: Some(b), plugin: "lu".into(), call: "hi".into(), text: r#"[{"op":"message","text":"lua /d"}]"#.into() }]);

        // a plugin that does not exist, a function that does not exist
        assert!(host.call("nope", "x", json!({}), &[]).unwrap_err().contains("no such plugin"));
        let c = host.call("lu", "missing", json!({"call": "missing"}), &[]).unwrap();
        match &wait(&mut host, &rx)[..] {
            [Out::Failed { id: Some(i), text }] => {
                assert_eq!(*i, c);
                assert!(text.contains("no such function"), "{text}");
            }
            other => panic!("{other:?}"),
        }

        // the process dies mid-call: the call is reported, and the next call starts it again
        let d = host.call("sh", "die", json!({"call": "die"}), &[]).unwrap();
        match &wait(&mut host, &rx)[..] {
            [Out::Failed { id: Some(i), text }] => {
                assert_eq!(*i, d);
                assert!(text.contains("the process ended"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        let e = host.call("sh", "x", json!({"call": "x"}), &[]).unwrap();
        assert!(matches!(&wait(&mut host, &rx)[..], [Out::Answer { id: Some(i), .. }] if *i == e));

        host.stop(|ev| json!({"event": ev}));
        for n in ["sh", "lu"] {
            let _ = std::fs::remove_file(root.join(n).join("plugin.toml"));
            let _ = std::fs::remove_file(root.join(n).join("init.lua"));
            let _ = std::fs::remove_dir(root.join(n));
        }
        let _ = std::fs::remove_dir(&root);
    }

    #[test]
    fn a_command_plugin_has_nothing_to_call() {
        let (wake, _rx) = channel();
        let def = PluginDef {
            name: "c".into(),
            description: String::new(),
            dir: PathBuf::from("/tmp"),
            runtime: Runtime::Command,
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
        };
        let mut host = Host::new(vec![def], wake);
        assert!(host.call("c", "x", json!({}), &[]).unwrap_err().contains("nothing to call"));
    }

    #[test]
    fn the_settings_reach_the_plugin_layered() {
        let root = std::env::temp_dir().join(format!("lvim-plugin-hostcfg-{}", std::process::id()));
        let l = plugin(
            &root,
            "cfg",
            "runtime = \"lua\"\n[config.pdf]\nrender = \"pdftoppm\"\ndpi = 100\n",
            &[("init.lua", "function show(ctx) lvim.notify(ctx.config.pdf.render .. ':' .. ctx.config.pdf.dpi .. ':' .. tostring(ctx.config.extra)) end")],
        );
        std::fs::write(root.join("cfg.toml"), "[pdf]\ndpi = 150\n").unwrap(); // the shared override file
        let (wake, rx) = channel();
        let mut host = Host::new(vec![l], wake);
        // no configure yet: the defaults
        host.call("cfg", "show", json!({"call": "show"}), &[]).unwrap();
        assert!(matches!(&wait(&mut host, &rx)[..], [Out::Answer { text, .. }] if text.contains("pdftoppm:100:nil")));
        // this tool overrides the render program; the shared file's dpi sits between
        let own: toml::Table = toml::from_str("[cfg.pdf]\nrender = \"mutool\"\n[cfg]\nextra = true\n").unwrap();
        assert!(host.configure_all(&root, &own).is_empty());
        host.call("cfg", "show", json!({"call": "show"}), &[]).unwrap();
        assert!(matches!(&wait(&mut host, &rx)[..], [Out::Answer { text, .. }] if text.contains("mutool:150:true")));
        // another tool, with no override of its own: the shared file over the defaults
        let (wake2, rx2) = channel();
        let mut other = Host::new(host.defs().to_vec(), wake2);
        assert!(other.configure_all(&root, &toml::Table::new()).is_empty());
        other.call("cfg", "show", json!({"call": "show"}), &[]).unwrap();
        assert!(matches!(&wait(&mut other, &rx2)[..], [Out::Answer { text, .. }] if text.contains("pdftoppm:150:nil")));
        // a broken shared file is reported, and the plugin still gets defaults + the tool's own
        std::fs::write(root.join("cfg.toml"), "[pdf").unwrap();
        assert_eq!(host.configure_all(&root, &own).len(), 1);
        for f in ["cfg/plugin.toml", "cfg/init.lua", "cfg.toml"] {
            let _ = std::fs::remove_file(root.join(f));
        }
        let _ = std::fs::remove_dir(root.join("cfg"));
        let _ = std::fs::remove_dir(&root);
    }
}
