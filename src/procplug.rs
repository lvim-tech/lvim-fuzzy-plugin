//! `runtime = "process"`: a plugin that is one long-lived program, started on the first call.
//!
//! lvc speaks to it over its stdin / stdout, one JSON object per line (JSON-RPC 2.0 style):
//!
//! ```text
//! → {"id":1,"method":"call","params":{ "call":"do_x", "file":..., "dir":..., ... }}
//! ← {"id":1,"result":[{"op":"message","text":"done"}]}        (instructions, see plugins.rs)
//! ← {"id":1,"error":"what went wrong"}
//! → {"method":"event","params":{"event":"cd", ...}}            (no answer; only what `events` asks for)
//! ```
//!
//! Its stderr is dropped (it must not draw over the terminal). A reader thread turns each line into a
//! [`Msg`] on the app's channel and wakes the loop, so an answer shows at once, with no tick. A thread
//! that writes keeps a slow plugin from ever blocking the interface.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Sender, channel};

use serde_json::Value;

/// What a plugin process sent back.
#[derive(Debug)]
pub enum Msg {
    /// An answer: the request `id`, and its instructions as JSON text (or the error the plugin gave).
    Reply { plugin: String, generation: u64, id: u64, result: Result<String, String> },
    /// The process ended (its stdout closed).
    Died { plugin: String, generation: u64 },
    /// Something a Lua plugin did on its own (a `lvc.run` callback): instructions as JSON text.
    Async { plugin: String, result: Result<String, String> },
    /// A `native` (.so), `wasm` or `lua` call finished (on its own thread): the request `id`, its instructions as JSON text.
    Done { plugin: String, id: u64, result: Result<String, String> },
}

pub struct Proc {
    /// Which start of the plugin this is (a late message of a dead one is told apart).
    pub generation: u64,
    /// `None` once it is being closed (the plugin's stdin closes with it).
    tx: Option<Sender<String>>,
    child: Child,
}

impl Proc {
    /// Start `exec` (`sh -c`, in the plugin's directory, `$LVIM_PLUGIN_DIR` set).
    pub fn spawn(plugin: &str, dir: &Path, exec: &str, generation: u64, out: Sender<Msg>, wake: Sender<()>) -> std::io::Result<Proc> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(exec)
            .current_dir(dir)
            .env("LVIM_PLUGIN_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = channel::<String>();
        std::thread::spawn(move || {
            // ends when the Proc is dropped (the channel closes: the plugin's stdin closes too)
            for line in rx {
                if stdin.write_all(line.as_bytes()).and_then(|_| stdin.write_all(b"\n")).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let name = plugin.to_string();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some((id, result)) = parse_reply(&line) {
                    let _ = out.send(Msg::Reply { plugin: name.clone(), generation, id, result });
                    let _ = wake.send(());
                }
            }
            let _ = out.send(Msg::Died { plugin: name, generation });
            let _ = wake.send(());
        });
        Ok(Proc { generation, tx: Some(tx), child })
    }

    /// Queue a line for the plugin; false when its writer is gone.
    pub fn send(&self, line: String) -> bool {
        self.tx.as_ref().is_some_and(|t| t.send(line).is_ok())
    }

    /// The process has ended.
    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
    }

    /// Close its stdin and give it a moment to leave on its own (the drop then kills what is left).
    pub fn shutdown(mut self) {
        self.tx = None;
        for _ in 0..10 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
}

impl Drop for Proc {
    /// A process that is let go is stopped and reaped (no zombie stays behind).
    fn drop(&mut self) {
        self.tx = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A call request line.
pub fn request(id: u64, params: &Value) -> String {
    serde_json::json!({"id": id, "method": "call", "params": params}).to_string()
}

/// An event line (no `id`: no answer).
pub fn event(params: &Value) -> String {
    serde_json::json!({"method": "event", "params": params}).to_string()
}

/// `{"id":1,"result":[...]}` → (1, Ok(the result as JSON text)); `{"id":1,"error":"x"}` → (1, Err("x")).
/// Anything else (no id, not JSON) is ignored.
pub fn parse_reply(line: &str) -> Option<(u64, Result<String, String>)> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let id = v.get("id")?.as_u64()?;
    if let Some(e) = v.get("error") {
        let msg = e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string());
        return Some((id, Err(msg)));
    }
    Some((id, Ok(v.get("result").cloned().unwrap_or(Value::Null).to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    #[test]
    fn replies() {
        assert_eq!(parse_reply(r#"{"id":3,"result":[{"op":"reload"}]}"#), Some((3, Ok(r#"[{"op":"reload"}]"#.into()))));
        assert_eq!(parse_reply(r#"{"id":4,"error":"boom"}"#), Some((4, Err("boom".into()))));
        assert_eq!(parse_reply(r#"{"id":5}"#), Some((5, Ok("null".into()))));
        assert_eq!(parse_reply("not json"), None);
        assert_eq!(parse_reply(r#"{"result":[]}"#), None);
        assert!(request(1, &serde_json::json!({"call":"x"})).contains(r#""method":"call""#));
        assert!(!event(&serde_json::json!({"event":"cd"})).contains("\"id\""));
    }

    /// A shell "plugin": answers every request with a message naming the request's id; `die` ends it.
    const ECHO: &str = r#"while read line; do
  case "$line" in *die*) exit 0;; esac
  id=$(printf %s "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  [ -n "$id" ] && printf '{"id":%s,"result":[{"op":"message","text":"pong %s"}]}\n' "$id" "$id"
done"#;

    #[test]
    fn talks_to_a_process_and_sees_it_die() {
        let (out, rx) = channel();
        let (wake, _w) = channel();
        let p = Proc::spawn("t", Path::new("/tmp"), ECHO, 7, out, wake).unwrap();
        assert!(p.send(request(1, &serde_json::json!({}))));
        assert!(p.send(event(&serde_json::json!({"event":"cd"}))));
        assert!(p.send(request(2, &serde_json::json!({}))));
        let mut got = Vec::new();
        for _ in 0..2 {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Msg::Reply { generation: 7, id, result, .. }) => got.push((id, result.unwrap())),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got[0].0, 1);
        assert_eq!(got[1].0, 2);
        assert!(got[1].1.contains("pong 2"));
        assert!(p.send("die".into()));
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Msg::Died { generation: 7, .. }) => {}
            other => panic!("{other:?}"),
        }
        let mut p = p;
        std::thread::sleep(Duration::from_millis(100));
        assert!(p.exited());
        // nothing more comes (the reader has ended: the channel is closed)
        assert!(matches!(rx.recv_timeout(Duration::from_millis(50)), Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout)));
    }

    #[test]
    fn shutdown_closes_stdin_and_ends_the_process() {
        let (out, rx) = channel();
        let (wake, _w) = channel();
        let p = Proc::spawn("t", Path::new("/tmp"), ECHO, 1, out, wake).unwrap();
        p.shutdown();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(Msg::Died { .. })));
    }
}
