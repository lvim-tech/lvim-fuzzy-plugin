//! `runtime = "lua"`: a plugin written in Lua — an `init.lua` in the plugin's directory, run by an embedded
//! Lua 5.4 (`mlua`, built in: nothing to install). Actions say `call = "function"`. The API is the `lvim`
//! table (`lvc` is an alias): `lvim.notify(...)` and so on.
//!
//! `init.lua` runs once, when the plugin is first used; its globals (or the table it returns) are the plugin's
//! functions, and its state lives on between calls. A function gets one argument, the context table (`file`,
//! `files`, `dir`, `other_dir`, `panel`, `arg`), and gives instructions back through the `lvc` table:
//!
//! ```lua
//! lvc.cd(path)  lvc.other_cd(path)  lvc.select(name)  lvc.mark(name)  lvc.reload()
//! lvc.notify(text)  lvc.error(text)  lvc.preview(text)
//! lvc.input(prompt, "then_function", default)   lvc.confirm(text, "then_function")
//! lvc.menu(title, { { label = "One", call = "one", arg = "x" }, ... })
//! lvc.run({ "git", "status" }, function(code, stdout, stderr) ... end)   -- in the background
//! lvc.current()  lvc.marked()  lvc.dirs()                                 -- what is selected now
//! ```
//!
//! (a function may also return a string — a message — or a list of instruction tables). Everything runs on a
//! thread of its own, one call at a time, so the interface never waits; a call that runs more than
//! [`LIMIT`] instructions is stopped with an error (an endless loop cannot freeze anything), and the memory
//! is capped at [`MEMORY`]. A Lua plugin has the Lua standard library (files, `os.execute`) with the user's
//! rights — as a shell-script plugin has; for a sandbox use `runtime = "wasm"`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::{Function, HookTriggers, Lua, MultiValue, Table, Value as LuaValue, VmState};
use serde_json::{Value, json};

use crate::procplug::Msg;

/// Instructions one call may run (an endless loop ends with an error in about a second).
pub const LIMIT: u64 = 200_000_000;
/// The memory a plugin's Lua state may use.
pub const MEMORY: usize = 256 << 20;
/// The hook fires every this many instructions.
const STEP: u64 = 10_000;

enum Job {
    /// A call from an action: the request `id`, the function, the context.
    Call { id: u64, name: String, ctx: Value },
    /// A background `lvc.run` finished: its callback's key, the exit code, stdout, stderr.
    Done { cb: u64, code: i64, out: String, err: String },
}

/// A running Lua plugin (its thread ends when this is dropped).
pub struct LuaPlugin {
    tx: Sender<Job>,
}

type Instrs = Arc<Mutex<Vec<Value>>>;

fn json_of_lua(v: &LuaValue) -> Value {
    match v {
        LuaValue::Nil => Value::Null,
        LuaValue::Boolean(b) => json!(b),
        LuaValue::Integer(i) => json!(i),
        LuaValue::Number(n) => json!(n),
        LuaValue::String(s) => json!(s.to_string_lossy().to_string()),
        LuaValue::Table(t) => {
            let n = t.raw_len();
            if n > 0 {
                Value::Array(t.sequence_values::<LuaValue>().flatten().map(|x| json_of_lua(&x)).collect())
            } else {
                let mut m = serde_json::Map::new();
                for (k, x) in t.pairs::<String, LuaValue>().flatten() {
                    m.insert(k, json_of_lua(&x));
                }
                Value::Object(m)
            }
        }
        _ => Value::Null,
    }
}

