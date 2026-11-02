//! The commands against the stand-in node and the fake CLI: the envelope back with a line about it, one shared
//! session with a cursor per agent (macOS), physical input only where it's allowed, screenshots posted, and calls
//! stopped at their deadline.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::*;
use pointman_extension::testing::RunOptions;
use serde_json::json;

const MAC: bool = cfg!(target_os = "macos");

#[test]
fn a_call_answers_its_envelope_and_a_line() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let done =
        node.run("desktop.run", json!({"args": ["snapshot", "--app", "Finder", "-i"], "agent": "@patchbay"})).unwrap();
    let r = &done.result;
    assert_eq!(r["ok"], true);
    assert_eq!(r["data"]["tree"]["children"][0]["ref_id"], "@s1:e1");
    assert_eq!(r["text"], "snapshot: Finder, Downloads, 3 refs (snapshot s1)");
    assert_eq!(r["machine"], "testbox");

    let snap = &fake.calls_of("snapshot")[0];
    let home = node.state_dir_path().join("home");
    assert_eq!(std::path::Path::new(&snap.home), home, "agent-desktop's state is the extension's own");
    assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700, "and private, as it requires");
    let cwd = std::path::Path::new(&snap.cwd).canonicalize().unwrap();
    assert!(cwd.starts_with(node.work_dir().canonicalize().unwrap()), "it runs in the job's folder: {}", snap.cwd);
    for c in fake.calls() {
        assert_eq!(c.scope, "<none>|<none>", "the node's own AGENT_DESKTOP_ variables never reach it");
    }
}

#[test]
fn every_agent_has_its_own_cursor_in_one_session() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let snap = json!({"args": ["snapshot", "--app", "Finder"], "agent": "patchbay", "doing": "opening the render"});
    node.run("desktop.run", snap.clone()).unwrap();
    node.run("desktop.run", json!({"args": ["click", "@s1:e1"], "agent": "@deskprobe"})).unwrap();
    node.run("desktop.run", snap).unwrap();

    if !MAC {
        assert!(fake.calls_of("session").is_empty() && fake.calls_of("cursor-overlay").is_empty());
        assert!(fake.calls_of("snapshot").iter().all(|c| c.flag("--session").is_none()));
        return;
    }
    let starts = fake.calls_of("session");
    assert_eq!(starts.len(), 1, "one session for every call: {starts:?}");
    for f in ["--cursor", "--multi-agent", "--no-trace"] {
        assert!(starts[0].args.contains(&f.to_string()), "{:?}", starts[0].args);
    }
    let cursors = fake.calls_of("cursor-overlay");
    let labels: Vec<_> = cursors.iter().map(|c| (c.flag("--agent-id").unwrap(), c.flag("--label").unwrap())).collect();
    assert_eq!(
        labels,
        [("patchbay".into(), "@patchbay: opening the render".into()), ("deskprobe".into(), "@deskprobe".into())],
        "a cursor for each agent, set once while its label stays"
    );
    for c in fake.calls_of("snapshot").iter().chain(&fake.calls_of("click")) {
        assert_eq!(c.flag("--session").as_deref(), Some("run-1"), "{:?}", c.args);
    }
    assert_eq!(fake.calls_of("click")[0].flag("--agent-id").as_deref(), Some("deskprobe"));
}

#[test]
fn an_ended_session_starts_again_once() {
    if !MAC {
        return;
    }
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    node.run("desktop.run", json!({"args": ["snapshot"], "agent": "a"})).unwrap();
    fake.end("run-1");
    let done = node.run("desktop.run", json!({"args": ["snapshot"], "agent": "a"})).unwrap();
    assert_eq!(done.result["ok"], true, "{}", done.result);
    let snaps = fake.calls_of("snapshot");
    assert_eq!(snaps.iter().map(|c| c.flag("--session").unwrap()).collect::<Vec<_>>(), ["run-1", "run-1", "run-2"]);
    assert_eq!(fake.calls_of("cursor-overlay").len(), 2, "the new session gets the agent's cursor again");
}

#[test]
fn no_cursor_no_session() {
    let fake = Fake::new();
    let node = node(&fake, json!({"cursor": false}));
    node.run("desktop.run", json!({"args": ["snapshot"], "agent": "a"})).unwrap();
    assert!(fake.calls_of("session").is_empty());
    assert_eq!(fake.calls_of("snapshot")[0].args, ["snapshot"]);
}

