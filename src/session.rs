//! What agent-desktop needs from the person's graphical session that a node started as a service may not have.
//!
//! On macOS, nothing: the node runs in the person's login session, and agent-desktop finds the apps itself.
//! On Linux, the node may run as a systemd user unit without the session's variables, so they're found in the
//! runtime folder (`/run/user/<uid>`), as the screen extension finds Wayland's: the session bus (AT-SPI is reached
//! through it, as `org.a11y.Bus`), the Wayland socket, Hyprland's instance, and X11's display. NixOS's own
//! programs (wl-copy, for the Wayland clipboard) join the PATH, and apps agent-desktop launches get accessibility:
//! a session with AT-SPI off sets NO_AT_BRIDGE and GTK_A11Y=none for everything. (Found on nixos under Hyprland,
//! EXT-117.)

use std::path::{Path, PathBuf};

/// The variables to add for this call: only those the node's environment lacks, and only where they exist.
pub fn env() -> Vec<(String, String)> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let have = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).or_else(|| uid().map(run_user));
    let mut env = linux_env(runtime.as_deref(), &have, Path::new("/tmp/.X11-unix"));
    let user = std::env::var("USER").unwrap_or_default();
    let more =
        [PathBuf::from("/run/current-system/sw/bin"), PathBuf::from(format!("/etc/profiles/per-user/{user}/bin"))];
    if let Some(path) = path_with(std::env::var_os("PATH").as_deref(), &more) {
        env.push(("PATH".into(), path));
    }
    env.push(("QT_ACCESSIBILITY".into(), "1".into()));
    env
}

/// Taken out of the node's environment for each call (Linux): they'd keep apps it launches off the AT-SPI bus.
pub fn unset() -> &'static [&'static str] {
    if cfg!(target_os = "linux") {
        &["NO_AT_BRIDGE", "GTK_A11Y"]
    } else {
        &[]
    }
}

/// The PATH with `more` after it, those that exist and aren't on it already; None when nothing's added.
pub fn path_with(path: Option<&std::ffi::OsStr>, more: &[PathBuf]) -> Option<String> {
    let mut dirs: Vec<PathBuf> = path.map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    let before = dirs.len();
    for m in more {
        if m.is_dir() && !dirs.contains(m) {
            dirs.push(m.clone());
        }
    }
    if dirs.len() == before {
        return None;
    }
    std::env::join_paths(dirs).ok().map(|p| p.to_string_lossy().to_string())
}

/// Why the release's glibc build can't start here, if it can't: its loader isn't where it looks (NixOS without
/// nix-ld). The Linux builds are glibc's, and NixOS keeps its loader in the store.
pub fn no_loader() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let loader = match std::env::consts::ARCH {
        "aarch64" => "/lib/ld-linux-aarch64.so.1",
        _ => "/lib64/ld-linux-x86-64.so.2",
    };
    (!Path::new(loader).exists()).then(|| {
        format!(
            "agent-desktop's Linux build needs glibc's loader at {loader}, and this machine has none there. On NixOS, \
             turn on nix-ld (programs.nix-ld.enable = true) and rebuild."
        )
    })
}

fn run_user(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

/// This process's real uid, from /proc (no libc needed).
fn uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|l| l.strip_prefix("Uid:")).and_then(|r| r.split_whitespace().next()?.parse().ok())
}

pub fn linux_env(runtime: Option<&Path>, have: &dyn Fn(&str) -> bool, x11: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(rt) = runtime.filter(|rt| rt.is_dir()) else { return out };
    if !have("XDG_RUNTIME_DIR") {
        out.push(("XDG_RUNTIME_DIR".into(), rt.to_string_lossy().into()));
    }
    if !have("DBUS_SESSION_BUS_ADDRESS") && rt.join("bus").exists() {
        out.push(("DBUS_SESSION_BUS_ADDRESS".into(), format!("unix:path={}", rt.join("bus").display())));
    }
    if !have("WAYLAND_DISPLAY") {
        if let Some(w) = live_named(rt, "wayland-", |n| !n.ends_with(".lock"), Path::to_path_buf) {
            out.push(("WAYLAND_DISPLAY".into(), w));
        }
    }
    if !have("HYPRLAND_INSTANCE_SIGNATURE") {
        if let Some(h) = live_named(&rt.join("hypr"), "", |_| true, |d| d.join(".socket.sock")) {
            out.push(("HYPRLAND_INSTANCE_SIGNATURE".into(), h));
        }
    }
    if !have("DISPLAY") {
        if let Some(x) = live_named(x11, "X", |n| n[1..].chars().all(|c| c.is_ascii_digit()), Path::to_path_buf) {
            out.push(("DISPLAY".into(), format!(":{}", &x[1..])));
        }
    }
    out
}

