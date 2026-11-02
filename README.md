# desktop

A Pointman extension: [agent-desktop](https://github.com/vercel-labs/agent-desktop) on each machine, so agents can
read its apps through the OS accessibility tree and act on them: click, type, set values, press keys, scroll,
launch apps. An agent anywhere sends a command to any machine with `create_job(kind="desktop.run", machine=…)`.

It pairs with the [screen](https://github.com/pointman-ai/pointman-screen) extension. When a person wants to watch,
the agent opens screen's live pane on their device first. On macOS, every agent then gets its own cursor, labelled
with its handle and what it's doing, where it acts.

One Rust binary, `bin/desktop`, built for each platform with the
[pointman-extension](https://github.com/pointman-ai/pointman-extension) crate.

## agent-desktop itself

The extension doesn't use npm or Homebrew. It installs one pinned agent-desktop release into its state folder
(`agent-desktop/<version>/`): the executable with its platform helper beside it, from
`agent-desktop-v<version>-<target>.tar.gz` on agent-desktop's GitHub releases. Before unpacking, it checks the
archive against a sha256 that `src/install.rs` carries.

- **Where the hashes come from:** the release's own `checksums.txt`. Each archive's build provenance was checked
  first with `gh attestation verify <archive> --repo vercel-labs/agent-desktop`.
- **A new version** is one change to `src/install.rs`: `VERSION` and all four hashes.
- **On startup:** the extension installs it in the background, so the first call doesn't wait.

| Platform | Archive | Needs |
|---|---|---|
| macOS (arm64, x86_64) | `*-apple-darwin` | Accessibility for the node (System Settings › Privacy & Security › Accessibility), granted once at the machine |
| Linux (x86_64, arm64) | `*-unknown-linux-gnu` (glibc) | glibc's loader at `/lib64` (NixOS: `programs.nix-ld.enable`), and AT-SPI on in the desktop session (NixOS: `services.gnome.at-spi2-core.enable`, `QT_ACCESSIBILITY=1`, then log out and in) |

On Linux, the node may run as a systemd user unit. A call there gets the session's variables it lacks from
`/run/user/<uid>`: the session bus (agent-desktop finds the AT-SPI bus through it), the Wayland socket, Hyprland's
instance and X11's display. The call also gets NixOS's system and per-user `bin` folders on its PATH (for
`wl-copy`), and `QT_ACCESSIBILITY=1`. `NO_AT_BRIDGE` and `GTK_A11Y` are removed, so apps it launches join the bus.
Under Hyprland, these work: listing apps and windows, snapshots, `click`, `type`, `set-value`, and the clipboard.
`press`, `focus-window` and pointer input need GNOME's Mutter on Wayland. Screenshots ask for consent on screen, so
`screen.still` is the way there.

## Commands

| Kind | Params | What |
|---|---|---|
| `desktop.run` | `args`, `agent?`, `doing?`, `headed?`, `timeout?` | `agent-desktop <args>` on this machine; its JSON envelope back, with `text` (a line about it) and `machine` |
| `desktop.status` | | agent-desktop's version, platform and permissions here |

- **Errors are answers.** agent-desktop's `error` (`STALE_REF`, `APP_NOT_FOUND`…) comes back as the result, for the
  agent to branch on. The job only fails when agent-desktop can't run: not installed, crashed, timed out, or
  physical input asked for where it's off.
- **Screenshots are posted, not answered.** A `screenshot` without a path comes back as a PNG output, and the
  answer names it instead of carrying base64. A file it writes where it ran (`screenshot … out.png`) is posted too.
- **A huge answer goes back as a file.** Over 512 KB, the full JSON is posted as a document.
- **Deadlines:** each call has 60 s unless `timeout` says otherwise (at most 600). It can be cancelled, which kills
  the CLI. Output still arriving 0.5 s after the CLI exits is dropped: a process it left behind holding the pipe
  (`wl-copy`, owning a clipboard it set) isn't waited for.

## Sessions and cursors (macOS)

Every call shares one agent-desktop session on the machine, started with `--cursor --multi-agent --no-trace`. Each
call runs with `--agent-id <agent>`, so each agent gets a cursor of its own, labelled `@<agent>: <doing>`. The
session lasts a day, then the next call starts a new one and clears the old one's files. If the session ends under
a call (`session_ended`), the extension starts a new one and runs the call once more. A call that names its own
`--session` or `--agent-id` is left as it is. With the `cursor` setting off, calls run without a session.

agent-desktop keeps its state (snapshots' refs, sessions) in the extension's state folder (`home/`, mode 0700, as
it requires). The node's own `AGENT_DESKTOP_*` variables never reach it.

## Settings

| Setting | Default | |
|---|---|---|
| `headed` | false | "Physical input". On, it lets agents ask for physical input, which moves the pointer and types into the focused window. Each machine's person turns it on. Off: only accessibility actions, which leave the pointer and focus alone |
| `cursor` | true | On macOS, draws each agent's own cursor, with its name, where it acts |

## Health

| Part | OK when | Fix |
|---|---|---|
| `cli` | the pinned agent-desktop is installed | Update: install it now |
| `access` | `agent-desktop status` says accessibility is granted | Check. On macOS it asks the OS to prompt on the machine's screen (`permissions --request`): someone there allows the node, then restarts it |

## Development

`cargo test`. The tests run the extension under the SDK's stand-in node with a fake `agent-desktop`, a shell script
(`tests/common`) that logs each call and answers as the real one does. No test reaches GitHub or a real desktop,
except `cargo test -- --ignored`, which downloads the pinned release and runs its `version`. CI runs `cargo fmt`,
`clippy` and the tests on macOS and Ubuntu.

A release is a tag that's extension.toml's version: `.github/workflows/release.yml` builds the four platforms and
attaches them.
