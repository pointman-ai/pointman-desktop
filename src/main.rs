//! The extension's process: the node starts it (`bin/desktop`, from the extension's folder) and talks to it over
//! stdin and stdout (the SDK handles that). Log with `h.log(…)`: stdout is the node's.
//!
//! `desktop.run` runs one agent-desktop command on this machine for an agent anywhere (create_job with
//! `machine`). Every call shares one agent-desktop session here, made with a cursor for each agent (macOS), so a
//! person watching the screen pane sees whose cursor acts and where. Each call runs on a thread of its own (no
//! lane), and can be cancelled: the CLI is killed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use desktop::cli::{self, Call};
use desktop::{install, session};
use pointman_extension::protocol::output_request::Role;
use pointman_extension::protocol::{HealthCheckRequest, HealthCheckResponse, HealthFixRequest, HealthFixResponse};
use pointman_extension::v1::health::part::State;
use pointman_extension::v1::health::{Action, Part};
use pointman_extension::{Extension, Handle, Job};
use serde_json::{json, Value};

/// A call nobody gave a deadline: most take a second or two; `wait` and slow apps take longer.
const DEFAULT_TIMEOUT: u64 = 60;
const MAX_TIMEOUT: u64 = 600;
/// The extension's own calls (sessions, cursors, health).
const QUICK: Duration = Duration::from_secs(30);
/// The shared session starts again after this: its refs are long stale, and its files are cleared.
const SESSION_DAYS: u64 = 1;
/// Health reads permissions at most this often.
const HEALTH_KEPT: Duration = Duration::from_secs(60);
/// An answer bigger than this goes back as a file, with the envelope's small parts.
const ANSWER_MAX: usize = 512 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Default)]
struct Shared {
    /// The session every call runs in, and when it started.
    id: Option<String>,
    started: u64,
    /// Each agent's cursor label, as last set in that session.
    cursors: HashMap<String, String>,
}

struct Ext {
    h: Handle,
    /// The last install's error; held while one runs.
    installing: Mutex<Option<String>>,
    session: Mutex<Shared>,
    health: Mutex<Option<(Instant, Result<Value, String>)>>,
}

impl Ext {
    fn state(&self) -> PathBuf {
        let s = self.h.state_dir();
        if s.is_absolute() {
            s
        } else {
            std::env::current_dir().map(|d| d.join(&s)).unwrap_or(s)
        }
    }

    fn setting(&self, name: &str, default: bool) -> bool {
        self.h.settings()[name].as_bool().unwrap_or(default)
    }

    /// The CLI: the pinned release, installed now if it isn't yet. POINTMAN_DESKTOP_CLI points the tests at a fake.
    fn cli(&self) -> anyhow::Result<PathBuf> {
        if let Some(p) = std::env::var_os("POINTMAN_DESKTOP_CLI").filter(|p| !p.is_empty()) {
            return Ok(PathBuf::from(p));
        }
        if let Some(why) = session::no_loader() {
            bail!(why);
        }
        let state = self.state();
        if let Some(p) = install::installed(&state) {
            return Ok(p);
        }
        let mut last = lock(&self.installing);
        if let Some(p) = install::installed(&state) {
            return Ok(p);
        }
        let releases = std::env::var("POINTMAN_DESKTOP_RELEASES").unwrap_or_else(|_| install::RELEASES.to_string());
        match install::install(&state, &releases) {
            Ok(p) => {
                *last = None;
                self.h.log(format!("installed agent-desktop {} at {}", install::VERSION, p.display()));
                Ok(p)
            }
            Err(e) => {
                *last = Some(format!("{e:#}"));
                Err(e.context(format!("installing agent-desktop {}", install::VERSION)))
            }
        }
    }

