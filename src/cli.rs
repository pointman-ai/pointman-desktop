//! One agent-desktop call: its argv from what an agent asked, run with a deadline and cancellable, and its answer
//! (one JSON envelope on stdout: `{ok, command, data | error}`) read back.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde_json::Value;

/// More output than this isn't an envelope an agent can use.
const STDOUT_MAX: u64 = 64 * 1024 * 1024;
const STDERR_MAX: u64 = 64 * 1024;

pub struct Call<'a> {
    pub bin: &'a Path,
    pub argv: &'a [String],
    /// Set for this call, over the node's own environment.
    pub env: &'a [(String, String)],
    /// Taken out of the node's environment for this call.
    pub unset: &'a [&'a str],
    pub cwd: &'a Path,
    pub timeout: Duration,
}

#[derive(Debug, Default)]
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// Run it to its end, its deadline, or `cancelled()`: then it's killed. Its output is what it wrote by the time it
/// ended: a process it left behind holding the pipes (wl-copy, owning a Wayland clipboard it set) isn't waited for.
pub fn run(call: &Call, cancelled: &dyn Fn() -> bool) -> anyhow::Result<Ran> {
    let mut cmd = Command::new(call.bin);
    cmd.args(call.argv).current_dir(call.cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // a scope from the node's own environment would send every call into someone else's session
    for name in ["AGENT_DESKTOP_SESSION", "AGENT_DESKTOP_AGENT_ID", "AGENT_DESKTOP_HOME"] {
        cmd.env_remove(name);
    }
    for name in call.unset {
        cmd.env_remove(name);
    }
    for (k, v) in call.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", call.bin.display()))?;
    let out = Pipe::read(child.stdout.take().context("its stdout")?, STDOUT_MAX);
    let err = Pipe::read(child.stderr.take().context("its stderr")?, STDERR_MAX);
    let started = Instant::now();
    let mut ran = Ran::default();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if cancelled() {
            ran.cancelled = true;
        } else if started.elapsed() > call.timeout {
            ran.timed_out = true;
        }
        if ran.cancelled || ran.timed_out {
            let _ = child.kill();
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(20));
    };
    ran.code = status.and_then(|s| s.code());
    let grace = Instant::now() + LINGER;
    ran.stdout = out.take(grace);
    ran.stderr = err.take(grace);
    Ok(ran)
}

/// How long output may keep coming after the CLI has ended, before what's there is taken.
const LINGER: Duration = Duration::from_millis(500);

/// One of its pipes, read on a thread into a buffer that can be taken before the pipe closes.
struct Pipe {
    buf: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
}

impl Pipe {
    fn read(mut from: impl Read + Send + 'static, max: u64) -> Pipe {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (b, d) = (buf.clone(), done.clone());
        thread::spawn(move || {
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match from.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = b.lock().unwrap_or_else(|e| e.into_inner());
                        // past the most it keeps, the rest is drained, so a chatty CLI never blocks on a full pipe
                        let room = (max as usize).saturating_sub(b.len());
                        b.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
            d.store(true, Ordering::SeqCst);
        });
        Pipe { buf, done }
    }

