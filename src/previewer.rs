//! [`Previewer`]: the part of a host that asks the PREVIEW providers of its plugins about a file.
//!
//! The host gives it a path; it classifies the file ([`FileInfo`]), takes the providers that match (best first,
//! see [`crate::provider`]) and calls them one after another until one does not answer `pass`. The answer is the
//! plugin's JSON array of instructions, as text: drawing it is for the host (`lvim_fuzzy_ui::provided` does it).
//! Nothing waits: [`Previewer::request`] returns a ticket, the [`Reply`] with that ticket comes out of
//! [`Previewer::poll`] when the plugin has answered (the host's `wake` channel says when to look).
//!
//! SCHEDULING. A plugin process answers one call at a time, so the previewer sends it one and keeps the rest WAITING
//! here, where they can still be changed: the most urgent first (`prio`, 0 = what the user is looking at, a bigger
//! number = made ahead), the newest first among equals; a new place ([`Previewer::request`] supersedes) drops what
//! waited for the place before; the same question asked twice is asked once. A call already running cannot be taken
//! back (the process is busy with it), but nothing piles up behind it: only the newest wish waits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use serde_json::{Value, json};

use crate::manifest::Manifest;
use crate::provider::{FileInfo, Provider, Registry};
use crate::{Host, Out};

/// What a request came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The ticket [`Previewer::request`] gave.
    pub ticket: u64,
    pub path: PathBuf,
    /// The `extra` the request was made with (the host's own: the position asked for, the size).
    pub extra: Value,
    /// `plugin/provider` that answered (empty when none did).
    pub by: String,
    /// The JSON array of instructions; `[{"op":"pass"}]` when every provider passed. Err = every provider failed
    /// (the last one's one-line error).
    pub answer: Result<String, String>,
}

struct Job {
    ticket: u64,
    /// 0 = what the user is looking at; bigger = made ahead of time.
    prio: u8,
    /// "preview" or "open": which providers are asked and what the call is named.
    class: String,
    path: PathBuf,
    extra: Value,
    /// The providers not yet asked.
    rest: Vec<Provider>,
    /// The one being asked.
    now: Provider,
    last_err: Option<String>,
}

/// A job whose plugin is busy with another call.
struct Waiting {
    job: Job,
    seq: u64,
}

pub struct Previewer {
    host: Host,
    registry: Registry,
    /// Sent to a plugin, answer not yet in: by the host's call id.
    jobs: HashMap<u64, Job>,
    /// Not sent yet: their plugin is busy.
    waiting: Vec<Waiting>,
    /// The call (host call id) each plugin is busy with.
    busy: HashMap<String, u64>,
    next_seq: u64,
    next_ticket: u64,
    /// Mistakes found while loading (broken manifests, broken override files).
    pub problems: Vec<String>,
}

impl Previewer {
    /// The plugins of `roots` meant for `host` (`"lvim-fuzzy-commander"`, `"lvim-fuzzy-cli"`, `"lvim-fuzzy-picker"`), configured with the shared override
    /// files in the first root and `host_plugins` (this tool's `[plugins]` table).
    pub fn load(roots: &[PathBuf], host: &str, host_plugins: &toml::Table, wake: Sender<()>) -> Previewer {
        let (manifests, mut problems) = crate::manifest::load_roots(roots, host);
        Previewer::from_manifests(manifests, roots.first().map(PathBuf::as_path), host_plugins, wake, &mut problems)
    }

    fn from_manifests(manifests: Vec<Manifest>, root: Option<&Path>, host_plugins: &toml::Table, wake: Sender<()>, problems: &mut Vec<String>) -> Previewer {
        let (registry, errs) = Registry::from_manifests(&manifests);
        problems.extend(errs);
        let mut host = Host::new(manifests.into_iter().map(|m| m.def).collect(), wake);
        if let Some(root) = root {
            problems.extend(host.configure_all(root, host_plugins));
        }
        Previewer {
            host,
            registry,
            jobs: HashMap::new(),
            waiting: Vec::new(),
            busy: HashMap::new(),
            next_seq: 0,
            next_ticket: 1,
            problems: std::mem::take(problems),
        }
    }

