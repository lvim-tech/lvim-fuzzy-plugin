//! `runtime = "native"`: a plugin that is a shared library (`.so`), loaded on the first call.
//!
//! The C ABI (version [`ABI`]) is three functions, with NUL-terminated UTF-8 strings (JSON text), so
//! nothing depends on the compiler's version or on Rust types:
//!
//! ```c
//! uint32_t lvim_plugin_abi(void);                                   // must return 1
//! char    *lvim_plugin_call(const char *name, const char *ctx_json); // the answer, or NULL
//! void     lvim_plugin_free(char *answer);                          // frees what `call` returned
//! ```
//!
//! The answer is a JSON array of instructions (see plugins.rs), or `{"error":"text"}`. The `lvim-fuzzy-plugin-api`
//! crate (`plugin-api/`) writes all three from one Rust function (the `lvim_fuzzy_plugin!` macro), catching
//! panics there. A library that crashes the process, or never returns, cannot be stopped — it is native
//! code in lvc's own process; use `runtime = "process"` or `"wasm"` for a plugin that must be isolated.
//! Calls run on a thread of their own (the interface never waits for one), one at a time per library.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;

/// The ABI version a library must report from `lvim_plugin_abi`.
pub const ABI: u32 = 1;

type AbiFn = unsafe extern "C" fn() -> u32;
type CallFn = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char;
type FreeFn = unsafe extern "C" fn(*mut c_char);

/// A loaded plugin library (kept loaded for the life of lvc).
pub struct Lib {
    call: CallFn,
    free: FreeFn,
}

// the function pointers are plain code addresses; a Lib is used behind a Mutex, one call at a time
unsafe impl Send for Lib {}

fn dl_error() -> String {
    // SAFETY: dlerror returns NULL or a valid C string owned by libc
    unsafe {
        let e = libc::dlerror();
        if e.is_null() { "unknown error".into() } else { CStr::from_ptr(e).to_string_lossy().into_owned() }
    }
}

fn symbol(handle: *mut c_void, name: &str) -> Result<*mut c_void, String> {
    let c = CString::new(name).expect("a symbol name");
    // SAFETY: handle came from dlopen; c is a valid C string
    let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
    if p.is_null() { Err(format!("no `{name}` in the library (is it built with lvim_fuzzy_plugin!?)")) } else { Ok(p) }
}

impl Lib {
    /// Load `path` and check its ABI version; any problem is the error text.
    pub fn open(path: &Path) -> Result<Lib, String> {
        let c = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| "a NUL in the path".to_string())?;
        // SAFETY: c is a valid C string
        let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("{}: {}", path.display(), dl_error()));
        }
        // SAFETY: the three symbols have the documented C signatures (checked by the ABI version below)
        unsafe {
            let abi: AbiFn = std::mem::transmute::<*mut c_void, AbiFn>(symbol(handle, "lvim_plugin_abi")?);
            let v = abi();
            if v != ABI {
                return Err(format!("{}: ABI version {v}, lvc speaks {ABI}", path.display()));
            }
            let call: CallFn = std::mem::transmute::<*mut c_void, CallFn>(symbol(handle, "lvim_plugin_call")?);
            let free: FreeFn = std::mem::transmute::<*mut c_void, FreeFn>(symbol(handle, "lvim_plugin_free")?);
            Ok(Lib { call, free })
        }
    }

    /// Call function `name` with the context JSON. Ok = the instructions as JSON text.
    pub fn call(&self, name: &str, ctx_json: &str) -> Result<String, String> {
        let n = CString::new(name).map_err(|_| "a NUL in the call name".to_string())?;
        let c = CString::new(ctx_json).map_err(|_| "a NUL in the context".to_string())?;
        // SAFETY: valid C strings in; the answer is read and handed back to the library's own free
        let text = unsafe {
            let p = (self.call)(n.as_ptr(), c.as_ptr());
            if p.is_null() {
                return Err("the plugin returned nothing".into());
            }
            let t = CStr::from_ptr(p).to_string_lossy().into_owned();
            (self.free)(p);
            t
        };
        answer(&text)
    }
}

/// An array of instructions → Ok; `{"error":"x"}` → Err("x").
pub(crate) fn answer(text: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).map_err(|e| format!("bad JSON from the plugin: {e}"))?;
    if let Some(e) = v.get("error") {
        return Err(e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string()));
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(answer(r#"[{"op":"reload"}]"#), Ok(r#"[{"op":"reload"}]"#.into()));
        assert_eq!(answer(r#"{"error":"boom"}"#), Err("boom".into()));
        assert!(answer("nope").is_err());
    }

    /// Build a tiny C library, load it, call it; a wrong ABI and a missing symbol are refused.
    #[test]
    fn loads_a_c_library() {
        let Ok(cc) = std::process::Command::new("cc").arg("--version").output() else { return };
        let _ = cc;
        let dir = std::env::temp_dir().join(format!("lvc-native-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let build = |name: &str, src: &str| -> std::path::PathBuf {
            let c = dir.join(format!("{name}.c"));
            let so = dir.join(format!("lib{name}.so"));
            std::fs::write(&c, src).unwrap();
            let st = std::process::Command::new("cc").args(["-shared", "-fPIC", "-o"]).arg(&so).arg(&c).status().unwrap();
            assert!(st.success());
            so
        };
        let good = build(
            "good",
            r#"#include <stdlib.h>
#include <string.h>
#include <stdio.h>
unsigned lvim_plugin_abi(void) { return 1; }
char *lvim_plugin_call(const char *name, const char *ctx) {
  char *r = malloc(256);
  if (strcmp(name, "fail") == 0) snprintf(r, 256, "{\"error\":\"it failed\"}");
  else snprintf(r, 256, "[{\"op\":\"message\",\"text\":\"native %s\"}]", name);
  return r;
}
void lvim_plugin_free(char *p) { free(p); }
"#,
        );
        let lib = Lib::open(&good).unwrap();
        assert_eq!(lib.call("hi", "{}").unwrap(), r#"[{"op":"message","text":"native hi"}]"#);
        assert_eq!(lib.call("fail", "{}"), Err("it failed".into()));

        let old = build("old", "unsigned lvim_plugin_abi(void) { return 9; }\n");
        assert!(Lib::open(&old).err().unwrap().contains("ABI version 9"));
        let bare = build("bare", "int unrelated;\n");
        assert!(Lib::open(&bare).err().unwrap().contains("lvim_plugin_abi"));
        assert!(Lib::open(&dir.join("nothing.so")).is_err());
    }
}