/// The entry of `dir` that starts with `prefix`, passes `ok`, and whose socket (`socket` of its path) takes a
/// connection; the newest of them when there are several. A session that crashed leaves its files behind
/// (`hypr/<signature>/`, `wayland-1`), so the first by name can be a dead one.
fn live_named(
    dir: &Path,
    prefix: &str,
    ok: impl Fn(&str) -> bool,
    socket: impl Fn(&Path) -> PathBuf,
) -> Option<String> {
    let mut live: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with(prefix) && name.len() > prefix.len() && ok(&name)) {
                return None;
            }
            let sock = socket(&e.path());
            if !listening(&sock) {
                return None;
            }
            let at = std::fs::metadata(&sock).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            Some((at, name))
        })
        .collect();
    live.sort();
    live.pop().map(|(_, name)| name)
}

/// Something is listening on this Unix socket: connecting and hanging up at once, as a client that changed its
/// mind would. A dead session's socket refuses.
#[cfg(unix)]
fn listening(sock: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(sock).is_ok()
}

#[cfg(not(unix))]
fn listening(_: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder short enough for Unix socket paths (macOS allows 104 bytes; its temp folder is long).
    fn short_dir() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("de").tempdir_in("/tmp").unwrap()
    }

    /// A socket that's listening, kept open by the listener it returns.
    fn serve(path: &Path) -> std::os::unix::net::UnixListener {
        std::os::unix::net::UnixListener::bind(path).unwrap()
    }

    /// A socket file nobody listens on: what a crashed session leaves.
    fn dead(path: &Path) {
        drop(std::os::unix::net::UnixListener::bind(path).unwrap());
    }

    #[test]
    fn a_service_gets_the_sessions_sockets() {
        let rt = short_dir();
        let x11 = short_dir();
        std::fs::write(rt.path().join("bus"), "").unwrap();
        std::fs::write(rt.path().join("wayland-1.lock"), "").unwrap();
        let _w = serve(&rt.path().join("wayland-1"));
        std::fs::create_dir_all(rt.path().join("hypr/abc_123")).unwrap();
        let _h = serve(&rt.path().join("hypr/abc_123/.socket.sock"));
        let _x = serve(&x11.path().join("X0"));
        let none = |_: &str| false;
        let env = linux_env(Some(rt.path()), &none, x11.path());
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("XDG_RUNTIME_DIR").as_deref(), rt.path().to_str());
        assert_eq!(get("DBUS_SESSION_BUS_ADDRESS"), Some(format!("unix:path={}", rt.path().join("bus").display())));
        assert_eq!(get("WAYLAND_DISPLAY").as_deref(), Some("wayland-1"));
        assert_eq!(get("HYPRLAND_INSTANCE_SIGNATURE").as_deref(), Some("abc_123"));
        assert_eq!(get("DISPLAY").as_deref(), Some(":0"));
    }

    #[test]
    fn a_crashed_sessions_leftovers_are_skipped() {
        let rt = short_dir();
        let x11 = short_dir();
        dead(&rt.path().join("wayland-0"));
        let _w = serve(&rt.path().join("wayland-1"));
        for sig in ["aaa_old", "zzz_also_old"] {
            std::fs::create_dir_all(rt.path().join("hypr").join(sig)).unwrap();
            dead(&rt.path().join("hypr").join(sig).join(".socket.sock"));
        }
        std::fs::create_dir_all(rt.path().join("hypr/mmm_live")).unwrap();
        let _h = serve(&rt.path().join("hypr/mmm_live/.socket.sock"));
        std::fs::create_dir_all(rt.path().join("hypr/no_socket")).unwrap();
        dead(&x11.path().join("X0"));
        let none = |_: &str| false;
        let env = linux_env(Some(rt.path()), &none, x11.path());
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("WAYLAND_DISPLAY").as_deref(), Some("wayland-1"), "wayland-0 is dead");
        assert_eq!(get("HYPRLAND_INSTANCE_SIGNATURE").as_deref(), Some("mmm_live"), "the others are dead");
        assert_eq!(get("DISPLAY"), None, "X0 is dead, and a dead display is no better than none");
    }

    #[test]
    fn the_path_gets_what_exists_once() {
        let a = tempfile::tempdir().unwrap();
        let more = [a.path().to_path_buf(), PathBuf::from("/nonexistent/bin")];
        let path = path_with(Some(std::ffi::OsStr::new("/usr/bin")), &more).unwrap();
        assert_eq!(path, format!("/usr/bin:{}", a.path().display()));
        assert_eq!(path_with(Some(std::ffi::OsStr::new(&path)), &more), None, "nothing new, no change");
    }

    #[test]
    fn what_the_node_has_is_kept() {
        let rt = tempfile::tempdir().unwrap();
        std::fs::write(rt.path().join("bus"), "").unwrap();
        let all = |_: &str| true;
        assert!(linux_env(Some(rt.path()), &all, Path::new("/nonexistent")).is_empty());
        let none = |_: &str| false;
        assert!(linux_env(Some(Path::new("/nonexistent/rt")), &none, Path::new("/nonexistent")).is_empty());
    }
}
