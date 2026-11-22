//! The plugin host of the lvim tools — `lvim-commander`, `lvim-fuzzy-cli` (lfz) and `lvim-picker` (Neovim,
//! through the `lvim-fuzzy-cli preview --plugin` helper) — in one place.
//!
//! A plugin is a directory with a `plugin.toml` ([`manifest::load`]); its `runtime` says how its functions
//! run — a program per action (`command`, the host's own business), one long-lived `process`, a `native`
//! `.so`, a `wasm` module or a `lua` script. All of them speak the same JSON: the host sends a context
//! (`{"call": "fn", "arg": ..., "dir": ..., ...}`) and the plugin answers with an array of instructions
//! (`{"op": "message", "text": "..."}`, ...); which instructions exist, and what is in the context, is for the
//! host to define — this crate only moves JSON. [`Host`] runs it all: it starts a plugin on its first call,
//! answers come back from threads of their own (the tool's interface never waits), a call that does not
//! answer in time is reported, events (`cd`, `cursor`, ...) go to the processes that asked.
//!
//! Every runtime but `process` is a cargo feature (`native`, `wasm`, `lua`; all on by default).

pub mod askpass;
pub mod config;
pub mod host;
pub mod install;
pub mod manifest;
pub mod previewer;
pub mod provider;
pub mod procplug;
pub mod remote;
pub mod types;

#[cfg(feature = "lua")]
pub mod luaplug;
#[cfg(feature = "native")]
pub mod native;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use host::{Host, Out};
pub use manifest::{EVENTS, Manifest, PluginDef, default_root};
pub use previewer::{Previewer, Reply};
pub use remote::{Remote, RemoteEntry, RemoteName};
pub use provider::{FileInfo, Provider, Registry, have, missing_requires};

/// How a plugin's `call` actions are executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Runtime {
    /// A process per action (`command`); no `call`. The host runs these itself.
    #[default]
    Command,
    /// One long-lived process, spoken to over stdin / stdout.
    Process,
    /// A shared library (`.so`).
    Native,
    /// A WebAssembly module.
    Wasm,
    /// An `init.lua`.
    Lua,
}

impl Runtime {
    pub fn parse(s: &str) -> Option<Runtime> {
        Some(match s {
            "command" => Runtime::Command,
            "process" => Runtime::Process,
            "native" => Runtime::Native,
            "wasm" => Runtime::Wasm,
            "lua" => Runtime::Lua,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Runtime::Command => "command",
            Runtime::Process => "process",
            Runtime::Native => "native",
            Runtime::Wasm => "wasm",
            Runtime::Lua => "lua",
        }
    }
}

/// The state of a plugin's runtime, for a listing (`lvc --plugins`): does its library load, its module
/// compile, its script parse? `Ok("")` for a `command` plugin (nothing to check); the text is one line.
pub fn status(def: &PluginDef) -> Result<String, String> {
    #[cfg(any(feature = "native", feature = "wasm", feature = "lua"))]
    let dir = &def.dir;
    let name = &def.name;
    match def.runtime {
        Runtime::Command => Ok(String::new()),
        Runtime::Process => Ok(format!("exec: {}", def.exec.as_deref().unwrap_or("?"))),
        #[cfg(feature = "native")]
        Runtime::Native => {
            let l = def.library.as_deref().unwrap_or("?");
            native::Lib::open(&dir.join(l)).map(|_| format!("library {l}: loads (ABI {})", native::ABI)).map_err(|e| format!("{name}: {e}"))
        }
        #[cfg(feature = "wasm")]
        Runtime::Wasm => {
            let m = def.module.as_deref().unwrap_or("?");
            wasm::Wasm::open(&dir.join(m))
                .map(|_| format!("module {m}: loads (ABI {}, {})", wasm::ABI, if def.allow_write { "may write" } else { "read-only" }))
                .map_err(|e| format!("{name}: {e}"))
        }
        #[cfg(feature = "lua")]
        Runtime::Lua => {
            let s = def.script.as_deref().unwrap_or("init.lua");
            luaplug::check(&dir.join(s)).map(|_| format!("script {s}: compiles (embedded Lua 5.4)")).map_err(|e| format!("{name}: {e}"))
        }
        #[allow(unreachable_patterns)]
        other => Err(format!("{name}: this build has no `{}` runtime", other.name())),
    }
}
