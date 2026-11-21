//! A plugin's settings in layers, last one wins:
//!
//! 1. the plugin's DEFAULTS — the `[config]` table of its `plugin.toml` ([`crate::PluginDef::config`]);
//! 2. the SHARED override `<plugins dir>/<plugin>.toml` (optional; every tool reads it);
//! 3. the HOST's own override — what the tool's own config holds for this plugin (lfz, lvc, the Neovim picker each
//!    keep their own, so one tool can change a setting without touching the others).
//!
//! The merge is a deep one: tables merge key by key, anything else (a string, a number, a list) is replaced.
//! The result goes to the plugin as JSON in every call (`ctx.config`, see [`crate::Host::configure`]).

use std::path::Path;

use serde_json::Value;

/// `over` laid over `base`: tables merge, everything else is replaced.
pub fn merge(base: &mut toml::Table, over: &toml::Table) {
    for (k, v) in over {
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// The shared override file of `plugin` under `root`, if there is one (a broken file is an error text).
pub fn shared_override(root: &Path, plugin: &str) -> Result<toml::Table, String> {
    let file = root.join(format!("{plugin}.toml"));
    match std::fs::read_to_string(&file) {
        Ok(t) => toml::from_str(&t).map_err(|e| format!("{}: {e}", file.display())),
        Err(_) => Ok(toml::Table::new()),
    }
}

/// The three layers merged, as JSON for the plugin.
pub fn effective(defaults: &toml::Table, shared: &toml::Table, host: &toml::Table) -> Value {
    let mut t = defaults.clone();
    merge(&mut t, shared);
    merge(&mut t, host);
    serde_json::to_value(&t).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> toml::Table {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn the_layers_merge_deeply_and_the_last_wins() {
        let defaults = t("[pdf]\nrender = \"pdftoppm\"\ndpi = 100\n[table]\nrows = 500\n");
        let shared = t("[pdf]\ndpi = 150\n");
        let host = t("[pdf]\nrender = \"mutool\"\n[archive]\ndepth = 3\n");
        let v = effective(&defaults, &shared, &host);
        assert_eq!(v["pdf"]["render"], "mutool"); // the host's own
        assert_eq!(v["pdf"]["dpi"], 150); // the shared file's, over the default
        assert_eq!(v["table"]["rows"], 500); // the default, untouched
        assert_eq!(v["archive"]["depth"], 3); // a key only the host has
        // another host that overrides nothing sees the shared file over the defaults
        let other = effective(&defaults, &shared, &toml::Table::new());
        assert_eq!(other["pdf"]["render"], "pdftoppm");
        assert_eq!(other["pdf"]["dpi"], 150);
    }

    #[test]
    fn a_list_is_replaced_not_merged() {
        let mut base = t("order = [\"a\", \"b\"]\n");
        merge(&mut base, &t("order = [\"c\"]\n"));
        assert_eq!(base["order"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_shared_file_is_optional() {
        let dir = std::env::temp_dir().join(format!("lvim-plugin-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(shared_override(&dir, "nothing").unwrap().is_empty());
        std::fs::write(dir.join("p.toml"), "x = 1\n").unwrap();
        assert_eq!(shared_override(&dir, "p").unwrap()["x"].as_integer(), Some(1));
        std::fs::write(dir.join("bad.toml"), "x = [").unwrap();
        assert!(shared_override(&dir, "bad").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
