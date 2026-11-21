//! An lvc plugin as a long-lived process. lvc writes one JSON request per line on stdin:
//!
//!   {"id":1,"method":"call","params":{"call":"hello","file":"/x/a","files":[...],"dir":"/x","other_dir":"/y","panel":"left","arg":null}}
//!   {"method":"event","params":{"event":"cd","dir":"/x", ...}}      (no answer; only what `events` asked for)
//!
//! and reads one JSON answer per request: `{"id":1,"result":[ instructions ]}` or `{"id":1,"error":"text"}`.
//! Instructions: cd, other-cd, select, mark, reload, message, error, input, confirm, menu, preview.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        match req["method"].as_str() {
            Some("call") => {
                let id = req["id"].clone();
                let reply = match call(&req["params"]) {
                    Ok(instructions) => json!({"id": id, "result": instructions}),
                    Err(e) => json!({"id": id, "error": e}),
                };
                let _ = writeln!(out, "{reply}");
                let _ = out.flush();
            }
            // an event has no answer; "quit" is the last thing lvc says
            Some("event") if req["params"]["event"] == "quit" => break,
            _ => {}
        }
    }
}

fn call(ctx: &Value) -> Result<Value, String> {
    let file = ctx["file"].as_str().unwrap_or("(none)");
    let arg = ctx["arg"].as_str().unwrap_or("");
    Ok(match ctx["call"].as_str().unwrap_or("") {
        "hello" => json!([{"op": "message", "text": format!("hello from a Rust process: {file}")}]),
        "ask" => json!([{"op": "input", "prompt": "Your name", "value": "", "then": "greet"}]),
        "greet" => json!([{"op": "message", "text": format!("hi, {arg}!")}]),
        "pick" => json!([{"op": "menu", "title": "Pick", "items": [
            {"label": "Go to /tmp", "call": "go_tmp"},
            {"label": "Say it", "call": "say", "arg": "picked"}]}]),
        "go_tmp" => json!([{"op": "cd", "path": "/tmp"}]),
        "say" => json!([{"op": "message", "text": arg}]),
        "sure" => json!([{"op": "confirm", "text": "Really reload?", "then": "reload"}]),
        "reload" => json!([{"op": "reload"}, {"op": "message", "text": "reloaded"}]),
        other => return Err(format!("no such function: {other}")),
    })
}