fn lua_of_json(lua: &Lua, v: &Value) -> mlua::Result<LuaValue> {
    Ok(match v {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Number(n) => n.as_i64().map(LuaValue::Integer).unwrap_or_else(|| LuaValue::Number(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => LuaValue::String(lua.create_string(s)?),
        Value::Array(a) => {
            let t = lua.create_table()?;
            for (i, x) in a.iter().enumerate() {
                t.raw_set(i + 1, lua_of_json(lua, x)?)?;
            }
            LuaValue::Table(t)
        }
        Value::Object(m) => {
            let t = lua.create_table()?;
            for (k, x) in m {
                t.raw_set(k.as_str(), lua_of_json(lua, x)?)?;
            }
            LuaValue::Table(t)
        }
    })
}

/// The `lvc` table: each function pushes an instruction (or, `lvc.run`, starts a background process).
fn make_api(
    lua: &Lua,
    instrs: Instrs,
    ctx: Arc<Mutex<Value>>,
    callbacks: Arc<Mutex<HashMap<u64, mlua::RegistryKey>>>,
    next_cb: Arc<AtomicU64>,
    me: Sender<Job>,
    wake: Sender<()>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let push = |name: &'static str, op: &'static str, field: &'static str| -> mlua::Result<()> {
        let i = instrs.clone();
        t.set(
            name,
            lua.create_function(move |_, text: String| {
                i.lock().unwrap().push(json!({ "op": op, field: text }));
                Ok(())
            })?,
        )
    };
    push("cd", "cd", "path")?;
    push("other_cd", "other-cd", "path")?;
    push("select", "select", "name")?;
    push("mark", "mark", "name")?;
    push("notify", "message", "text")?;
    push("error", "error", "text")?;
    push("preview", "preview", "text")?;
    {
        let i = instrs.clone();
        t.set("reload", lua.create_function(move |_, ()| {
            i.lock().unwrap().push(json!({ "op": "reload" }));
            Ok(())
        })?)?;
    }
    {
        let i = instrs.clone();
        t.set(
            "input",
            lua.create_function(move |_, (prompt, then, value): (String, String, Option<String>)| {
                i.lock().unwrap().push(json!({ "op": "input", "prompt": prompt, "then": then, "value": value.unwrap_or_default() }));
                Ok(())
            })?,
        )?;
    }
    {
        let i = instrs.clone();
        t.set(
            "confirm",
            lua.create_function(move |_, (text, then): (String, String)| {
                i.lock().unwrap().push(json!({ "op": "confirm", "text": text, "then": then }));
                Ok(())
            })?,
        )?;
    }
    {
        let i = instrs.clone();
        t.set(
            "menu",
            lua.create_function(move |_, (title, items): (String, Table)| {
                let items = json_of_lua(&LuaValue::Table(items));
                i.lock().unwrap().push(json!({ "op": "menu", "title": title, "items": items }));
                Ok(())
            })?,
        )?;
    }
    {
        let c = ctx.clone();
        t.set("current", lua.create_function(move |_, ()| Ok(c.lock().unwrap()["file"].as_str().map(str::to_string)))?)?;
        let c = ctx.clone();
        t.set("marked", lua.create_function(move |_, ()| {
            Ok(c.lock().unwrap()["files"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default())
        })?)?;
        let c = ctx;
        t.set("dirs", lua.create_function(move |lua, ()| {
            let c = c.lock().unwrap();
            let d = lua.create_table()?;
            d.set("dir", c["dir"].as_str().map(str::to_string))?;
            d.set("other", c["other_dir"].as_str().map(str::to_string))?;
            d.set("panel", c["panel"].as_str().map(str::to_string))?;
            Ok(d)
        })?)?;
    }
    // lvc.run(argv, callback): the process runs in the background; the callback gets (code, stdout, stderr)
    t.set(
        "run",
        lua.create_function(move |lua, (argv, cb): (Vec<String>, Function)| {
            let Some((prog, args)) = argv.split_first() else {
                return Err(mlua::Error::runtime("lvc.run: an empty command"));
            };
            let key = lua.create_registry_value(cb)?;
            let id = next_cb.fetch_add(1, Ordering::Relaxed);
            callbacks.lock().unwrap().insert(id, key);
            let (prog, args, me, wake) = (prog.clone(), args.to_vec(), me.clone(), wake.clone());
            std::thread::spawn(move || {
                let done = match std::process::Command::new(&prog).args(&args).stdin(std::process::Stdio::null()).output() {
                    Ok(o) => Job::Done {
                        cb: id,
                        code: o.status.code().unwrap_or(-1) as i64,
                        out: String::from_utf8_lossy(&o.stdout).into_owned(),
                        err: String::from_utf8_lossy(&o.stderr).into_owned(),
                    },
                    Err(e) => Job::Done { cb: id, code: -1, out: String::new(), err: format!("{prog}: {e}") },
                };
                let _ = me.send(done);
                let _ = wake.send(());
            });
            Ok(())
        })?,
    )?;
    Ok(t)
}

/// Run `f` with the instruction budget reset; its collected instructions (and what it returned) as JSON text.
fn invoke(
    instrs: &Instrs,
    fired: &AtomicU64,
    f: impl FnOnce() -> mlua::Result<MultiValue>,
) -> Result<String, String> {
    instrs.lock().unwrap().clear();
    fired.store(0, Ordering::Relaxed);
    let ret = f().map_err(|e| {
        let m = e.to_string();
        if m.contains("lvc: instruction limit") {
            "the script ran too long and was stopped".to_string()
        } else {
            m.lines().next().unwrap_or("error").to_string()
        }
    })?;
    let mut out = std::mem::take(&mut *instrs.lock().unwrap());
    // a returned string is a message, a returned list is more instructions
    for v in ret.iter() {
        match v {
            LuaValue::String(s) => out.push(json!({ "op": "message", "text": s.to_string_lossy().to_string() })),
            LuaValue::Table(t) => {
                if let Value::Array(a) = json_of_lua(&LuaValue::Table(t.clone())) {
                    out.extend(a);
                }
            }
            _ => {}
        }
    }
    Ok(Value::Array(out).to_string())
}

impl LuaPlugin {
    /// Load `script` in a new Lua state on its own thread. The script's top level runs here (and waits for
    /// it: a load error is the error text). Answers go to `out` as [`Msg`]s, each with a wake-up.
    pub fn start(plugin: &str, dir: &Path, script: &Path, out: Sender<Msg>, wake: Sender<()>) -> Result<LuaPlugin, String> {
        let src = std::fs::read_to_string(script).map_err(|e| format!("{}: {e}", script.display()))?;
        let (tx, rx) = channel::<Job>();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        let (name, dir, me) = (plugin.to_string(), dir.to_path_buf(), tx.clone());
        let chunk_name = format!("@{}", script.display());
        std::thread::Builder::new()
            .name(format!("lua-{plugin}"))
            .stack_size(16 << 20)
            .spawn(move || {
                let lua = Lua::new();
                let _ = lua.set_memory_limit(MEMORY);
                let instrs: Instrs = Default::default();
                let ctx = Arc::new(Mutex::new(Value::Null));
                let callbacks = Arc::new(Mutex::new(HashMap::new()));
                let fired = Arc::new(AtomicU64::new(0));
                let setup = (|| -> mlua::Result<Option<Table>> {
                    let f = fired.clone();
                    lua.set_hook(HookTriggers { every_nth_instruction: Some(STEP as u32), ..Default::default() }, move |_, _| {
                        if f.fetch_add(STEP, Ordering::Relaxed) + STEP > LIMIT {
                            Err(mlua::Error::runtime("lvc: instruction limit"))
                        } else {
                            Ok(VmState::Continue)
                        }
                    })?;
                    let api = make_api(&lua, instrs.clone(), ctx.clone(), callbacks.clone(), Arc::new(AtomicU64::new(1)), me, wake.clone())?;
                    // `lvim` is the name; `lvc` stays as an alias
                    lua.globals().set("lvc", api.clone())?;
                    lua.globals().set("lvim", api)?;
                    // `require "x"` finds x.lua beside init.lua
                    let package: Table = lua.globals().get("package")?;
                    let path: String = package.get("path")?;
                    package.set("path", format!("{}/?.lua;{path}", dir.display()))?;
                    fired.store(0, Ordering::Relaxed);
                    let ret: LuaValue = lua.load(&src).set_name(chunk_name).eval()?;
                    Ok(match ret {
                        LuaValue::Table(t) => Some(t),
                        _ => None,
                    })
                })();
                let module = match setup {
                    Ok(m) => {
                        let _ = ready_tx.send(Ok(()));
                        m
                    }
                    Err(e) => {
                        let m = e.to_string();
                        let m = if m.contains("lvc: instruction limit") {
                            "the script ran too long and was stopped".to_string()
                        } else {
                            m.lines().next().unwrap_or("error").to_string()
                        };
                        let _ = ready_tx.send(Err(m));
                        return;
                    }
                };
                let find = |lua: &Lua, name: &str| -> Option<Function> {
                    module.as_ref().and_then(|m| m.get::<Function>(name).ok()).or_else(|| lua.globals().get::<Function>(name).ok())
                };
                for job in rx {
                    match job {
                        Job::Call { id, name: call, ctx: c } => {
                            *ctx.lock().unwrap() = c.clone();
                            let result = match find(&lua, &call) {
                                None => Err(format!("no such function: {call}")),
                                Some(func) => invoke(&instrs, &fired, || {
                                    let arg = lua_of_json(&lua, &c)?;
                                    func.call::<MultiValue>(arg)
                                }),
                            };
                            let _ = out.send(Msg::Done { plugin: name.clone(), id, result });
                        }
                        Job::Done { cb, code, out: so, err } => {
                            let key = callbacks.lock().unwrap().remove(&cb);
                            let Some(key) = key else { continue };
                            let result = invoke(&instrs, &fired, || {
                                let f: Function = lua.registry_value(&key)?;
                                f.call::<MultiValue>((code, so, err))
                            });
                            let _ = lua.remove_registry_value(key);
                            let _ = out.send(Msg::Async { plugin: name.clone(), result });
                        }
                    }
                    let _ = wake.send(());
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|_| "the Lua thread died while loading".to_string())??;
        Ok(LuaPlugin { tx })
    }

    /// Queue a call; false when the plugin's thread is gone.
    pub fn call(&self, id: u64, name: &str, ctx: Value) -> bool {
        self.tx.send(Job::Call { id, name: name.to_string(), ctx }).is_ok()
    }
}

/// Does the script compile? (syntax only: nothing of it runs)
pub fn check(script: &Path) -> Result<(), String> {
    let src = std::fs::read_to_string(script).map_err(|e| format!("{}: {e}", script.display()))?;
    Lua::new()
        .load(&src)
        .set_name(format!("@{}", script.display()))
        .into_function()
        .map(|_| ())
        .map_err(|e| e.to_string().lines().next().unwrap_or("error").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    fn plugin(script: &str) -> (LuaPlugin, Receiver<Msg>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lvc-lua-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')'], ""));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("init.lua"), script).unwrap();
        std::fs::write(dir.join("helper.lua"), "return { twice = function(x) return x * 2 end }").unwrap();
        let (out, rx) = channel();
        let (wake, _w) = channel();
        let p = LuaPlugin::start("t", &dir, &dir.join("init.lua"), out, wake).unwrap();
        (p, rx, dir)
    }

    fn ask(p: &LuaPlugin, rx: &Receiver<Msg>, id: u64, name: &str, ctx: Value) -> Result<String, String> {
        assert!(p.call(id, name, ctx));
        match rx.recv_timeout(Duration::from_secs(20)).unwrap() {
            Msg::Done { id: got, result, .. } => {
                assert_eq!(got, id);
                result
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn calls_return_instructions_and_state_lives_on() {
        let (p, rx, dir) = plugin(
            r#"
            local helper = require "helper"
            local n = 0
            function hello(ctx)
              n = n + 1
              lvc.notify("hello " .. (ctx.file or "nothing") .. " #" .. n .. " " .. helper.twice(21))
              lvc.cd("/tmp")
            end
            function asks(ctx) lvc.input("Name", "greet", "x"); lvc.confirm("Sure?", "go"); lvc.menu("M", { { label = "A", call = "a", arg = "1" } }) end
            function plain(ctx) return "just a string" end
            function lists(ctx) return { { op = "reload" }, { op = "message", text = "m" } } end
            function who(ctx) local d = lvc.dirs(); return lvc.current() .. "|" .. #lvc.marked() .. "|" .. d.dir .. "|" .. d.panel end
            function bad(ctx) error("boom") end
            "#,
        );
        let ctx = json!({"file": "/d/a", "files": ["/d/a", "/d/b"], "dir": "/d", "other_dir": "/o", "panel": "left", "arg": null});
        let a = ask(&p, &rx, 1, "hello", ctx.clone()).unwrap();
        assert_eq!(a, r#"[{"op":"message","text":"hello /d/a #1 42"},{"op":"cd","path":"/tmp"}]"#);
        assert!(ask(&p, &rx, 2, "hello", ctx.clone()).unwrap().contains("#2")); // the state is kept
        let asks = ask(&p, &rx, 3, "asks", ctx.clone()).unwrap();
        let v: Value = serde_json::from_str(&asks).unwrap();
        assert_eq!(v[0]["op"], "input");
        assert_eq!(v[0]["then"], "greet");
        assert_eq!(v[0]["value"], "x");
        assert_eq!(v[1], json!({"op":"confirm","text":"Sure?","then":"go"}));
        assert_eq!(v[2]["items"][0]["call"], "a");
        assert_eq!(ask(&p, &rx, 4, "plain", ctx.clone()).unwrap(), r#"[{"op":"message","text":"just a string"}]"#);
        assert_eq!(ask(&p, &rx, 5, "lists", ctx.clone()).unwrap(), r#"[{"op":"reload"},{"op":"message","text":"m"}]"#);
        assert_eq!(ask(&p, &rx, 6, "who", ctx.clone()).unwrap(), r#"[{"op":"message","text":"/d/a|2|/d|left"}]"#);
        assert!(ask(&p, &rx, 7, "bad", ctx.clone()).unwrap_err().contains("boom"));
        assert_eq!(ask(&p, &rx, 8, "nope", ctx), Err("no such function: nope".into()));
        let _ = std::fs::remove_file(dir.join("init.lua"));
        let _ = std::fs::remove_file(dir.join("helper.lua"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn an_endless_loop_is_stopped_and_run_calls_back() {
        let (p, rx, dir) = plugin(
            r#"
            function spin(ctx) while true do end end
            function ok(ctx) lvc.notify("fine") end
            function go(ctx)
              lvc.run({ "sh", "-c", "echo out; echo err >&2; exit 3" }, function(code, out, err)
                lvc.notify(code .. ":" .. out:gsub("%s+$", "") .. ":" .. err:gsub("%s+$", ""))
              end)
            end
            "#,
        );
        let ctx = json!({"dir": "/d"});
        assert_eq!(ask(&p, &rx, 1, "spin", ctx.clone()), Err("the script ran too long and was stopped".into()));
        assert_eq!(ask(&p, &rx, 2, "ok", ctx.clone()).unwrap(), r#"[{"op":"message","text":"fine"}]"#);
        assert_eq!(ask(&p, &rx, 3, "go", ctx).unwrap(), "[]");
        match rx.recv_timeout(Duration::from_secs(20)).unwrap() {
            Msg::Async { result, .. } => assert_eq!(result.unwrap(), r#"[{"op":"message","text":"3:out:err"}]"#),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_file(dir.join("init.lua"));
        let _ = std::fs::remove_file(dir.join("helper.lua"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn a_bad_script_is_refused() {
        let dir = std::env::temp_dir().join(format!("lvc-lua-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.join("init.lua");
        let (out, _rx) = channel();
        let (wake, _w) = channel();
        std::fs::write(&s, "function (").unwrap();
        assert!(check(&s).is_err());
        assert!(LuaPlugin::start("t", &dir, &s, out.clone(), wake.clone()).is_err());
        std::fs::write(&s, "error('at load')").unwrap();
        assert!(LuaPlugin::start("t", &dir, &s, out.clone(), wake.clone()).err().unwrap().contains("at load"));
        std::fs::write(&s, "while true do end").unwrap();
        assert!(LuaPlugin::start("t", &dir, &s, out, wake).err().unwrap().contains("too long"));
        assert!(check(&dir.join("missing.lua")).is_err());
        let _ = std::fs::remove_file(&s);
        let _ = std::fs::remove_dir(&dir);
    }
}
