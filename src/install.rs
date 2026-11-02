//! agent-desktop on a machine: one pinned release of github.com/vercel-labs/agent-desktop for this platform
//! (`agent-desktop-v<VERSION>-<target>.tar.gz`), checked against the sha256 this file carries, and only its
//! executable and its platform helper taken out, side by side, into `<state>/agent-desktop/<VERSION>/`. No npm, no
//! Homebrew, no sudo, nothing outside that folder.
//!
//! The hashes are the release's own `checksums.txt`, each archive's build provenance checked first with
//! `gh attestation verify <archive> --repo vercel-labs/agent-desktop` (built by its release.yml). A new version is a
//! change to this file: VERSION and all four hashes together.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use sha2::{Digest, Sha256};

pub const VERSION: &str = "1.0.2";

/// Each platform's archive and its sha256, from the v1.0.2 release.
const BUILDS: [(&str, &str); 4] = [
    ("aarch64-apple-darwin", "7bee385196d7b478de80cf05ca4304aaa38f0eba593b3406ad3c247c1773be19"),
    ("x86_64-apple-darwin", "8edda6df17eafc042395502a823dbb8415bc8ac14835a6731e4d8becc9cb4121"),
    ("x86_64-unknown-linux-gnu", "ad661b7a3fd8f990a769243f4889a240ca0e2c8dab3ef5bddd6019086c71e0b4"),
    ("aarch64-unknown-linux-gnu", "864cb782e0afc8bf0df19622016b24bdbd1a83fc7749f784ee82aa49a7f845eb"),
];

pub const RELEASES: &str = "https://github.com/vercel-labs/agent-desktop/releases/download";

/// The files an archive may give: the CLI and its helper, which must sit beside it.
const FILES: [&str; 3] = ["agent-desktop", "agent-desktop-macos-helper", "agent-desktop-linux-clipboard-owner"];

/// Its archives are 2 to 4 MB; anything much bigger isn't one.
const ARCHIVE_MAX: u64 = 64 * 1024 * 1024;
const FILE_MAX: u64 = 128 * 1024 * 1024;

/// This platform's build, as agent-desktop's releases name it.
pub fn target() -> Option<&'static str> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// The sha256 this extension trusts for `target`'s archive.
pub fn sha256_for(target: &str) -> Option<&'static str> {
    BUILDS.iter().find(|(t, _)| *t == target).map(|(_, sum)| *sum)
}

pub fn archive_name(target: &str) -> String {
    format!("agent-desktop-v{VERSION}-{target}.tar.gz")
}

/// The pinned CLI, if it's installed: `<state>/agent-desktop/<VERSION>/agent-desktop`.
pub fn installed(state: &Path) -> Option<PathBuf> {
    let cli = state.join("agent-desktop").join(VERSION).join("agent-desktop");
    cli.is_file().then_some(cli)
}

fn download(url: &str, max: u64) -> anyhow::Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("pointman-desktop/", env!("CARGO_PKG_VERSION")))
        .build();
    let r = agent.get(url).call().with_context(|| format!("downloading {url}"))?;
    let mut out = Vec::new();
    r.into_reader().take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        bail!("{url} is bigger than it should be");
    }
    Ok(out)
}

/// Download this platform's pinned archive from `releases` (agent-desktop's releases, or a test's) and install it.
pub fn install(state: &Path, releases: &str) -> anyhow::Result<PathBuf> {
    let target = target().context("agent-desktop has no build for this platform")?;
    let want = sha256_for(target).context("no pinned sha256 for this platform")?;
    let name = archive_name(target);
    let archive = download(&format!("{}/v{VERSION}/{name}", releases.trim_end_matches('/')), ARCHIVE_MAX)?;
    install_archive(state, &archive, &name, want)
}

