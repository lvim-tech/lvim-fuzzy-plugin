//! `runtime = "wasm"`: a plugin that is a WebAssembly module, run by the `wasmi` interpreter.
//!
//! The same JSON as everywhere, through a small guest ABI (version [`ABI`]); the module exports:
//!
//! ```text
//! memory
//! lvim_plugin_abi() -> i32                                        must be 1
//! lvim_alloc(len: i32) -> i32          lvim_free(ptr: i32, len: i32)
//! lvim_plugin_call(name_ptr, name_len, ctx_ptr, ctx_len) -> i64   (answer_ptr << 32) | answer_len; 0 = nothing
//! ```
//!
//! (the `lvim-fuzzy-plugin-api` crate's `lvim_fuzzy_plugin!` writes them when built for `wasm32-wasip1`). The answer is an
//! array of instructions or `{"error":"text"}`. A module may also export `_initialize` (a Rust reactor): it
//! runs first.
//!
//! Isolation: each call gets a NEW instance (no state between calls, nothing left behind by a crash);
//! memory is limited ([`MEMORY`]); every instruction costs fuel and a call has [`FUEL`] of it, so an endless
//! loop ends with an error instead of freezing anything; the module can reach no file but the two panel
//! directories (the same path inside as outside), READ-ONLY unless `allow_write = true`; no network, no
//! environment, no output to the terminal.

use std::path::{Path, PathBuf};

use wasmi_wasi::wasi_common::dir::{OpenResult, ReaddirCursor, ReaddirEntity, WasiDir};
use wasmi_wasi::wasi_common::file::{FdFlags, Filestat, OFlags};
use wasmi_wasi::wasi_common::{Error, ErrorExt, WasiCtx};
use wasmi::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

/// The guest ABI version `lvim_plugin_abi` must return.
pub const ABI: i32 = 1;
/// Instructions one call may run (an endless loop runs out of it in a few seconds).
pub const FUEL: u64 = 400_000_000;
/// The memory a module may grow to.
pub const MEMORY: usize = 256 << 20;

/// A compiled plugin module (cheap to instantiate for each call).
pub struct Wasm {
    engine: Engine,
    module: Module,
    /// Fuel per call ([`FUEL`]; a test lowers it).
    fuel: u64,
}

struct Host {
    wasi: WasiCtx,
    limits: StoreLimits,
}

/// A directory the module sees, with nothing in it changeable: `open_file` refuses writing and creating,
/// every other changing operation is left at its default (refused).
struct ReadOnly(Box<dyn WasiDir>);

#[async_trait::async_trait]
impl WasiDir for ReadOnly {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn open_file(
        &self,
        symlink_follow: bool,
        path: &str,
        oflags: OFlags,
        read: bool,
        write: bool,
        fdflags: FdFlags,
    ) -> Result<OpenResult, Error> {
        let changes = OFlags::CREATE | OFlags::EXCLUSIVE | OFlags::TRUNCATE;
        if write || oflags.intersects(changes) || fdflags.contains(FdFlags::APPEND) {
            return Err(Error::perm());
        }
        Ok(match self.0.open_file(symlink_follow, path, oflags, read, false, fdflags).await? {
            OpenResult::Dir(d) => OpenResult::Dir(Box::new(ReadOnly(d))),
            f => f,
        })
    }

    async fn readdir(
        &self,
        cursor: ReaddirCursor,
    ) -> Result<Box<dyn Iterator<Item = Result<ReaddirEntity, Error>> + Send>, Error> {
        self.0.readdir(cursor).await
    }

    async fn read_link(&self, path: &str) -> Result<PathBuf, Error> {
        self.0.read_link(path).await
    }

    async fn get_filestat(&self) -> Result<Filestat, Error> {
        self.0.get_filestat().await
    }

    async fn get_path_filestat(&self, path: &str, follow_symlinks: bool) -> Result<Filestat, Error> {
        self.0.get_path_filestat(path, follow_symlinks).await
    }
}