    /// agent-desktop's own state (snapshots' refs, sessions): in the extension's folder, private, as it requires.
    fn home(&self) -> anyhow::Result<PathBuf> {
        let home = self.state().join("home");
        std::fs::create_dir_all(&home).with_context(|| format!("making {}", home.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(home)
    }

    fn env(&self, home: &Path) -> Vec<(String, String)> {
        let mut env = vec![("AGENT_DESKTOP_HOME".to_string(), home.to_string_lossy().to_string())];
        env.extend(session::env());
        env
    }

    /// One of the extension's own calls, in its scratch folder, read as an envelope.
    fn quick(&self, bin: &Path, home: &Path, argv: &[&str]) -> anyhow::Result<Value> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let env = self.env(home);
        let call = Call { bin, argv: &argv, env: &env, unset: session::unset(), cwd: home, timeout: QUICK };
        cli::envelope(&cli::run(&call, &|| false)?)
    }

    /// The session every call shares, with a cursor for each agent: started when there's none, or the day's is
    /// over. Only where a cursor can be drawn (macOS) and the `cursor` setting is on; elsewhere, none.
    fn shared_session(&self, bin: &Path, home: &Path) -> Option<String> {
        if !cfg!(target_os = "macos") || !self.setting("cursor", true) {
            return None;
        }
        let mut s = lock(&self.session);
        if s.id.is_none() {
            // the last process's, if it's still today's
            if let Ok(saved) = std::fs::read_to_string(self.state().join("session.json")) {
                let v: Value = serde_json::from_str(&saved).unwrap_or_default();
                if let (Some(id), Some(at)) = (v["id"].as_str(), v["started"].as_u64()) {
                    s.id = Some(id.to_string());
                    s.started = at;
                }
            }
        }
        if s.id.is_some() && now().saturating_sub(s.started) < SESSION_DAYS * 86_400 {
            return s.id.clone();
        }
        if let Some(old) = s.id.take() {
            let _ = self.quick(bin, home, &["session", "end", &old]);
            let _ = self.quick(bin, home, &["session", "gc", "--ended"]);
        }
        s.cursors.clear();
        let started = self.quick(
            bin,
            home,
            &["session", "start", "--name", "pointman", "--no-trace", "--cursor", "--multi-agent"],
        );
        match started.as_ref().ok().and_then(|v| v["data"]["session_id"].as_str()) {
            Some(id) => {
                s.id = Some(id.to_string());
                s.started = now();
                let saved = json!({"id": id, "started": s.started}).to_string();
                let _ = std::fs::write(self.state().join("session.json"), saved);
                s.id.clone()
            }
            None => {
                self.h.log(format!("couldn't start the shared session: {started:?}"));
                None
            }
        }
    }

    fn forget_session(&self) {
        let mut s = lock(&self.session);
        s.id = None;
        s.cursors.clear();
        let _ = std::fs::remove_file(self.state().join("session.json"));
    }

    /// The agent's cursor, labelled with its handle and what it's doing: set when it's new or the label changed.
    fn cursor(&self, bin: &Path, home: &Path, id: &str, agent: &str, label: &str) {
        if lock(&self.session).cursors.get(agent).map(String::as_str) == Some(label) {
            return;
        }
        let set = self.quick(
            bin,
            home,
            &["--session", id, "--agent-id", agent, "cursor-overlay", "enable", "--label", label],
        );
        match set {
            Ok(v) if v["ok"] == true => {
                lock(&self.session).cursors.insert(agent.to_string(), label.to_string());
            }
            other => self.h.log(format!("couldn't set {agent}'s cursor: {other:?}")),
        }
    }
}

fn strings(v: &Value, name: &str) -> anyhow::Result<Vec<String>> {
    let Some(list) = v.as_array() else {
        bail!("{name} is the words after `agent-desktop`, as a list: [\"snapshot\", \"--app\", \"Finder\", \"-i\"]")
    };
    list.iter()
        .map(|a| a.as_str().map(String::from).context(format!("{name} has something that isn't a word")))
        .collect()
}

/// desktop.run: one agent-desktop command, its envelope back, a screenshot or a file it wrote posted.
fn run(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let args = strings(&job.params["args"], "args")?;
    if args.is_empty() {
        bail!("args is empty: the words after `agent-desktop`, [\"snapshot\", \"--app\", \"Finder\", \"-i\"]");
    }
    let asked_headed = job.params["headed"].as_bool().unwrap_or(false);
    if (asked_headed || args.iter().any(|a| a == "--headed")) && !x.setting("headed", false) {
        bail!(
            "Physical input is off on {} (the Desktop extension's \"Physical input\" setting): act on refs without \
             headed, or ask this machine's person to turn it on.",
            x.h.machine()
        );
    }
    let agent = cli::agent_id(job.params["agent"].as_str());
    let label = cli::label(&agent, job.params["doing"].as_str());
    let timeout = Duration::from_secs(job.params["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT));

    job.progress(None, "agent-desktop");
    let bin = x.cli()?;
    let home = x.home()?;
    std::fs::create_dir_all(&job.work).with_context(|| format!("making {}", job.work.display()))?;
    let before = files_in(&job.work);

    let mut tries = 0;
    let mut env = loop {
        tries += 1;
        let shared = if cli::scoped(&args) { None } else { x.shared_session(&bin, &home) };
        let mut argv = Vec::new();
        if let Some(id) = &shared {
            x.cursor(&bin, &home, id, &agent, &label);
            argv.extend(["--session".to_string(), id.clone(), "--agent-id".to_string(), agent.clone()]);
        }
        if asked_headed && !args.iter().any(|a| a == "--headed") {
            argv.push("--headed".to_string());
        }
        argv.extend(args.iter().cloned());
        let vars = x.env(&home);
        let call = Call { bin: &bin, argv: &argv, env: &vars, unset: session::unset(), cwd: &job.work, timeout };
        let ran = cli::run(&call, &|| job.cancelled())?;
        if ran.cancelled {
            bail!(pointman_extension::Cancelled);
        }
        let env = cli::envelope(&ran)?;
        if shared.is_some() && cli::session_ended(&env) && tries == 1 {
            x.forget_session();
            continue;
        }
        break env;
    };

    // what it made: a screenshot's picture, and any file it wrote where it ran (a screenshot or trace path)
    let shot = cli::take_screenshot(&mut env, &job.work, 1)?;
    let made: Vec<PathBuf> = files_in(&job.work).into_iter().filter(|f| !before.contains(f)).collect();
    for f in &made {
        let image = matches!(ext(f).as_str(), "png" | "jpg" | "jpeg");
        job.output(f, if image { "image" } else { "document" }, Role::Main, json!({"file": name_of(f)}));
    }
    let text = cli::summary(&env);
    let mut answer = env;
    let size = answer.to_string().len();
    if size > ANSWER_MAX {
        let name = format!("{}.json", file_stem(answer["command"].as_str().unwrap_or_default()));
        let path = job.work.join(&name);
        std::fs::write(&path, serde_json::to_vec_pretty(&answer)?)?;
        job.output(&path, "document", Role::Main, json!({"file": name}));
        answer["data"] = json!({"file": name, "bytes": size, "note": "Too big to answer inline, so it's posted as a \
            file. Narrow it: snapshot -i --compact, --skeleton, then --root <ref>; or find."});
    }
    answer["text"] = Value::String(match shot {
        Some(p) => format!("{text}: posted {}", name_of(&p)),
        None => text,
    });
    answer["machine"] = Value::String(x.h.machine());
    Ok(answer)
}

/// A file name made from agent-desktop's `command`: only `[a-z0-9-]`, so it stays in the job's folder.
fn file_stem(command: &str) -> String {
    let stem: String = command
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(40)
        .collect();
    if stem.is_empty() {
        "answer".into()
    } else {
        stem
    }
}

/// desktop.status: the version, the platform and what this machine lets it do.
fn status(x: &Ext, _job: &Job) -> anyhow::Result<Value> {
    let bin = x.cli()?;
    let home = x.home()?;
    let v = x.quick(&bin, &home, &["status"])?;
    let mut out = v.clone();
    out["text"] = Value::String(status_line(&x.h.machine(), &v));
    Ok(out)
}

fn status_line(machine: &str, v: &Value) -> String {
    let d = &v["data"];
    if v["ok"] != true {
        return format!("agent-desktop on {machine}: {}", cli::summary(v));
    }
    let p = &d["permissions"];
    let perm = |k: &str| p[k]["state"].as_str().unwrap_or("unknown").to_string();
    format!(
        "agent-desktop {} on {machine} ({}): accessibility {}, screen recording {}",
        d["version"].as_str().unwrap_or("?"),
        d["platform"].as_str().unwrap_or("?"),
        perm("accessibility"),
        perm("screen_recording")
    )
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    files.sort();
    files
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn part(id: &str, kind: &str, name: &str, state: State, needs: Vec<Action>, why: String) -> Part {
    Part {
        id: id.into(),
        kind: kind.into(),
        name: Some(name.into()),
        state: state.into(),
        needs: needs.into_iter().map(Into::into).collect(),
        why: Some(why),
        ..Default::default()
    }
}

/// The install's last error, or None while one is going.
fn lock_try(m: &Mutex<Option<String>>) -> Option<Option<String>> {
    match m.try_lock() {
        Ok(g) => Some(g.clone()),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner().clone()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Health's two parts: agent-desktop itself, and whether this machine lets it see and act on apps.
fn health(x: &Ext) -> Vec<Part> {
    let state = x.state();
    let pinned = format!("agent-desktop {} (pinned, sha256 checked)", install::VERSION);
    let found = std::env::var_os("POINTMAN_DESKTOP_CLI").map(PathBuf::from).or_else(|| install::installed(&state));
    let Some(bin) = found else {
        let why = match lock_try(&x.installing) {
            None => format!("installing {pinned}"),
            Some(Some(e)) => format!("couldn't install {pinned}: {e}"),
            Some(None) => format!("not installed yet: {pinned} installs at the first call"),
        };
        return vec![part("cli", "cli", "agent-desktop", State::Failing, vec![Action::Update], why)];
    };
    let cli = Part {
        version: Some(install::VERSION.into()),
        ..part("cli", "cli", "agent-desktop", State::Ok, vec![], pinned)
    };
    let mut kept = lock(&x.health);
    let read = match kept.as_ref().filter(|(at, _)| at.elapsed() < HEALTH_KEPT) {
        Some((_, r)) => r.clone(),
        None => {
            let r = x.home().and_then(|home| x.quick(&bin, &home, &["status"])).map_err(|e| format!("{e:#}"));
            *kept = Some((Instant::now(), r.clone()));
            r
        }
    };
    let access =
        |state: State, needs: Vec<Action>, why: String| part("access", "permission", "Access", state, needs, why);
    let access = match read {
        Err(e) => access(State::Failing, vec![Action::Check], format!("agent-desktop doesn't run: {e}")),
        Ok(v) if v["ok"] != true => access(State::Failing, vec![Action::Check], cli::summary(&v)),
        Ok(v) => {
            let a = v["data"]["permissions"]["accessibility"]["state"].as_str().unwrap_or("unknown");
            match a {
                "granted" | "not_required" => access(State::Ok, vec![], status_line(&x.h.machine(), &v)),
                "denied" if cfg!(target_os = "macos") => access(
                    State::Failing,
                    vec![Action::Check],
                    format!(
                        "macOS doesn't let the node control apps: System Settings › Privacy & Security › \
                         Accessibility, turn on {}, then restart the node",
                        node_program()
                    ),
                ),
                "denied" => access(
                    State::Failing,
                    vec![Action::Check],
                    "AT-SPI, Linux's accessibility bus, is off in this desktop session, so no app's tree can be read. \
                     On NixOS: services.gnome.at-spi2-core.enable = true and \
                     environment.sessionVariables.QT_ACCESSIBILITY = \"1\", rebuild, log out and back in, and \
                     restart the apps."
                        .to_string(),
                ),
                other => access(
                    State::Unspecified,
                    vec![Action::Check],
                    format!("accessibility {other}: {}", status_line(&x.h.machine(), &v)),
                ),
            }
        }
    };
    vec![cli, access]
}

/// The program macOS asks about: the node, which starts this extension and so agent-desktop. System Settings lists
/// a node inside an app (Pointman.app) under the app's name, and one on its own under the file's.
fn node_program() -> String {
    match parent_path() {
        Some(path) => program_named(&path),
        None => "the Pointman node (pointman-node)".into(),
    }
}

/// How System Settings lists the program at `path`, and where its + button finds it.
fn program_named(path: &str) -> String {
    let (name, at) = match path.find(".app/") {
        Some(i) => {
            let app = &path[..i + 4];
            (app.rsplit('/').next().unwrap_or(app).trim_end_matches(".app"), app)
        }
        None => (path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("pointman-node"), path),
    };
    format!("{name} (+ adds it from {at} if it isn't listed)")
}

/// The path of the program that started this one: the node.
#[cfg(target_os = "macos")]
fn parent_path() -> Option<String> {
    let mut buf = vec![0u8; 4096];
    // SAFETY: proc_pidpath writes at most the buffer's size into it, and says how much it wrote
    let n = unsafe { libc::proc_pidpath(libc::getppid(), buf.as_mut_ptr().cast(), buf.len() as u32) };
    let path = std::str::from_utf8(buf.get(..usize::try_from(n).ok()?)?).ok()?;
    (!path.is_empty()).then(|| path.to_string())
}

#[cfg(not(target_os = "macos"))]
fn parent_path() -> Option<String> {
    None
}

fn fix(x: &Ext, req: &HealthFixRequest) -> anyhow::Result<HealthFixResponse> {
    let message = match (req.part.as_str(), req.action.as_known()) {
        ("cli", Some(Action::Update)) => {
            let p = x.cli()?;
            format!("agent-desktop {} is at {}.", install::VERSION, p.display())
        }
        ("access", Some(Action::Check)) => {
            *lock(&x.health) = None;
            let bin = x.cli()?;
            let home = x.home()?;
            if cfg!(target_os = "macos") {
                // macOS's own prompt, on this machine's screen: only someone there can allow it
                let asked = x.quick(&bin, &home, &["permissions", "--request"])?;
                format!(
                    "Asked macOS on {}: someone at it allows the node in System Settings › Privacy & Security › \
                     Accessibility, then restarts the node. {}",
                    x.h.machine(),
                    status_line(&x.h.machine(), &x.quick(&bin, &home, &["status"]).unwrap_or(asked))
                )
            } else {
                status_line(&x.h.machine(), &x.quick(&bin, &home, &["status"])?)
            }
        }
        _ => "Nothing to fix from here.".to_string(),
    };
    Ok(HealthFixResponse { message, ..Default::default() })
}

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let x = Arc::new(Ext {
        h: ext.handle(),
        installing: Mutex::new(None),
        session: Mutex::new(Shared::default()),
        health: Mutex::new(None),
    });

    // the pinned CLI is fetched as soon as the extension starts, so the first call doesn't wait for it
    let xs = x.clone();
    ext.on_start(move |_: &Handle| {
        let xs = xs.clone();
        std::thread::spawn(move || {
            if let Err(e) = xs.cli() {
                xs.h.log(format!("{e:#}"));
            }
        });
    });

    type Cmd = fn(&Ext, &Job) -> anyhow::Result<Value>;
    let commands: [(&str, Cmd); 2] = [("desktop.run", run), ("desktop.status", status)];
    for (kind, f) in commands {
        let xc = x.clone();
        ext.command(kind, move |job: &Job| f(&xc, job));
    }

    let xh = x.clone();
    ext.on(move |_: HealthCheckRequest| Ok(HealthCheckResponse { parts: health(&xh), ..Default::default() }));
    let xf = x.clone();
    ext.on(move |req: HealthFixRequest| fix(&xf, &req));

    ext.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_says_what_is_allowed() {
        let v = json!({"ok": true, "command": "status", "data": {"version": "1.0.2", "platform": "macos",
            "permissions": {"accessibility": {"state": "granted"}, "screen_recording": {"state": "denied"}}}});
        assert_eq!(
            status_line("mac-m4", &v),
            "agent-desktop 1.0.2 on mac-m4 (macos): accessibility granted, screen recording denied"
        );
    }

    #[test]
    fn macos_lists_an_apps_node_under_the_app() {
        assert_eq!(
            program_named("/Applications/Pointman.app/Contents/Resources/bin/pointman-node"),
            "Pointman (+ adds it from /Applications/Pointman.app if it isn't listed)"
        );
        assert_eq!(
            program_named("/Users/j/.local/bin/pointman-node"),
            "pointman-node (+ adds it from /Users/j/.local/bin/pointman-node if it isn't listed)"
        );
    }

    #[test]
    fn an_overflow_file_stays_in_the_jobs_folder() {
        assert_eq!(file_stem("snapshot"), "snapshot");
        assert_eq!(file_stem("list-apps"), "list-apps");
        assert_eq!(file_stem("../../.ssh/x"), "sshx");
        assert_eq!(file_stem("/etc/passwd"), "etcpasswd");
        assert_eq!(file_stem(""), "answer");
        assert_eq!(file_stem("../"), "answer");
    }
}
