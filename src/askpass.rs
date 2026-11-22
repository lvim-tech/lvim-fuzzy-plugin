//! Asking a person for a secret with what is on the machine: the passphrase prompt the user already has. Shared by the plugins that need one
//! (the remote plugin for an SSH key, the git plugin for a push). Tried in this order:
//!
//! 1. the `askpass` setting — a command that prints the secret (`{prompt}` is the question, quoted);
//! 2. the `pinentry` setting — a pinentry program (it speaks the Assuan protocol: `ward`, `pinentry-qt`, …);
//! 3. the `pinentry-program` of `~/.gnupg/gpg-agent.conf` (where `ward install` puts `ward`);
//! 4. `pinentry` on the PATH;
//! 5. `$SSH_ASKPASS`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

/// A string for a shell: in single quotes.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `%`, CR and LF are percent-escaped in an Assuan line.
pub fn assuan_escape(s: &str) -> String {
    s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

pub fn assuan_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `pinentry-program` line of `~/.gnupg/gpg-agent.conf`.
fn pinentry_of_gpg_agent() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let text = std::fs::read_to_string(home.join(".gnupg/gpg-agent.conf")).ok()?;
    text.lines().find_map(|l| l.trim().strip_prefix("pinentry-program").map(|r| PathBuf::from(r.trim())))
}

/// Ask a pinentry program for a secret: `description` is what it says, the answer is the text.
pub fn pinentry_get(program: &Path, title: &str, description: &str) -> Result<String, String> {
    let mut child = Command::new(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut out = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let mut line = String::new();
    let read = |out: &mut BufReader<_>, line: &mut String| -> Result<String, String> {
        line.clear();
        out.read_line(line).map_err(|e| e.to_string())?;
        Ok(line.trim_end().to_string())
    };
    // the greeting
    let hello = read(&mut out, &mut line)?;
    if !hello.starts_with("OK") {
        return Err(format!("{}: {hello}", program.display()));
    }
    for cmd in [format!("SETTITLE {}", assuan_escape(title)), format!("SETDESC {}", assuan_escape(description)), "SETPROMPT Passphrase:".to_string()] {
        writeln!(stdin, "{cmd}").map_err(|e| e.to_string())?;
        let r = read(&mut out, &mut line)?;
        if !r.starts_with("OK") {
            return Err(format!("{}: {r}", program.display()));
        }
    }
    writeln!(stdin, "GETPIN").map_err(|e| e.to_string())?;
    let mut secret: Option<String> = None;
    loop {
        let r = read(&mut out, &mut line)?;
        if let Some(d) = r.strip_prefix("D ") {
            secret = Some(assuan_unescape(d));
        } else if r.starts_with("OK") {
            break;
        } else if r.starts_with("ERR") {
            let _ = writeln!(stdin, "BYE");
            let _ = child.wait();
            // 83886179: the user cancelled
            return Err(if r.contains("83886179") || r.to_lowercase().contains("cancel") { "cancelled".to_string() } else { r });
        } else if r.is_empty() {
            return Err("the prompt closed without an answer".into());
        }
    }
    let _ = writeln!(stdin, "BYE");
    let _ = child.wait();
    secret.ok_or_else(|| "nothing was given".to_string())
}

/// A command that prints the secret (the `askpass` setting, or `$SSH_ASKPASS` with the question as its argument).
pub fn command_get(cmd: &str, prompt: &str) -> Result<String, String> {
    let line = if cmd.contains("{prompt}") { cmd.replace("{prompt}", &sh_quote(prompt)) } else { format!("{cmd} {}", sh_quote(prompt)) };
    let o = Command::new("sh").arg("-c").arg(&line).stdin(Stdio::null()).stderr(Stdio::null()).output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err("cancelled".into());
    }
    let t = String::from_utf8_lossy(&o.stdout).trim_end_matches(['\n', '\r']).to_string();
    if t.is_empty() { Err("nothing was given".into()) } else { Ok(t) }
}

pub fn which(prog: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(prog)).find(|f| f.is_file()))
}

/// Ask for a secret from the first prompt there is (see the top of this file). `def` holds the `askpass` / `pinentry` settings (a plugin's
/// connection or settings; anything without them is fine).
pub fn ask(def: &Value, title: &str, description: &str) -> Result<String, String> {
    if let Some(c) = def["askpass"].as_str().filter(|c| !c.is_empty()) {
        return command_get(c, description);
    }
    let candidates: Vec<PathBuf> = def["pinentry"].as_str().map(PathBuf::from).into_iter().chain(pinentry_of_gpg_agent()).chain(which("pinentry")).filter(|p| p.is_file()).collect();
    let mut last = String::new();
    for p in &candidates {
        match pinentry_get(p, title, description) {
            Ok(s) => return Ok(s),
            // the user said no: that is the answer
            Err(e) if e == "cancelled" => return Err(e),
            Err(e) => last = e,
        }
    }
    if let Ok(c) = std::env::var("SSH_ASKPASS")
        && !c.is_empty()
    {
        return command_get(&c, description);
    }
    Err(if last.is_empty() { "no passphrase prompt on this machine (a pinentry, or SSH_ASKPASS): set `askpass` or `pinentry`".into() } else { last })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assuan_lines_are_escaped_and_read_back() {
        assert_eq!(assuan_escape("a%b\nc"), "a%25b%0Ac");
        assert_eq!(assuan_unescape("a%25b%0Ac"), "a%b\nc");
        assert_eq!(assuan_unescape("plain"), "plain");
        // a lone % is left as it is
        assert_eq!(assuan_unescape("50%"), "50%");
        assert_eq!(assuan_unescape("%zz"), "%zz");
    }

    fn fake(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lvim-askpass-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_pinentry_is_asked_in_assuan() {
        let d = tmp("pin");
        // answers what a pinentry answers: OK to the settings, the secret (with an escaped %) to GETPIN
        let p = fake(&d, "pinentry-fake", "echo 'OK Pleased to meet you'\nwhile read -r l; do case \"$l\" in GETPIN) echo 'D s3cr%25t'; echo OK;; BYE) echo OK; exit 0;; *) echo OK;; esac; done\n");
        assert_eq!(pinentry_get(&p, "t", "d").unwrap(), "s3cr%t");
        // the user said no
        let n = fake(&d, "pinentry-no", "echo 'OK hi'\nwhile read -r l; do case \"$l\" in GETPIN) echo 'ERR 83886179 Operation cancelled';; BYE) echo OK; exit 0;; *) echo OK;; esac; done\n");
        assert_eq!(pinentry_get(&n, "t", "d").unwrap_err(), "cancelled");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_settings_askpass_and_pinentry_come_first() {
        let d = tmp("order");
        let pin = fake(&d, "pe", "echo 'OK hi'\nwhile read -r l; do case \"$l\" in GETPIN) echo 'D from-pinentry'; echo OK;; BYE) echo OK; exit 0;; *) echo OK;; esac; done\n");
        assert_eq!(ask(&serde_json::json!({"askpass": "printf 'from-askpass\\n' #"}), "t", "d").unwrap(), "from-askpass");
        assert_eq!(ask(&serde_json::json!({"pinentry": pin}), "t", "d").unwrap(), "from-pinentry");
        let _ = std::fs::remove_dir_all(&d);
    }
}