impl Wasm {
    /// Compile the module at `path` and check that it speaks this ABI (a trial instance, no directories).
    pub fn open(path: &Path) -> Result<Wasm, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut cfg = Config::default();
        cfg.consume_fuel(true);
        let engine = Engine::new(&cfg);
        let module = Module::new(&engine, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let w = Wasm { engine, module, fuel: FUEL };
        let (mut store, instance) = w.instance(&[], false)?;
        let abi = instance
            .get_typed_func::<(), i32>(&store, "lvim_plugin_abi")
            .map_err(|_| "no `lvim_plugin_abi` export (is it built with lvim_fuzzy_plugin! for wasm32-wasip1?)".to_string())?
            .call(&mut store, ())
            .map_err(|e| format!("lvim_plugin_abi: {e}"))?;
        if abi != ABI {
            return Err(format!("{}: ABI version {abi}, lvc speaks {ABI}", path.display()));
        }
        Ok(w)
    }

    /// A new instance: the WASI context sees only `dirs` (read-only unless `write`), the fuel is full.
    fn instance(&self, dirs: &[PathBuf], write: bool) -> Result<(Store<Host>, wasmi::Instance), String> {
        let wasi = wasmi_wasi::WasiCtxBuilder::new().build();
        for d in dirs {
            let Ok(dir) = wasmi_wasi::Dir::open_ambient_dir(d, wasmi_wasi::ambient_authority()) else { continue };
            let inner: Box<dyn WasiDir> = Box::new(wasmi_wasi::sync::dir::Dir::from_cap_std(dir));
            let dir: Box<dyn WasiDir> = if write { inner } else { Box::new(ReadOnly(inner)) };
            wasi.push_preopened_dir(dir, d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let host = Host {
            wasi,
            limits: StoreLimitsBuilder::new().memory_size(MEMORY).instances(2).build(),
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|h| &mut h.limits);
        store.set_fuel(self.fuel).map_err(|e| e.to_string())?;
        let mut linker = <Linker<Host>>::new(&self.engine);
        wasmi_wasi::add_to_linker(&mut linker, |h: &mut Host| &mut h.wasi).map_err(|e| e.to_string())?;
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| format!("cannot start the module: {e}"))?;
        if let Ok(init) = instance.get_typed_func::<(), ()>(&store, "_initialize") {
            init.call(&mut store, ()).map_err(|e| trap(&e.to_string()))?;
        }
        Ok((store, instance))
    }

    /// Call `name` with the context JSON, in a fresh instance. Ok = the instructions as JSON text.
    pub fn call(&self, name: &str, ctx_json: &str, dirs: &[PathBuf], write: bool) -> Result<String, String> {
        let (mut store, instance) = self.instance(dirs, write)?;
        let memory = instance.get_memory(&store, "memory").ok_or("the module exports no `memory`")?;
        let missing = |n: &str| format!("no `{n}` export");
        let alloc = instance.get_typed_func::<i32, i32>(&store, "lvim_alloc").map_err(|_| missing("lvim_alloc"))?;
        let free = instance.get_typed_func::<(i32, i32), ()>(&store, "lvim_free").map_err(|_| missing("lvim_free"))?;
        let call = instance
            .get_typed_func::<(i32, i32, i32, i32), i64>(&store, "lvim_plugin_call")
            .map_err(|_| missing("lvim_plugin_call"))?;
        let put = |store: &mut Store<Host>, bytes: &[u8]| -> Result<(i32, i32), String> {
            let len = bytes.len() as i32;
            let ptr = alloc.call(&mut *store, len).map_err(|e| trap(&e.to_string()))?;
            memory.write(&mut *store, ptr as usize, bytes).map_err(|e| format!("writing to the module: {e}"))?;
            Ok((ptr, len))
        };
        let (np, nl) = put(&mut store, name.as_bytes())?;
        let (cp, cl) = put(&mut store, ctx_json.as_bytes())?;
        let packed = call.call(&mut store, (np, nl, cp, cl)).map_err(|e| trap(&e.to_string()))?;
        let (ap, al) = ((packed >> 32) as u32 as usize, (packed & 0xffff_ffff) as usize);
        if packed == 0 {
            return Err("the plugin returned nothing".into());
        }
        let mut buf = vec![0u8; al];
        memory.read(&store, ap, &mut buf).map_err(|e| format!("reading the answer: {e}"))?;
        for (p, l) in [(np, nl), (cp, cl), (ap as i32, al as i32)] {
            let _ = free.call(&mut store, (p, l));
        }
        crate::native::answer(&String::from_utf8_lossy(&buf))
    }
}

/// A trap's message in words: running out of fuel is "ran too long".
fn trap(msg: &str) -> String {
    if msg.contains("fuel") {
        "the plugin ran too long and was stopped".into()
    } else {
        format!("the plugin trapped: {msg}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the example plugin (offline, in a temp target dir); None when the wasm target is missing.
    fn example() -> Option<PathBuf> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/rust-wasm");
        let target = std::env::temp_dir().join(format!("lvc-wasm-example-{}", std::process::id()));
        let st = std::process::Command::new("cargo")
            .args(["build", "--release", "--offline", "--target", "wasm32-wasip1"])
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &target)
            .env_remove("RUSTFLAGS")
            .status()
            .ok()?;
        st.success().then(|| target.join("wasm32-wasip1/release/lvim_rust_wasm.wasm"))
    }

    #[test]
    fn runs_the_example_sandboxed() {
        let Some(wasm) = example() else { return };
        let mut w = Wasm::open(&wasm).unwrap();
        w.fuel = 20_000_000; // enough for the calls below; the endless loop ends sooner
        let dir = std::env::temp_dir().join(format!("lvc-wasm-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "x").unwrap();
        let dirs = vec![dir.clone(), dir.clone()]; // as the app passes them: the panels may show one directory
        let ctx = format!(r#"{{"dir":"{}"}}"#, dir.display());

        let hello = w.call("hello", &ctx, &dirs, false).unwrap();
        assert!(hello.contains("hello from a Rust wasm module"), "{hello}");
        assert!(w.call("count", &ctx, &dirs, false).unwrap().contains("1 entries"));
        assert_eq!(w.call("nothing", &ctx, &dirs, false), Err("no such function: nothing".into()));

        // read-only by default: the write is refused and nothing is made; allow_write lets it through
        let refused = w.call("write", &ctx, &dirs, false).unwrap();
        assert!(refused.contains("could not write"), "{refused}");
        assert!(!dir.join("lvc-wasm-was-here").exists());
        assert!(w.call("write", &ctx, &dirs, true).unwrap().contains("wrote"));
        assert!(dir.join("lvc-wasm-was-here").exists());

        // a directory that was not given is not there at all
        let elsewhere = format!(r#"{{"dir":"{}"}}"#, std::env::temp_dir().display());
        assert!(w.call("count", &elsewhere, &dirs, false).is_err());

        // an endless loop is stopped by the fuel limit, and the next call is fine
        assert_eq!(w.call("spin", &ctx, &dirs, false), Err("the plugin ran too long and was stopped".into()));
        assert!(w.call("hello", &ctx, &dirs, false).is_ok());
        let _ = std::fs::remove_file(dir.join("a"));
        let _ = std::fs::remove_file(dir.join("lvc-wasm-was-here"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn refuses_what_is_not_a_plugin() {
        let dir = std::env::temp_dir().join(format!("lvc-wasm-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.wasm");
        std::fs::write(&empty, b"\0asm\x01\0\0\0").unwrap(); // a valid module with no exports
        assert!(Wasm::open(&empty).err().unwrap().contains("lvim_plugin_abi"));
        let junk = dir.join("junk.wasm");
        std::fs::write(&junk, b"not wasm").unwrap();
        assert!(Wasm::open(&junk).is_err());
        assert!(Wasm::open(&dir.join("missing.wasm")).is_err());
        let _ = std::fs::remove_file(&empty);
        let _ = std::fs::remove_file(&junk);
        let _ = std::fs::remove_dir(&dir);
    }
}
