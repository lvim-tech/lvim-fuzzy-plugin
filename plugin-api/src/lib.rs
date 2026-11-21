//! Write an lvc plugin as a shared library (`runtime = "native"`) in one function.
//!
//! ```ignore
//! use lvim_fuzzy_plugin_api::{lvim_fuzzy_plugin, serde_json::{json, Value}};
//!
//! fn handle(call: &str, ctx: &Value) -> Result<Value, String> {
//!     match call {
//!         "hello" => Ok(json!([{"op": "message", "text": format!("in {}", ctx["dir"])}])),
//!         other => Err(format!("no such function: {other}")),
//!     }
//! }
//!
//! lvim_fuzzy_plugin!(handle);
//! ```
//!
//! built as a `cdylib` — for `runtime = "native"` natively, for `runtime = "wasm"` with
//! `cargo build --release --target wasm32-wasip1` (the same macro exports the right functions for each; the
//! module can read the two panel directories with plain `std::fs`; a panic in it is a trap, shown as an error).
//! `handle` gets the call name and the context (`file`, `files`, `dir`, `other_dir`,
//! `panel`, `arg`) and returns the instructions (a JSON array: `cd`, `message`, `input`, `menu` ... see the
//! lvc README) or an error text. A panic inside is caught here and shown as an error — lvc itself is not
//! taken down by it (an abort, a segfault or an endless loop in native code still would).

pub use serde_json;
use serde_json::Value;
use std::ffi::{CStr, CString, c_char};

/// The C ABI version this crate speaks (`lvim_plugin_abi`).
pub const ABI_VERSION: u32 = 1;

type Handler = fn(&str, &Value) -> Result<Value, String>;

/// Parse the context, run the handler with panics caught (where the target unwinds), answer as JSON.
#[doc(hidden)]
pub fn answer_json(name: &str, ctx: &str, handler: Handler) -> Value {
    let run = std::panic::catch_unwind(|| {
        let ctx: Value = serde_json::from_str(ctx).map_err(|e| format!("bad context: {e}"))?;
        handler(name, &ctx)
    });
    match run {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => serde_json::json!({ "error": e }),
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            serde_json::json!({ "error": format!("the plugin panicked: {msg}") })
        }
    }
}

/// What `lvim_plugin_call` does (native): C strings in, a C string out (freed by [`free`]).
///
/// # Safety
/// `name` and `ctx` are valid NUL-terminated strings (lvc passes them so).
#[doc(hidden)]
pub unsafe fn dispatch(name: *const c_char, ctx: *const c_char, handler: Handler) -> *mut c_char {
    // SAFETY: the caller promises valid C strings
    let (name, ctx) = unsafe { (CStr::from_ptr(name).to_string_lossy(), CStr::from_ptr(ctx).to_string_lossy()) };
    let answer = answer_json(&name, &ctx, handler);
    CString::new(answer.to_string()).map(CString::into_raw).unwrap_or(std::ptr::null_mut())
}

/// `lvim_alloc` of a WebAssembly module: a buffer of `len` bytes for lvc to write into.
#[doc(hidden)]
pub fn wasm_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// `lvim_free` of a WebAssembly module.
///
/// # Safety
/// `ptr` / `len` came from [`wasm_alloc`] (or [`wasm_dispatch`]'s answer).
#[doc(hidden)]
pub unsafe fn wasm_free(ptr: *mut u8, len: usize) {
    // SAFETY: made by wasm_alloc with capacity max(len, 1)
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1)) });
}