    /// Is there any plugin that previews anything?
    pub fn is_empty(&self) -> bool {
        !self.has("preview")
    }

    /// Does any plugin offer providers of `class` (`"preview"`, `"open"`)?
    pub fn has(&self, class: &str) -> bool {
        self.registry.all().iter().any(|p| p.class == class)
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The providers that would be asked about `path`, best first (`plugin/id`).
    pub fn candidates(&self, path: &Path) -> Vec<String> {
        self.registry.select("preview", &FileInfo::of(path)).iter().map(|p| format!("{}/{}", p.plugin, p.id)).collect()
    }

    /// Ask the providers about `path`; `extra` is added to every call's context (the area's size, the position of
    /// a PDF page, ...). None = no provider matches the file: nothing was asked.
    ///
    /// This is a NEW PLACE: what still waited for the place before (a request of the same class that was not sent yet)
    /// is dropped and never answered. Ask the same thing twice and you get the first ticket back.
    pub fn request(&mut self, path: &Path, extra: Value) -> Option<u64> {
        self.schedule("preview", path, extra, 0, true)
    }

    /// [`Previewer::request`] for the providers of another class (`"open"`); supersedes nothing.
    pub fn request_class(&mut self, class: &str, path: &Path, extra: Value) -> Option<u64> {
        self.schedule(class, path, extra, 0, false)
    }

    /// A request made AHEAD of time (the neighbour of the file shown): it waits behind everything with a smaller
    /// `prio`, supersedes nothing, and is dropped by the next [`Previewer::request`] if it has not been sent.
    pub fn request_ahead(&mut self, path: &Path, extra: Value, prio: u8) -> Option<u64> {
        self.schedule("preview", path, extra, prio.max(1), false)
    }

    /// Drop what is waiting (not sent yet) in `class`; a call already running goes on and its answer is delivered.
    pub fn cancel_waiting(&mut self, class: &str) {
        self.waiting.retain(|w| w.job.class != class);
    }

    /// Forget every request nobody wants any more: the waiting ones are dropped, the running ones' answers are
    /// dropped when they come.
    pub fn cancel_all(&mut self) {
        self.jobs.clear();
        self.waiting.clear();
    }

    /// How many requests wait (not sent: their plugin is busy) and how many run.
    pub fn queued(&self) -> (usize, usize) {
        (self.waiting.len(), self.jobs.len())
    }

    fn schedule(&mut self, class: &str, path: &Path, extra: Value, prio: u8, supersede: bool) -> Option<u64> {
        let mut cands: Vec<Provider> = self.registry.select(class, &FileInfo::of(path)).into_iter().cloned().collect();
        if cands.is_empty() {
            return None;
        }
        // the same question is already on its way (or waits): its answer is this one's
        let same = |j: &Job| j.class == class && j.path == path && j.extra == extra;
        if let Some(t) = self.jobs.values().map(|j| (j, j.ticket)).chain(self.waiting.iter().map(|w| (&w.job, w.job.ticket))).find(|(j, _)| same(j)).map(|(_, t)| t) {
            return Some(t);
        }
        if supersede {
            self.waiting.retain(|w| !(w.job.class == class && w.job.prio >= prio));
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let now = cands.remove(0);
        let job = Job { ticket, prio, class: class.to_string(), path: path.to_path_buf(), extra, rest: cands, now, last_err: None };
        self.enqueue(job);
        Some(ticket)
    }

    /// Put `job` among the waiting ones and send what can be sent.
    fn enqueue(&mut self, job: Job) {
        self.next_seq += 1;
        self.waiting.push(Waiting { job, seq: self.next_seq });
        self.dispatch();
    }

    /// Send the most urgent waiting job whose plugin is free — again and again until none can go.
    fn dispatch(&mut self) {
        loop {
            let best = (0..self.waiting.len())
                .filter(|&i| !self.busy.contains_key(&self.waiting[i].job.now.plugin))
                .min_by_key(|&i| (self.waiting[i].job.prio, std::cmp::Reverse(self.waiting[i].seq)));
            let Some(i) = best else { return };
            let w = self.waiting.swap_remove(i);
            self.start(w.job);
        }
    }

    fn ctx(job: &Job) -> Value {
        let mut ctx = job.extra.clone();
        if !ctx.is_object() {
            ctx = json!({});
        }
        let o = ctx.as_object_mut().expect("an object");
        o.insert("call".into(), job.class.clone().into());
        o.insert("provider".into(), job.now.id.clone().into());
        // a plugin runs in its own directory: it gets a path that means the same there
        let abs = std::path::absolute(&job.path).unwrap_or_else(|_| job.path.clone());
        o.insert("path".into(), abs.to_string_lossy().into_owned().into());
        ctx
    }

    /// Send `job` to its current provider; a provider that cannot even be called is skipped for the next one.
    fn start(&mut self, mut job: Job) {
        loop {
            match self.host.call(&job.now.plugin, &job.class.clone(), Self::ctx(&job), &[]) {
                Ok(id) => {
                    self.busy.insert(job.now.plugin.clone(), id);
                    self.jobs.insert(id, job);
                    return;
                }
                Err(e) => {
                    job.last_err = Some(e);
                    if job.rest.is_empty() {
                        self.jobs.insert(u64::MAX - job.ticket, job); // finished by the next poll
                        return;
                    }
                    job.now = job.rest.remove(0);
                }
            }
        }
    }

    /// The requests that have an answer now.
    pub fn poll(&mut self) -> Vec<Reply> {
        let mut replies = Vec::new();
        // a job that could not be started at all
        let dead: Vec<u64> = self.jobs.keys().copied().filter(|k| *k > u64::MAX / 2).collect();
        for k in dead {
            if let Some(j) = self.jobs.remove(&k) {
                replies.push(Reply { ticket: j.ticket, path: j.path, extra: j.extra, by: String::new(), answer: Err(j.last_err.unwrap_or_default()) });
            }
        }
        for out in self.host.poll() {
            let (id, result) = match out {
                Out::Answer { id: Some(id), text, .. } => (id, Ok(text)),
                Out::Failed { id: Some(id), text } => (id, Err(text)),
                _ => continue,
            };
            // the plugin is free again (also when nobody wants the answer any more)
            self.busy.retain(|_, v| *v != id);
            let Some(mut job) = self.jobs.remove(&id) else { continue };
            let by = format!("{}/{}", job.now.plugin, job.now.id);
            let passed = match &result {
                Ok(text) => serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|v| v.as_array().and_then(|a| a.first().cloned()))
                    .is_none_or(|first| first["op"] == "pass"),
                Err(_) => true,
            };
            if !passed {
                replies.push(Reply { ticket: job.ticket, path: job.path, extra: job.extra, by, answer: result });
                continue;
            }
            if let Err(e) = &result {
                job.last_err = Some(e.clone());
            }
            if job.rest.is_empty() {
                let answer = match job.last_err {
                    Some(e) if result.is_err() => Err(e),
                    _ => Ok(r#"[{"op":"pass"}]"#.to_string()),
                };
                replies.push(Reply { ticket: job.ticket, path: job.path, extra: job.extra, by: String::new(), answer });
            } else {
                job.now = job.rest.remove(0);
                self.enqueue(job);
            }
        }
        self.dispatch();
        replies
    }

    /// Ask the providers of `class` about `path` and WAIT for the answer (a host that needs it at once, as the one
    /// who presses Enter on a file does): the answer's JSON text (`[{"op":"pass"}]` when no provider has anything),
    /// or the one-line reason there is none. Do not mix with [`Previewer::poll`] on the same value: this takes
    /// every answer that comes while it waits.
    pub fn ask(&mut self, class: &str, path: &Path, extra: Value, timeout: std::time::Duration) -> Result<String, String> {
        let Some(ticket) = self.request_class(class, path, extra) else { return Ok(r#"[{"op":"pass"}]"#.to_string()) };
        let until = std::time::Instant::now() + timeout;
        loop {
            if let Some(r) = self.poll().into_iter().find(|r| r.ticket == ticket) {
                return r.answer;
            }
            if std::time::Instant::now() > until {
                return Err(format!("{class}: the plugin did not answer in time"));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
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
    use std::time::{Duration, Instant};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lvim-previewer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A process plugin in a shell script: provider `a` passes, any other answers with a fact naming itself.
    fn make_plugin(root: &Path, name: &str, providers: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("run.sh"),
            r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  case "$line" in
    *'"method":"call"'*)
      case "$line" in
        *'"provider":"a"'*) echo "{\"id\":$id,\"result\":[{\"op\":\"pass\"}]}" ;;
        *) p=$(printf '%s' "$line" | sed 's/.*"provider":"\([a-z]*\)".*/\1/'); echo "{\"id\":$id,\"result\":[{\"op\":\"preview\",\"facts\":[[\"by\",\"$p\"]]}]}" ;;
      esac ;;
  esac
done
"#,
        )
        .unwrap();
        std::fs::write(d.join("plugin.toml"), format!("runtime = \"process\"\nexec = \"sh run.sh\"\n{providers}")).unwrap();
    }

    fn wait(p: &mut Previewer) -> Vec<Reply> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let r = p.poll();
            if !r.is_empty() || Instant::now() > until {
                return r;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_pass_goes_on_to_the_next_provider() {
        let root = tmp("pass");
        make_plugin(&root, "p", "[preview.a]\nmatch = { ext = [\"txt\"] }\npriority = 9\n[preview.b]\nmatch = { ext = [\"txt\"] }\npriority = 1\n");
        let (tx, _rx) = channel();
        let mut p = Previewer::load(std::slice::from_ref(&root), "lvim-fuzzy-cli", &toml::Table::new(), tx);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert!(!p.is_empty());
        let f = root.join("x.txt");
        std::fs::write(&f, "hi").unwrap();
        assert_eq!(p.candidates(&f), vec!["p/a", "p/b"]);
        let t = p.request(&f, json!({})).unwrap();
        let r = wait(&mut p);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].ticket, r[0].by.as_str()), (t, "p/b"));
        assert!(r[0].answer.as_ref().unwrap().contains(r#""by","b""#), "{:?}", r[0].answer);
        p.stop();
    }

    #[test]
    fn a_relative_path_reaches_the_plugin_as_an_absolute_one() {
        let root = tmp("abs");
        let d = root.join("p");
        std::fs::create_dir_all(&d).unwrap();
        // echoes the path it got as a fact
        std::fs::write(
            d.join("run.sh"),
            "while IFS= read -r line; do\n  id=$(printf '%s' \"$line\" | sed 's/.*\"id\":\\([0-9]*\\).*/\\1/')\n  case \"$line\" in *'\"method\":\"call\"'*) p=$(printf '%s' \"$line\" | sed 's/.*\"path\":\"\\([^\"]*\\)\".*/\\1/'); echo \"{\\\"id\\\":$id,\\\"result\\\":[{\\\"op\\\":\\\"preview\\\",\\\"facts\\\":[[\\\"path\\\",\\\"$p\\\"]]}]}\" ;; esac\ndone\n",
        )
        .unwrap();
        std::fs::write(d.join("plugin.toml"), "runtime = \"process\"\nexec = \"sh run.sh\"\n[preview.any]\n").unwrap();
        let (tx, _rx) = channel();
        let mut p = Previewer::load(std::slice::from_ref(&root), "lvim-fuzzy-cli", &toml::Table::new(), tx);
        p.request(Path::new("Cargo.toml"), json!({})).unwrap();
        let r = wait(&mut p);
        let text = r[0].answer.clone().unwrap();
        assert!(text.contains(&format!("{}/Cargo.toml", std::env::current_dir().unwrap().display())), "{text}");
        assert_eq!(r[0].path, Path::new("Cargo.toml"), "the reply keeps the path as the host gave it");
        p.stop();
    }

    #[test]
    fn another_class_is_asked_by_its_own_name_and_ask_waits() {
        let root = tmp("class");
        // answers a fact naming the call it got
        let d = root.join("p");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("run.sh"),
            "while IFS= read -r line; do\n  id=$(printf '%s' \"$line\" | sed 's/.*\"id\":\\([0-9]*\\).*/\\1/')\n  case \"$line\" in *'\"method\":\"call\"'*) c=$(printf '%s' \"$line\" | sed 's/.*\"call\":\"\\([a-z]*\\)\".*/\\1/'); echo \"{\\\"id\\\":$id,\\\"result\\\":[{\\\"op\\\":\\\"open\\\",\\\"called\\\":\\\"$c\\\"}]}\" ;; esac\ndone\n",
        )
        .unwrap();
        std::fs::write(d.join("plugin.toml"), "runtime = \"process\"\nexec = \"sh run.sh\"\n[open.any]\n").unwrap();
        let (tx, _rx) = channel();
        let mut p = Previewer::load(std::slice::from_ref(&root), "lvim-fuzzy-cli", &toml::Table::new(), tx);
        assert!(p.has("open") && !p.has("preview") && p.is_empty());
        let f = root.join("x.txt");
        std::fs::write(&f, "x").unwrap();
        let text = p.ask("open", &f, json!({}), Duration::from_secs(5)).unwrap();
        assert!(text.contains(r#""called":"open""#), "{text}");
        // no provider of the class: a pass at once, nothing waited for
        assert_eq!(p.ask("preview", &f, json!({}), Duration::from_secs(5)).unwrap(), r#"[{"op":"pass"}]"#);
        p.stop();
    }

    /// A process plugin that answers with the path it was asked about, and takes half a second over a file whose name
    /// has `slow` in it.
    fn make_slow_plugin(root: &Path) {
        let d = root.join("p");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("run.sh"),
            "while IFS= read -r line; do\n  id=$(printf '%s' \"$line\" | sed 's/.*\"id\":\\([0-9]*\\).*/\\1/')\n  case \"$line\" in *'\"method\":\"call\"'*)\n    p=$(printf '%s' \"$line\" | sed 's/.*\"path\":\"\\([^\"]*\\)\".*/\\1/')\n    case \"$p\" in *slow*) sleep 0.5 ;; esac\n    echo \"{\\\"id\\\":$id,\\\"result\\\":[{\\\"op\\\":\\\"preview\\\",\\\"facts\\\":[[\\\"path\\\",\\\"$p\\\"]]}]}\" ;;\n  esac\ndone\n",
        )
        .unwrap();
        std::fs::write(d.join("plugin.toml"), "runtime = \"process\"\nexec = \"sh run.sh\"\n[preview.any]\n").unwrap();
    }

    /// Collect `n` replies (in the order they come), waiting up to 5 s.
    fn collect(p: &mut Previewer, n: usize) -> Vec<Reply> {
        let until = Instant::now() + Duration::from_secs(5);
        let mut out = Vec::new();
        while out.len() < n && Instant::now() < until {
            out.extend(p.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
        out
    }

    fn slow_setup(name: &str) -> (Previewer, PathBuf) {
        let root = tmp(name);
        make_slow_plugin(&root);
        for f in ["slow1.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt"] {
            std::fs::write(root.join(f), "x").unwrap();
        }
        let (tx, _rx) = channel();
        (Previewer::load(std::slice::from_ref(&root), "lvim-fuzzy-cli", &toml::Table::new(), tx), root)
    }

    #[test]
    fn a_new_place_drops_what_waited_and_nothing_piles_up_behind_a_slow_call() {
        let (mut p, root) = slow_setup("sched-newplace");
        let t_slow = p.request(&root.join("slow1.txt"), json!({})).unwrap(); // running: the plugin is busy for 0.5 s
        let t_b = p.request(&root.join("b.txt"), json!({})).unwrap(); // waits
        assert_eq!(p.queued(), (1, 1));
        let t_c = p.request(&root.join("c.txt"), json!({})).unwrap(); // a new place: b is dropped, c waits
        assert_eq!(p.queued(), (1, 1), "only the newest wish waits");
        let got = collect(&mut p, 2);
        let tickets: Vec<u64> = got.iter().map(|r| r.ticket).collect();
        assert_eq!(tickets, vec![t_slow, t_c], "the running call is answered, then the newest; b never");
        std::thread::sleep(Duration::from_millis(300));
        assert!(p.poll().is_empty(), "b was dropped: no answer comes for ticket {t_b}");
        assert_eq!(p.queued(), (0, 0));
        p.stop();
    }

    #[test]
    fn the_urgent_goes_first_and_work_made_ahead_is_dropped_by_a_new_place() {
        let (mut p, root) = slow_setup("sched-prio");
        let t_slow = p.request(&root.join("slow1.txt"), json!({})).unwrap();
        let t_d = p.request_ahead(&root.join("d.txt"), json!({}), 2).unwrap();
        let t_e = p.request_ahead(&root.join("e.txt"), json!({}), 1).unwrap();
        assert_eq!(p.queued(), (2, 1));
        let got = collect(&mut p, 3);
        assert_eq!(got.iter().map(|r| r.ticket).collect::<Vec<_>>(), vec![t_slow, t_e, t_d], "prio 1 before prio 2");
        // a new place drops the neighbours that have not been sent
        let t_slow2 = p.request(&root.join("slow1.txt"), json!({"again": 1})).unwrap();
        p.request_ahead(&root.join("d.txt"), json!({"again": 1}), 2).unwrap();
        let t_f = p.request(&root.join("f.txt"), json!({})).unwrap();
        assert_eq!(p.queued(), (1, 1), "the ahead request was dropped by the new place");
        let got = collect(&mut p, 2);
        assert_eq!(got.iter().map(|r| r.ticket).collect::<Vec<_>>(), vec![t_slow2, t_f]);
        p.stop();
    }

    #[test]
    fn the_same_question_is_asked_once() {
        let (mut p, root) = slow_setup("sched-same");
        let a = p.request(&root.join("slow1.txt"), json!({"skip": 1})).unwrap();
        assert_eq!(p.request(&root.join("slow1.txt"), json!({"skip": 1})), Some(a), "running: the same ticket");
        let b = p.request(&root.join("b.txt"), json!({})).unwrap();
        assert_eq!(p.request(&root.join("b.txt"), json!({})), Some(b), "waiting: the same ticket");
        // another position of the same file is another question
        assert_ne!(p.request(&root.join("slow1.txt"), json!({"skip": 2})), Some(a));
        p.stop();
    }

    #[test]
    fn nothing_that_matches_is_none_and_all_passing_is_a_pass() {
        let root = tmp("none");
        make_plugin(&root, "p", "[preview.a]\nmatch = { ext = [\"txt\"] }\n");
        let (tx, _rx) = channel();
        let mut p = Previewer::load(std::slice::from_ref(&root), "lvim-fuzzy-cli", &toml::Table::new(), tx);
        let f = root.join("x.txt");
        std::fs::write(&f, "hi").unwrap();
        assert!(p.request(&root.join("x.rs"), json!({})).is_none());
        p.request(&f, json!({})).unwrap();
        let r = wait(&mut p);
        assert_eq!(r[0].answer.as_deref(), Ok(r#"[{"op":"pass"}]"#));
        assert_eq!(r[0].by, "");
        p.stop();
    }

    #[test]
    fn a_plugin_that_cannot_start_is_an_error_not_a_hang() {
        let root = tmp("bad");
        let d = root.join("p");
        std::fs::create_dir_all(&d).unwrap();
        // no `exec`: the call cannot even start
        std::fs::write(d.join("plugin.toml"), "runtime = \"process\"\n[preview.a]\nmatch = { ext = [\"txt\"] }\n").unwrap();
        let (tx, _rx) = channel();
        let mut p = Previewer::load(std::slice::from_ref(&root), "lvim-fuzzy-cli", &toml::Table::new(), tx);
        let f = root.join("x.txt");
        std::fs::write(&f, "hi").unwrap();
        p.request(&f, json!({})).unwrap();
        let r = p.poll();
        assert_eq!(r.len(), 1);
        assert!(r[0].answer.is_err());
    }
}
