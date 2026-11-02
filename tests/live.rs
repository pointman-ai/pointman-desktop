//! By hand only, on a machine with a desktop: `cargo test --test live -- --ignored --nocapture`. The extension
//! installs the real pinned agent-desktop into a scratch state folder, then reads this machine without acting on
//! anything: status, the running apps, and on macOS a snapshot of Finder in the shared session with a cursor.

use pointman_extension::testing::StandIn;
use serde_json::json;

#[test]
#[ignore]
fn reads_this_machine() {
    let node = StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_desktop"))
        .settings(json!({}))
        .machine("live")
        .start()
        .unwrap();
    let status = node.run("desktop.status", json!({})).unwrap();
    println!("{}", status.result["text"]);
    assert_eq!(status.result["ok"], true, "{}", status.result);
    let apps = node.run("desktop.run", json!({"args": ["list-apps"], "agent": "live-test"})).unwrap();
    println!("list-apps: ok={} {}", apps.result["ok"], apps.result["text"]);
    if cfg!(target_os = "macos") {
        let snap = node
            .run(
                "desktop.run",
                json!({"args": ["snapshot", "--app", "Finder", "-i", "--compact", "--skeleton"],
                "agent": "live-test", "doing": "reading Finder"}),
            )
            .unwrap();
        println!("{}", snap.result["text"]);
        assert_eq!(snap.result["ok"], true, "{}", snap.result);
        let saved = std::fs::read_to_string(node.state_dir_path().join("session.json")).unwrap();
        println!("shared session: {saved}");
    }
}