/// Check `archive` against `want`, then put its files in `<state>/agent-desktop/<VERSION>/`, all at once (a
/// half-unpacked folder never has the final name), and remove other versions'.
pub fn install_archive(state: &Path, archive: &[u8], name: &str, want: &str) -> anyhow::Result<PathBuf> {
    let got = format!("{:x}", Sha256::digest(archive));
    if !got.eq_ignore_ascii_case(want) {
        bail!("{name} doesn't match the sha256 this extension pins for agent-desktop {VERSION}: not installed");
    }
    let files = unpack(archive).with_context(|| format!("unpacking {name}"))?;
    if !files.iter().any(|(n, _)| n == "agent-desktop") {
        bail!("{name} has no agent-desktop");
    }
    let root = state.join("agent-desktop");
    fs::create_dir_all(&root)?;
    let part = root.join(format!(".{VERSION}.part"));
    let _ = fs::remove_dir_all(&part);
    fs::create_dir_all(&part)?;
    for (n, bytes) in &files {
        let path = part.join(n);
        fs::write(&path, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
    }
    let to = root.join(VERSION);
    let _ = fs::remove_dir_all(&to);
    fs::rename(&part, &to)?;
    // older versions' folders: a command host still running from one keeps its open file
    for entry in fs::read_dir(&root)?.flatten() {
        if entry.file_name() != VERSION && entry.path().is_dir() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(to.join("agent-desktop"))
}

/// The archive's top-level files that are agent-desktop's, and nothing else of it.
fn unpack(archive: &[u8]) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut out = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let parts: Vec<_> = path.components().filter(|c| !matches!(c, std::path::Component::CurDir)).collect();
        let [only] = parts.as_slice() else { continue };
        let n = only.as_os_str().to_string_lossy().to_string();
        if !FILES.contains(&n.as_str()) {
            continue;
        }
        if entry.size() > FILE_MAX {
            bail!("its {n} is bigger than it should be");
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        out.push((n, bytes));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, bytes) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, *bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn sum(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn every_platform_has_a_pinned_build() {
        for (os, arch) in [("macos", "aarch64"), ("macos", "x86_64"), ("linux", "x86_64"), ("linux", "aarch64")] {
            let t = target_for(os, arch).unwrap();
            let s = sha256_for(t).unwrap();
            assert!(s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()), "{t}: {s}");
        }
        assert_eq!(target_for("windows", "x86_64"), None);
        assert!(target().is_some());
        assert_eq!(archive_name("x86_64-unknown-linux-gnu"), "agent-desktop-v1.0.2-x86_64-unknown-linux-gnu.tar.gz");
    }

    #[test]
    fn installs_the_cli_and_its_helper_side_by_side() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join("agent-desktop/0.9.0")).unwrap();
        let a = archive(&[
            ("agent-desktop", b"#!/bin/sh\n"),
            ("./agent-desktop-macos-helper", b"helper"),
            ("README.md", b"not ours"),
            ("nested/agent-desktop", b"not at the top"),
        ]);
        let cli = install_archive(state.path(), &a, "a.tar.gz", &sum(&a)).unwrap();
        assert_eq!(cli, state.path().join("agent-desktop/1.0.2/agent-desktop"));
        assert_eq!(std::fs::read(&cli).unwrap(), b"#!/bin/sh\n");
        assert_eq!(std::fs::read(cli.with_file_name("agent-desktop-macos-helper")).unwrap(), b"helper");
        assert!(!cli.with_file_name("README.md").exists());
        assert!(!state.path().join("agent-desktop/0.9.0").exists(), "an older version is removed");
        assert_eq!(installed(state.path()), Some(cli.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&cli).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn a_wrong_hash_installs_nothing() {
        let state = tempfile::tempdir().unwrap();
        let a = archive(&[("agent-desktop", b"x")]);
        let e = install_archive(state.path(), &a, "a.tar.gz", &"0".repeat(64)).unwrap_err().to_string();
        assert!(e.contains("doesn't match the sha256"), "{e}");
        assert_eq!(installed(state.path()), None);
        assert!(!state.path().join("agent-desktop").exists());
    }

    #[test]
    fn an_archive_without_the_cli_installs_nothing() {
        let state = tempfile::tempdir().unwrap();
        let a = archive(&[("agent-desktop-macos-helper", b"x")]);
        let e = install_archive(state.path(), &a, "a.tar.gz", &sum(&a)).unwrap_err().to_string();
        assert!(e.contains("has no agent-desktop"), "{e}");
        assert_eq!(installed(state.path()), None);
    }

    /// The real download from agent-desktop's releases: `cargo test -- --ignored` (online).
    #[test]
    #[ignore]
    fn installs_the_pinned_release() {
        let state = tempfile::tempdir().unwrap();
        let cli = install(state.path(), RELEASES).unwrap();
        let out = std::process::Command::new(&cli).arg("version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains(VERSION), "{out:?}");
    }
}
