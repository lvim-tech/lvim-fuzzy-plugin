# lvim-fuzzy-plugin

The plugin host of the lvim tools: **lvim-fuzzy-commander** (`lvc`), **lvim-fuzzy-cli** (`lfz`) and **lvim-picker**
(Neovim, through the `lvim-fuzzy-plugin` helper). A plugin is a directory with a `plugin.toml`; its functions run in
one of four runtimes behind one JSON interface, so a plugin written once works in every tool that speaks the
instructions it uses. Plugins by other developers install the same way as your own: put the directory in the
plugins directory.

- `src/` — the host library (`Host`, `manifest`) and the runtimes: `process`, `native` (.so), `wasm`, `lua`.
  Every runtime but `process` is a cargo feature (`native`, `wasm`, `lua`; all on by default).
- `src/bin/lvim-fuzzy-plugin.rs` — the tool: `lvim-fuzzy-plugin list`, `check DIR` (a developer's check of a plugin),
  `host` (JSON lines over stdin / stdout: how a tool that cannot link the host, the Neovim picker, runs plugins).
- `plugin-api/` — `lvim-fuzzy-plugin-api`, the SDK: write a native or wasm plugin in one Rust function.
- `examples/plugins/` — one plugin per runtime (`rust-process`, `rust-native`, `rust-wasm`, `lua`).

## `plugin.toml`

```toml
description = "what it does"
runtime     = "wasm"                  # command (default, the tool's own) | process | native | wasm | lua
hosts       = ["lvc", "lfz"]          # optional: only for these tools (lvc, lfz, picker); all when absent
module      = "./plugin.wasm"         # wasm   — `exec` for process, `library` for native, `script` for lua
api         = 1                       # the plugin API it was written for; a newer one is refused with a message
version = "0.2.0"  author = "Name <mail>"  license = "MIT"  repository = "https://..."   # informational
# and the tables a tool understands — lvc's [actions.*] and [previewers.*], for instance
```

## One interface, four runtimes

The tool sends a **context** (JSON: `call`, `arg`, and what the tool knows — `file`, `files`, `dir`, ... for lvc);
the plugin answers with an array of **instructions** (`{"op": "message", "text": "..."}`, `cd`, `input`, `menu`,
`preview`, ...). The vocabulary belongs to the tool; this crate only moves the JSON.

| runtime | what it is | isolation |
|---|---|---|
| `process` | one long-lived program, JSON lines on stdin / stdout; optional `events` | its own process |
| `native` | a `.so` with a three-function C ABI (`lvim_plugin_abi/call/free`) | none: runs in the tool's process |
| `wasm` | a WebAssembly module (`wasm32-wasip1`), new instance per call | sandbox: fuel limit, 256 MB, the given directories read-only, no network |
| `lua` | `init.lua` in an embedded Lua 5.4, state kept; API `lvim.*` | none beyond an instruction limit |

**For a plugin you share, `wasm` is the one to choose**: one portable file, the user can see it cannot touch more
than the directories it is given. `native`, `process` and `lua` plugins run with the user's rights — like any
program the user installs — so a user should read what they install.

## Writing one

`lvim-fuzzy-plugin check DIR` says whether the manifest is valid and the runtime loads. In Rust, `lvim-fuzzy-plugin-api`'s
`lvim_plugin!(handle)` exports the C ABI (native) or the guest ABI (wasm32) around
`fn(&str, &serde_json::Value) -> Result<serde_json::Value, String>`; the examples show each runtime.

## Settings, and what a plugin offers

- **`requires = ["ffmpeg", "a|b"]`** — the programs a plugin calls (`a|b`: either will do). `lvim-fuzzy-plugin list` / `check`
  say what is missing on this machine, and a host shows it where the plugin's result would be.
- **`[config]`** — the plugin's DEFAULT settings. They are overridden in layers, last one wins, tables merging key
  by key: the plugin's defaults, then `~/.config/lvim-plugins/<plugin>.toml` (shared by every tool), then the
  tool's OWN override (lfz `[plugins.<name>]` in its config, lvc the same, the Neovim picker `setup{ plugins = … }`).
  So one tool can change a setting without touching the others. The merged table reaches the plugin in every call as
  `ctx.config` (`Host::configure` / `configure_all`).
- **`[preview.<id>]` / `[open.<id>]`** — what a plugin can preview or open: `match = { ext, name, mime, kind }`,
  `priority` (a user's plugin overrides a default by being higher), `positions`. `Registry::select(class, file)`
  gives the matching providers, best first. See `src/provider.rs`.
