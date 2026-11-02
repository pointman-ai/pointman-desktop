//! What the tests share: the extension under the SDK's stand-in node, with a fake `agent-desktop` (a shell script
//! that logs each call beside itself and answers as the real one does). No test reaches GitHub or a real desktop.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pointman_extension::testing::StandIn;
use serde_json::Value;
use tempfile::TempDir;

pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The fake CLI. It finds its folder from how it was started, so each test's is a link to this one script, written
/// once: one test writing a script while another starts a process is "text file busy" on Linux.
const FAKE: &str = r#"#!/bin/sh
here=$(dirname "$0")
{
  echo "----"
  echo "cwd=$(pwd)"
  echo "home=${AGENT_DESKTOP_HOME-<none>}"
  echo "scope=${AGENT_DESKTOP_SESSION-<none>}|${AGENT_DESKTOP_AGENT_ID-<none>}"
  for a in "$@"; do echo "arg=$a"; done
} >> "$here/calls.log"
session=""; prev=""; words=""
for a in "$@"; do
  if [ "$prev" = "--session" ]; then session="$a"; fi
  case "$a" in --session|--agent-id) ;; *) if [ "$prev" != "--session" ] && [ "$prev" != "--agent-id" ]; then words="$words $a"; fi;; esac
  prev="$a"
done
set -- $words
if [ -n "$session" ] && [ -f "$here/ended" ] && [ "$(cat "$here/ended")" = "$session" ]; then
  printf '{"ok":false,"command":"%s","version":"2.5","error":{"code":"INVALID_ARGS","details":{"kind":"session_ended","session_id":"%s"},"disposition":{"delivery":"not_delivered","retry":"safe"},"message":"Session has ended"}}\n' "$1" "$session"
  exit 1
fi
case "$1" in
  version) echo '{"ok":true,"command":"version","version":"2.5","data":{"version":"1.0.2","os":"macos","target":"aarch64"}}';;
  status)
    a=granted; [ -f "$here/denied" ] && a=denied
    printf '{"ok":true,"command":"status","version":"2.5","data":{"version":"1.0.2","platform":"macos","permissions":{"accessibility":{"state":"%s"},"screen_recording":{"state":"granted"}}}}\n' "$a";;
  permissions) echo '{"ok":true,"command":"permissions","version":"2.5","data":{"accessibility":{"state":"granted"}}}';;
  session)
    case "$2" in
      start)
        n=$(( $(cat "$here/sessions" 2>/dev/null || echo 0) + 1 )); echo $n > "$here/sessions"
        printf '{"ok":true,"command":"session","version":"2.5","data":{"session_id":"run-%s","cursor_overlay":{"enabled":true,"multi_agent":true}}}\n' "$n";;
      *) echo '{"ok":true,"command":"session","version":"2.5","data":{}}';;
    esac;;
  cursor-overlay) echo '{"ok":true,"command":"cursor-overlay","version":"2.5","data":{"cursor_overlay":{"enabled":true}}}';;
  snapshot) echo '{"ok":true,"command":"snapshot","version":"2.5","data":{"app":"Finder","window":{"id":"w-1","title":"Downloads"},"snapshot_id":"s1","ref_count":3,"tree":{"role":"window","children":[{"role":"button","name":"Back","ref_id":"@s1:e1"}]}}}';;
  big) printf '{"ok":true,"command":"snapshot","version":"2.5","data":{"pad":"%s"}}\n' "$(head -c 700000 /dev/zero | tr '\0' 'x')";;
  click) echo '{"ok":true,"command":"click","version":"2.5","data":{"disposition":{"delivery":"delivered","retry":"unsafe"}}}';;
  screenshot)
    if [ -n "$4" ]; then printf 'PNGFILE' > "$4"; echo '{"ok":true,"command":"screenshot","version":"2.5","data":{"path":"x"}}'
    else echo '{"ok":true,"command":"screenshot","version":"2.5","data":{"data":"iVBORw0KGgo=","format":"png","width":2,"height":2}}'; fi;;
  wait) sleep "$2"; echo '{"ok":true,"command":"wait","version":"2.5","data":{}}';;
  crash) echo "thread 'main' panicked" >&2; exit 101;;
  *) printf '{"ok":false,"command":"%s","version":"2.5","error":{"code":"INVALID_ARGS","message":"unknown command","disposition":{"delivery":"not_delivered","retry":"safe"}}}\n' "$1"; exit 2;;