#[test]
fn a_call_with_its_own_scope_is_left_alone() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    node.run("desktop.run", json!({"args": ["--session", "mine", "snapshot"], "agent": "a"})).unwrap();
    assert_eq!(fake.calls_of("snapshot")[0].args, ["--session", "mine", "snapshot"]);
}

#[test]
fn physical_input_only_where_its_allowed() {
    let fake = Fake::new();
    for settings in [json!({}), json!({"headed": false})] {
        let node = node(&fake, settings);
        for params in
            [json!({"args": ["click", "@s1:e1"], "headed": true}), json!({"args": ["--headed", "click", "@s1:e1"]})]
        {
            let e = node.run("desktop.run", params).unwrap_err().to_string();
            assert!(e.contains("Physical input is off on testbox"), "{e}");
        }
    }
    assert!(fake.calls_of("click").is_empty(), "nothing ran: off unless the person turns it on");
    let ok = common::node(&fake, json!({"cursor": false, "headed": true}));
    ok.run("desktop.run", json!({"args": ["click", "@s1:e1"], "headed": true})).unwrap();
    assert_eq!(fake.calls_of("click")[0].args, ["--headed", "click", "@s1:e1"]);
}

#[test]
fn a_screenshot_is_posted_not_answered() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let done = node.run("desktop.run", json!({"args": ["screenshot", "--app", "Finder"]})).unwrap();
    assert_eq!(done.result["data"]["file"], "screenshot-1.png");
    assert!(!done.result.to_string().contains("iVBORw0KGgo"), "no base64 in the answer");
    assert_eq!(done.outputs.len(), 1);
    assert_eq!(done.outputs[0].kind, "image");
    assert!(done.result["text"].as_str().unwrap().ends_with("posted screenshot-1.png"));

    let to_file = node.run("desktop.run", json!({"args": ["screenshot", "--app", "Finder", "out.png"]})).unwrap();
    assert_eq!(to_file.outputs.len(), 1, "a file it wrote where it ran is posted too");
    assert!(to_file.outputs[0].path.ends_with("out.png"), "{:?}", to_file.outputs[0]);
}

#[test]
fn a_huge_answer_goes_back_as_a_file() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let done = node.run("desktop.run", json!({"args": ["big"]})).unwrap();
    assert_eq!(done.result["data"]["file"], "snapshot.json");
    assert!(done.result.to_string().len() < 2000);
    assert_eq!(done.outputs[0].kind, "document");
}

#[test]
fn errors_are_answers_and_crashes_are_failures() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let done = node.run("desktop.run", json!({"args": ["nonsense"]})).unwrap();
    assert_eq!(done.result["ok"], false);
    assert_eq!(done.result["error"]["code"], "INVALID_ARGS");
    assert!(done.result["text"].as_str().unwrap().starts_with("nonsense: INVALID_ARGS unknown command"));
    let e = node.run("desktop.run", json!({"args": ["crash"]})).unwrap_err().to_string();
    assert!(e.contains("without its JSON (exit 101)") && e.contains("panicked"), "{e}");
    let e = node.run("desktop.run", json!({"args": []})).unwrap_err().to_string();
    assert!(e.contains("args is empty"), "{e}");
}

#[test]
fn a_slow_call_is_stopped_at_its_deadline() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let started = std::time::Instant::now();
    let options = RunOptions { timeout: Duration::from_secs(20), ..Default::default() };
    let e = node.run_with("desktop.run", json!({"args": ["wait", "8"], "timeout": 1}), options).unwrap_err();
    assert!(e.to_string().contains("didn't answer in time"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
}

#[test]
fn status_and_health_say_what_it_may_do() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let s = node.run("desktop.status", json!({})).unwrap();
    assert_eq!(
        s.result["text"],
        "agent-desktop 1.0.2 on testbox (macos): accessibility granted, screen recording granted"
    );
    let cli = part(&node, "cli");
    assert_eq!(cli["state"], "STATE_OK", "{cli}");
    assert_eq!(part(&node, "access")["state"], "STATE_OK");

    let denied = Fake::new();
    denied.deny();
    let node = common::node(&denied, json!({}));
    let access = part(&node, "access");
    if MAC {
        assert_eq!(access["state"], "STATE_FAILING", "{access}");
        assert!(access["why"].as_str().unwrap().contains("Privacy & Security › Accessibility"), "{access}");
    } else {
        assert_ne!(access["state"], "STATE_OK", "{access}");
    }
}
