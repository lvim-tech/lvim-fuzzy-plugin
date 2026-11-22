//! A client of the REMOTE plugin (`lvim-fuzzy-plugin-remote`): the files of another machine, a cloud or a mount as
//! directories. The tool asks (`list`, `stat`, `mkdir`, `rm`, `mv`, `cat`, `remotes`) and gets the answer later, from
//! [`Remote::poll`] — its interface never waits — or at once with [`Remote::ask`]. What the plugin answers is described in
//! its own crate; this module only moves the JSON and turns the answers into [`RemoteEntry`] values.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde_json::{Value, json};

use crate::host::{Host, Out};

/// The name the plugin is installed under.
pub const PLUGIN: &str = "lvim-fuzzy-plugin-remote";

/// One entry of a remote directory, as the plugin gives it.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Seconds since the epoch.
    pub mtime: i64,
    /// What the engine knows of Unix permissions and ownership (a cloud knows nothing).
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Where a symbolic link points.
    pub link: Option<String>,
}

impl RemoteEntry {
    /// An entry from the plugin's JSON (`{"name","dir","size","mtime","mode"?,…}`).
    pub fn from_json(v: &Value) -> Option<RemoteEntry> {
        Some(RemoteEntry {
            name: v.get("name")?.as_str()?.to_string(),
            dir: v.get("dir").and_then(Value::as_bool).unwrap_or(false),
            size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
            mtime: v.get("mtime").and_then(Value::as_i64).unwrap_or(0),
            mode: v.get("mode").and_then(Value::as_u64).map(|m| m as u32),
            uid: v.get("uid").and_then(Value::as_u64).map(|m| m as u32),
            gid: v.get("gid").and_then(Value::as_u64).map(|m| m as u32),
            link: v.get("link").and_then(Value::as_str).map(str::to_string),
        })
    }
}

/// A remote the plugin serves.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteName {
    pub name: String,
    pub engine: String,
    /// `user@host`, the rclone remote, the mount root: what tells connections apart in a list.
    pub detail: String,
    /// Where to start in it (the site's `path`), when it says.
    pub path: Option<String>,
    /// The site as the settings have it (`null` for a remote only rclone knows).
    pub site: Value,
}

/// The entries of a `list` answer.
pub fn entries(answer: &Value) -> Vec<RemoteEntry> {
    answer["entries"].as_array().map(|a| a.iter().filter_map(RemoteEntry::from_json).collect()).unwrap_or_default()
}