    /// What it has: once the pipe closes, or at `by`, whichever is first.
    fn take(&self, by: Instant) -> Vec<u8> {
        while !self.done.load(Ordering::SeqCst) && Instant::now() < by {
            thread::sleep(Duration::from_millis(10));
        }
        std::mem::take(&mut *self.buf.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// The envelope it printed. agent-desktop prints one even for a refused argument (exit 2); none means it crashed.
pub fn envelope(ran: &Ran) -> anyhow::Result<Value> {
    if ran.cancelled {
        bail!("cancelled");
    }
    let text = String::from_utf8_lossy(&ran.stdout);
    let found = text.trim();
    match serde_json::from_str::<Value>(found) {
        Ok(v) if v.get("ok").is_some_and(Value::is_boolean) => Ok(v),
        _ if ran.timed_out => bail!("agent-desktop didn't answer in time and was stopped{}", tail(&ran.stderr)),
        _ => bail!(
            "agent-desktop answered without its JSON (exit {}){}{}",
            ran.code.map_or("by a signal".to_string(), |c| c.to_string()),
            if found.is_empty() { String::new() } else { format!(": {}", clip(found, 400)) },
            tail(&ran.stderr)
        ),
    }
}

fn tail(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let s = s.trim();
    if s.is_empty() {
        String::new()
    } else {
        format!(". Its stderr: {}", clip_end(s, 600))
    }
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

fn clip_end(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    format!("…{}", s.chars().skip(n - max).collect::<String>())
}

/// The session this call ran in has ended (another caller, or `session gc`): start a new one and try again.
pub fn session_ended(env: &Value) -> bool {
    env["ok"] == false
        && env["error"]["details"]["kind"] == "session_ended"
        && env["error"]["disposition"]["delivery"] == "not_delivered"
}

/// Whether `args` already pick their own scope, so the extension's session and agent id stay out of them.
pub fn scoped(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--session" || a.starts_with("--session=") || a == "--agent-id" || a.starts_with("--agent-id="))
}

/// An agent's handle as agent-desktop's agent id: 1 to 64 letters, digits, `-` or `_`.
pub fn agent_id(handle: Option<&str>) -> String {
    let h: String = handle
        .unwrap_or_default()
        .trim()
        .trim_start_matches('@')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if h.is_empty() {
        "agent".to_string()
    } else {
        h
    }
}

/// What the cursor shows beside it: the agent's handle, and what it's doing in a few words.
pub fn label(agent: &str, doing: Option<&str>) -> String {
    let doing: String = doing.unwrap_or_default().split_whitespace().take(5).collect::<Vec<_>>().join(" ");
    let doing = clip(&doing, 48);
    if doing.is_empty() {
        format!("@{agent}")
    } else {
        format!("@{agent}: {doing}")
    }
}

/// One line about an answer, for whoever reads the job: what it did, or the error and what to do.
pub fn summary(env: &Value) -> String {
    let command = env["command"].as_str().unwrap_or("agent-desktop");
    if env["ok"] != true {
        let e = &env["error"];
        let mut s = format!(
            "{command}: {} {}",
            e["code"].as_str().unwrap_or("ERROR"),
            e["message"].as_str().unwrap_or("failed")
        );
        if let Some(how) = e["suggestion"].as_str() {
            s.push_str(&format!(" ({how})"));
        }
        if let Some(retry) = e["disposition"]["retry"].as_str() {
            s.push_str(&format!(" Retry: {retry}."));
        }
        return s;
    }
    let d = &env["data"];
    let mut s = command.to_string();
    // a list: how many (list-apps answers {apps: [...]}, list-windows a bare array)
    let list =
        d.as_array().or_else(|| d.as_object().filter(|o| o.len() == 1).and_then(|o| o.values().next()?.as_array()));
    if let Some(items) = list {
        s.push_str(&format!(": {} found", items.len()));
    }
    if let Some(app) = d["app"].as_str() {
        s.push_str(&format!(": {app}"));
    }
    if let Some(title) = d["window"]["title"].as_str().filter(|t| !t.is_empty()) {
        s.push_str(&format!(", {title}"));
    }
    if let Some(n) = d["ref_count"].as_u64() {
        s.push_str(&format!(", {n} refs (snapshot {})", d["snapshot_id"].as_str().unwrap_or("?")));
    }
    if let Some(delivery) = d["disposition"]["delivery"].as_str() {
        s.push_str(&format!(", {delivery}"));
    }
    s
}

/// A screenshot without a path comes back as base64 PNG in `data.data`: write it to `dir` and put its name there
/// instead, so the answer stays small and the picture can be posted.
pub fn take_screenshot(env: &mut Value, dir: &Path, n: usize) -> anyhow::Result<Option<PathBuf>> {
    use base64::Engine;
    if env["command"] != "screenshot" || env["ok"] != true {
        return Ok(None);
    }
    let Some(b64) = env["data"]["data"].as_str() else { return Ok(None) };
    let format = env["data"]["format"].as_str().unwrap_or("png").to_string();
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).context("its screenshot isn't base64")?;
    let name = format!("screenshot-{n}.{}", if format == "jpeg" || format == "jpg" { "jpg" } else { "png" });
    let path = dir.join(&name);
    std::fs::write(&path, bytes)?;
    env["data"]["data"] = Value::String(format!("(posted as {name})"));
    env["data"]["file"] = Value::String(name);
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_ids_are_what_agent_desktop_takes() {
        assert_eq!(agent_id(Some("@patchbay")), "patchbay");
        assert_eq!(agent_id(Some(" deskprobe ")), "deskprobe");
        assert_eq!(agent_id(Some("a b/c")), "abc");
        assert_eq!(agent_id(Some("@")), "agent");
        assert_eq!(agent_id(None), "agent");
        assert_eq!(agent_id(Some(&"x".repeat(80))).len(), 64);
    }

    #[test]
    fn labels_are_short() {
        assert_eq!(label("patchbay", None), "@patchbay");
        assert_eq!(label("patchbay", Some("  opening   the render  ")), "@patchbay: opening the render");
        assert_eq!(label("p", Some("one two three four five six seven")), "@p: one two three four five");
    }

    #[test]
    fn own_scope_is_left_alone() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(scoped(&a(&["--session", "s1", "snapshot"])));
        assert!(scoped(&a(&["snapshot", "--agent-id=x"])));
        assert!(!scoped(&a(&["snapshot", "--app", "Finder"])));
    }

    #[test]
    fn an_ended_session_is_seen() {
        let e = json!({"ok": false, "command": "snapshot", "error": {"code": "INVALID_ARGS",
            "details": {"kind": "session_ended", "session_id": "run-1"},
            "disposition": {"delivery": "not_delivered", "retry": "safe"}, "message": "Session 'run-1' has ended"}});
        assert!(session_ended(&e));
        let other = json!({"ok": false, "error": {"code": "STALE_REF", "disposition": {"delivery": "not_delivered"}}});
        assert!(!session_ended(&other));
    }

    #[test]
    fn summaries_say_what_happened() {
        let snap = json!({"ok": true, "command": "snapshot", "data": {"app": "Finder", "window": {"title": "Downloads"},
            "ref_count": 107, "snapshot_id": "s2y"}});
        assert_eq!(summary(&snap), "snapshot: Finder, Downloads, 107 refs (snapshot s2y)");
        assert_eq!(
            summary(&json!({"ok": true, "command": "list-apps", "data": {"apps": [1, 2, 3]}})),
            "list-apps: 3 found"
        );
        assert_eq!(summary(&json!({"ok": true, "command": "list-windows", "data": [1]})), "list-windows: 1 found");
        let err = json!({"ok": false, "command": "click", "error": {"code": "STALE_REF", "message": "Ref is stale",
            "suggestion": "Take a new snapshot", "disposition": {"retry": "safe"}}});
        assert_eq!(summary(&err), "click: STALE_REF Ref is stale (Take a new snapshot) Retry: safe.");
    }

    #[test]
    fn a_screenshot_becomes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = json!({"ok": true, "command": "screenshot", "data": {"data": "iVBORw0KGgo=", "format": "png",
            "width": 2, "height": 2}});
        let path = take_screenshot(&mut env, dir.path(), 1).unwrap().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG\r\n\x1a\n");
        assert_eq!(env["data"]["file"], "screenshot-1.png");
        assert_eq!(env["data"]["data"], "(posted as screenshot-1.png)");
        let mut other = json!({"ok": true, "command": "snapshot", "data": {"data": "x"}});
        assert!(take_screenshot(&mut other, dir.path(), 2).unwrap().is_none());
    }

    #[test]
    fn no_envelope_is_an_error_with_its_stderr() {
        let ran = Ran { code: Some(101), stderr: b"thread 'main' panicked".to_vec(), ..Default::default() };
        let e = envelope(&ran).unwrap_err().to_string();
        assert!(e.contains("without its JSON (exit 101)") && e.contains("panicked"), "{e}");
        let ok = Ran { code: Some(1), stdout: br#"{"ok": false, "error": {}}"#.to_vec(), ..Default::default() };
        assert_eq!(envelope(&ok).unwrap()["ok"], false);
    }
}
