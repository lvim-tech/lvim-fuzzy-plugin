//! A client of the MODES plugin (`lvim-fuzzy-plugin-modes`): the search modes of lfz (git-status, git-log, processes, docker, …) as data. The
//! plugin's `plugin.toml` says `modes = true` and holds one `[config.modes.NAME]` table per mode; the shared override file and a tool's own
//! `[plugins]` table change or add modes like any other setting. The tool asks `modes` and gets the list as JSON.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::host::{Host, Out};

/// The client: a [`Host`] that talks to the modes plugin only.
pub struct Modes {
    host: Host,
    /// The plugin that serves modes: the one whose `plugin.toml` says `modes = true` (empty: none installed).
    provider: String,
    /// Mistakes found while loading (broken manifests, broken override files).
    pub problems: Vec<String>,
}

impl Modes {
    /// The plugins of `roots` meant for `host` (`"lvim-fuzzy-cli"`, …), the modes plugin configured with the shared override file in the first
    /// root and `host_plugins` (this tool's `[plugins]` table).
    pub fn load(roots: &[PathBuf], host: &str, host_plugins: &toml::Table, wake: Sender<()>) -> Modes {
        let (manifests, mut problems) = crate::manifest::load_roots(roots, host);
        let provider = manifests
            .iter()
            .find(|m| m.table.get("modes").and_then(toml::Value::as_bool) == Some(true))
            .map(|m| m.def.name.clone())
            .unwrap_or_default();
        let mut h = Host::new(manifests.into_iter().map(|m| m.def).collect(), wake);
        if let Some(root) = roots.first() {
            problems.extend(h.configure_all(root, host_plugins));
        }
        Modes { host: h, provider, problems }
    }

    /// Is the modes plugin there (installed for this tool)?
    pub fn available(&self) -> bool {
        !self.provider.is_empty() && self.host.def(&self.provider).is_some()
    }

    /// The name of the plugin that serves the modes.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Ask for every mode and WAIT for the answer: the modes as the plugin has them (`name`, `description`, `title`, `command`, `live`,
    /// `pattern`, `preview`, `target`, `scope`, `requires`, `options`, `open`), in its order.
    pub fn ask(&mut self, timeout: Duration) -> Result<Vec<Value>, String> {
        if !self.available() {
            return Err("the modes plugin `lvim-fuzzy-plugin-modes` is not installed".into());
        }
        let id = self.host.call(&self.provider.clone(), "modes", json!({"call": "modes"}), &[])?;
        let until = Instant::now() + timeout;
        loop {
            for o in self.host.poll() {
                match o {
                    Out::Answer { id: Some(i), text, .. } if i == id => {
                        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                        return Ok(v.get(0).and_then(|ins| ins["modes"].as_array()).cloned().unwrap_or_default());
                    }
                    Out::Failed { id: Some(i), text } if i == id => return Err(text),
                    _ => {}
                }
            }
            if Instant::now() > until {
                return Err("the modes plugin did not answer in time".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn stop(&mut self) {
        self.host.stop(|_| json!({"event": "quit"}));
    }
}
