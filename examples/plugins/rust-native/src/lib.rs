//! An lvc plugin as a shared library: one function, `lvim_fuzzy_plugin!` exports the C ABI around it.

use lvim_fuzzy_plugin_api::lvim_fuzzy_plugin;
use lvim_fuzzy_plugin_api::serde_json::{Value, json};

fn handle(call: &str, ctx: &Value) -> Result<Value, String> {
    let files = ctx["files"].as_array().map(Vec::len).unwrap_or(0);
    Ok(match call {
        "hello" => json!([{"op": "message", "text": format!("hello from a Rust .so, in {}", ctx["dir"].as_str().unwrap_or("?"))}]),
        "count" => json!([{"op": "message", "text": format!("{files} item(s) to work on")}]),
        "ask" => json!([{"op": "input", "prompt": "Your name", "value": "", "then": "greet"}]),
        "greet" => json!([{"op": "message", "text": format!("hi, {}!", ctx["arg"].as_str().unwrap_or(""))}]),
        other => return Err(format!("no such function: {other}")),
    })
}

lvim_fuzzy_plugin!(handle);
