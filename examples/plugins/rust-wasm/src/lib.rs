//! An lvc plugin as a WebAssembly module. `lvim_fuzzy_plugin!` exports the guest ABI around one function; the
//! module reaches the panel directories with plain `std::fs` (WASI), nothing else.

use lvim_fuzzy_plugin_api::lvim_fuzzy_plugin;
use lvim_fuzzy_plugin_api::serde_json::{Value, json};

fn handle(call: &str, ctx: &Value) -> Result<Value, String> {
    let dir = ctx["dir"].as_str().unwrap_or(".");
    Ok(match call {
        "hello" => json!([{"op": "message", "text": format!("hello from a Rust wasm module, in {dir}")}]),
        "count" => {
            let n = std::fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))?.count();
            json!([{"op": "message", "text": format!("{n} entries in {dir}")}])
        }
        "write" => match std::fs::write(format!("{dir}/lvc-wasm-was-here"), b"hi") {
            Ok(()) => json!([{"op": "message", "text": "wrote lvc-wasm-was-here"}, {"op": "reload"}]),
            Err(e) => json!([{"op": "error", "text": format!("could not write: {e}")}]),
        },
        "spin" => {
            let mut x = 0u64;
            loop {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        }
        other => return Err(format!("no such function: {other}")),
    })
}

lvim_fuzzy_plugin!(handle);