/// The remotes of a `remotes` answer.
pub fn remote_names(answer: &Value) -> Vec<RemoteName> {
    answer["remotes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(RemoteName {
                        name: r["name"].as_str()?.to_string(),
                        engine: r["engine"].as_str().unwrap_or("rclone").to_string(),
                        detail: r["detail"].as_str().unwrap_or("").to_string(),
                        path: r["path"].as_str().map(str::to_string),
                        site: r["site"].clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The client: a [`Host`] that talks to the remote plugin only.
pub struct Remote {
    host: Host,
    /// Mistakes found while loading (broken manifests, broken override files).
    pub problems: Vec<String>,
    /// The calls on their way: id → what was asked (for the tool's own bookkeeping, `tag`).
    tags: HashMap<u64, u64>,
    next_tag: u64,
}

impl Remote {
    /// The plugins of `roots` meant for `host` (`"lvim-fuzzy-commander"`, …), the remote plugin configured with the shared
    /// override file in the first root and `host_plugins` (this tool's `[plugins]` table).
    pub fn load(roots: &[PathBuf], host: &str, host_plugins: &toml::Table, wake: Sender<()>) -> Remote {
        let (manifests, mut problems) = crate::manifest::load_roots(roots, host);
        let mut h = Host::new(manifests.into_iter().map(|m| m.def).collect(), wake);
        if let Some(root) = roots.first() {
            problems.extend(h.configure_all(root, host_plugins));
        }
        Remote { host: h, problems, tags: HashMap::new(), next_tag: 1 }
    }

    /// Is the remote plugin there (installed for this tool)?
    pub fn available(&self) -> bool {
        self.host.def(PLUGIN).is_some()
    }

    /// The plugin's settings as the tools hand them to a transfer (`LVIM_FUZZY_REMOTE_CONFIG`).
    pub fn config(&self) -> Value {
        self.host.config_of(PLUGIN)
    }

    /// Start a call (`list`, `stat`, `mkdir`, `rm`, `mv`, `cat`, `remotes`); the answer comes out of [`Remote::poll`] under the
    /// returned tag. `ctx`: `remote`, `path`, `to`, `dir`, `limit`.
    pub fn call(&mut self, op: &str, mut ctx: Value) -> Result<u64, String> {
        if !ctx.is_object() {
            ctx = json!({});
        }
        ctx["call"] = op.into();
        let id = self.host.call(PLUGIN, op, ctx, &[])?;
        let tag = self.next_tag;
        self.next_tag += 1;
        self.tags.insert(id, tag);
        Ok(tag)
    }

    /// The answers (the first instruction of each) and failures since the last look, by tag.
    pub fn poll(&mut self) -> Vec<(u64, Result<Value, String>)> {
        let mut out = Vec::new();
        for o in self.host.poll() {
            match o {
                Out::Answer { id: Some(id), text, .. } => {
                    let Some(tag) = self.tags.remove(&id) else { continue };
                    let v = serde_json::from_str::<Value>(&text).ok().and_then(|a| a.get(0).cloned()).unwrap_or(Value::Null);
                    out.push((tag, Ok(v)));
                }
                Out::Failed { id: Some(id), text } => {
                    let Some(tag) = self.tags.remove(&id) else { continue };
                    // the host names the plugin in front; the reason is what follows
                    let reason = text.strip_prefix(&format!("{PLUGIN}: ")).unwrap_or(&text).to_string();
                    out.push((tag, Err(reason)));
                }
                _ => {}
            }
        }
        out
    }

    /// [`Remote::call`] and WAIT for the answer.
    pub fn ask(&mut self, op: &str, ctx: Value, timeout: Duration) -> Result<Value, String> {
        let tag = self.call(op, ctx)?;
        // the host answers or gives up by itself (after its own time, longer while the plugin waits for a person); `timeout` is only the
        // most that is waited for a call that nothing ends
        let until = std::time::Instant::now() + timeout.max(Duration::from_secs(900));
        loop {
            if let Some((_, r)) = self.poll().into_iter().find(|(t, _)| *t == tag) {
                return r;
            }
            if std::time::Instant::now() > until {
                return Err(format!("{op}: the remote plugin did not answer in time"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn stop(&mut self) {
        self.host.stop(|_| json!({"event": "quit"}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// A remote plugin in a shell script: it answers `list` with one entry named after the remote, anything else with `done`.
    fn make(root: &std::path::Path) {
        let d = root.join(PLUGIN);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("run.sh"),
            r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"method":"call"'*)
      case "$line" in
        *'"call":"list"'*) r=$(printf '%s' "$line" | sed 's/.*"remote":"\([a-z]*\)".*/\1/'); echo "{\"id\":$id,\"result\":[{\"op\":\"list\",\"entries\":[{\"name\":\"$r\",\"dir\":true,\"size\":0,\"mtime\":7,\"mode\":493}]}]}" ;;
        *'"call":"boom"'*) echo "{\"id\":$id,\"error\":\"it broke\"}" ;;
        *) echo "{\"id\":$id,\"result\":[{\"op\":\"done\"}]}" ;;
      esac ;;
  esac
done
"#,
        )
        .unwrap();
        std::fs::write(d.join("plugin.toml"), "runtime = \"process\"\nexec = \"sh run.sh\"\n[config]\ntimeout = 5\n").unwrap();
    }

    #[test]
    fn a_client_asks_and_gets_entries_or_the_reason() {
        let root = std::env::temp_dir().join(format!("lvim-remote-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        make(&root);
        let (wake, _rx) = channel();
        let mut r = Remote::load(&[root.clone()], "lvim-fuzzy-commander", &toml::Table::new(), wake);
        assert!(r.available());
        assert_eq!(r.config()["timeout"], 5);
        let v = r.ask("list", json!({"remote": "work", "path": "/"}), Duration::from_secs(5)).unwrap();
        let e = entries(&v);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].name.as_str(), e[0].dir, e[0].mtime, e[0].mode), ("work", true, 7, Some(493)));
        assert_eq!(r.ask("mkdir", json!({"remote": "work", "path": "/x"}), Duration::from_secs(5)).unwrap()["op"], "done");
        assert_eq!(r.ask("boom", json!({}), Duration::from_secs(5)).unwrap_err(), "it broke");
        r.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn without_the_plugin_nothing_is_available() {
        let (wake, _rx) = channel();
        let r = Remote::load(&[std::env::temp_dir().join("lvim-no-such-root")], "lvim-fuzzy-commander", &toml::Table::new(), wake);
        assert!(!r.available());
    }

    #[test]
    fn the_answers_of_remotes_and_list() {
        let n = remote_names(&json!({"remotes": [{"name": "a", "engine": "mount"}, {"name": "b"}]}));
        assert_eq!((n[0].name.as_str(), n[0].engine.as_str()), ("a", "mount"));
        assert_eq!((n[1].name.as_str(), n[1].engine.as_str(), n[1].detail.as_str(), n[1].path.clone()), ("b", "rclone", "", None));
        let n = remote_names(&json!({"remotes": [{"name": "w", "engine": "sftp", "detail": "me@h", "path": "~/src", "site": {"host": "h"}}]}));
        assert_eq!((n[0].detail.as_str(), n[0].path.as_deref(), n[0].site["host"].as_str()), ("me@h", Some("~/src"), Some("h")));
        assert!(entries(&json!({})).is_empty());
    }
}