/// `lvim_plugin_call` of a WebAssembly module: `(answer_ptr << 32) | answer_len`.
///
/// # Safety
/// The pointers / lengths are buffers lvc wrote with [`wasm_alloc`].
#[doc(hidden)]
pub unsafe fn wasm_dispatch(np: *const u8, nl: usize, cp: *const u8, cl: usize, handler: Handler) -> u64 {
    // SAFETY: lvc wrote these buffers
    let (name, ctx) = unsafe {
        (
            String::from_utf8_lossy(std::slice::from_raw_parts(np, nl)).into_owned(),
            String::from_utf8_lossy(std::slice::from_raw_parts(cp, cl)).into_owned(),
        )
    };
    let mut bytes = answer_json(&name, &ctx, handler).to_string().into_bytes();
    bytes.shrink_to_fit();
    let (ptr, len) = (bytes.as_mut_ptr() as usize as u64, bytes.len() as u64);
    std::mem::forget(bytes);
    (ptr << 32) | len
}

/// What `lvim_plugin_free` does.
///
/// # Safety
/// `p` came from [`dispatch`] (or is null).
#[doc(hidden)]
pub unsafe fn free(p: *mut c_char) {
    if !p.is_null() {
        // SAFETY: the pointer was made by CString::into_raw in dispatch
        drop(unsafe { CString::from_raw(p) });
    }
}

/// Export what lvc looks for, built on `$handler`:
/// `fn(call: &str, ctx: &serde_json::Value) -> Result<serde_json::Value, String>`.
/// Built natively (a `cdylib`): the three C functions of `runtime = "native"`. Built for `wasm32-wasip1`:
/// the exports of `runtime = "wasm"`.
#[macro_export]
macro_rules! lvim_fuzzy_plugin {
    ($handler:path) => {
        #[cfg(not(target_arch = "wasm32"))]
        #[unsafe(no_mangle)]
        pub extern "C" fn lvim_plugin_abi() -> u32 {
            $crate::ABI_VERSION
        }

        /// # Safety
        /// Called by lvc with valid NUL-terminated strings.
        #[cfg(not(target_arch = "wasm32"))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn lvim_plugin_call(
            name: *const ::std::ffi::c_char,
            ctx: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_char {
            unsafe { $crate::dispatch(name, ctx, $handler) }
        }

        /// # Safety
        /// Called by lvc with a pointer `lvim_plugin_call` returned.
        #[cfg(not(target_arch = "wasm32"))]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn lvim_plugin_free(p: *mut ::std::ffi::c_char) {
            unsafe { $crate::free(p) }
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn lvim_plugin_abi() -> i32 {
            $crate::ABI_VERSION as i32
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn lvim_alloc(len: usize) -> *mut u8 {
            $crate::wasm_alloc(len)
        }

        /// # Safety
        /// Called by lvc with a buffer from `lvim_alloc` / `lvim_plugin_call`.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn lvim_free(ptr: *mut u8, len: usize) {
            unsafe { $crate::wasm_free(ptr, len) }
        }

        /// # Safety
        /// Called by lvc with buffers from `lvim_alloc`.
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn lvim_plugin_call(np: *const u8, nl: usize, cp: *const u8, cl: usize) -> u64 {
            unsafe { $crate::wasm_dispatch(np, nl, cp, cl, $handler) }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(handler: Handler, name: &str, ctx: &str) -> String {
        let (n, c) = (CString::new(name).unwrap(), CString::new(ctx).unwrap());
        // SAFETY: valid C strings; the answer is freed by `free`
        unsafe {
            let p = dispatch(n.as_ptr(), c.as_ptr(), handler);
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            free(p);
            s
        }
    }

    #[test]
    fn answers_errors_and_panics() {
        let h: Handler = |call, ctx| match call {
            "ok" => Ok(serde_json::json!([{"op":"message","text": ctx["dir"]}])),
            "boom" => panic!("kaboom"),
            _ => Err("nope".into()),
        };
        assert_eq!(run(h, "ok", r#"{"dir":"/x"}"#), r#"[{"op":"message","text":"/x"}]"#);
        assert_eq!(run(h, "other", "{}"), r#"{"error":"nope"}"#);
        assert!(run(h, "boom", "{}").contains("the plugin panicked: kaboom"));
        assert!(run(h, "ok", "not json").contains("bad context"));
    }
}