esac
"#;

fn script() -> PathBuf {
    static SCRIPT: OnceLock<PathBuf> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("pointman-desktop-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("fake-agent-desktop");
            std::fs::write(&path, FAKE).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        })
        .clone()
}

/// One test's fake CLI: its folder (the log, its sessions) and its path.
pub struct Fake {
    pub dir: TempDir,
}

impl Fake {
    pub fn new() -> Fake {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(script(), dir.path().join("agent-desktop")).unwrap();
        Fake { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("agent-desktop")
    }

    /// Each call it had: its working folder, AGENT_DESKTOP_HOME, scope variables and args.
    pub fn calls(&self) -> Vec<FakeCall> {
        calls_in(&self.dir.path().join("calls.log"))
    }

    /// The calls whose first word (after the scope flags) is `word`.
    pub fn calls_of(&self, word: &str) -> Vec<FakeCall> {
        self.calls().into_iter().filter(|c| c.word() == word).collect()
    }

    /// The session `id` has ended, as another caller or `session gc` would.
    pub fn end(&self, id: &str) {
        std::fs::write(self.dir.path().join("ended"), id).unwrap();
    }

    /// macOS hasn't given the node Accessibility.
    pub fn deny(&self) {
        std::fs::write(self.dir.path().join("denied"), "").unwrap();
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeCall {
    pub cwd: String,
    pub home: String,
    pub scope: String,
    pub args: Vec<String>,
}

impl FakeCall {
    /// Its command: the first word that isn't a scope flag or a scope flag's value.
    pub fn word(&self) -> String {
        let mut skip = false;
        for a in &self.args {
            if skip {
                skip = false;
                continue;
            }
            if a == "--session" || a == "--agent-id" {
                skip = true;
                continue;
            }
            if a == "--headed" {
                continue;
            }
            return a.clone();
        }
        String::new()
    }

    pub fn flag(&self, name: &str) -> Option<String> {
        let at = self.args.iter().position(|a| a == name)?;
        self.args.get(at + 1).cloned()
    }
}

pub fn calls_in(log: &Path) -> Vec<FakeCall> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut calls = Vec::new();
    for record in text.split("----\n").filter(|r| !r.trim().is_empty()) {
        let mut c = FakeCall::default();
        for line in record.lines() {
            match line.split_once('=') {
                Some(("cwd", v)) => c.cwd = v.into(),
                Some(("home", v)) => c.home = v.into(),
                Some(("scope", v)) => c.scope = v.into(),
                Some(("arg", v)) => c.args.push(v.into()),
                _ => {}
            }
        }
        calls.push(c);
    }
    calls
}

/// The extension under the stand-in, with the fake as its CLI and `settings`.
pub fn node(fake: &Fake, settings: Value) -> StandIn {
    StandIn::new(ROOT)
        .binary(env!("CARGO_BIN_EXE_desktop"))
        .settings(settings)
        .machine("testbox")
        .env("POINTMAN_DESKTOP_CLI", fake.path())
        .env("AGENT_DESKTOP_SESSION", "someone-elses")
        .env("AGENT_DESKTOP_AGENT_ID", "someone")
        .start()
        .unwrap()
}

/// health.check's part `id`.
pub fn part(node: &StandIn, id: &str) -> Value {
    let parts = node.call("health.check", serde_json::json!({})).unwrap();
    parts["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no {id} in {parts}"))
}
