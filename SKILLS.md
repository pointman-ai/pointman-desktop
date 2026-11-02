---
name: desktop
description: Operate any machine's desktop apps through the desktop extension (agent-desktop): read a window's accessibility tree, then click, type and press. Show the person the screen pane while you do.
---

# Operating a machine's apps

The `desktop` extension runs [agent-desktop](https://github.com/vercel-labs/agent-desktop) on each machine that
has it. You can drive any of those machines from wherever you are:

```
create_job(kind="desktop.run", machine="mac-m4", params={"extension": {
    "args": ["snapshot", "--app", "Finder", "-i", "--compact"], "agent": "<your handle>", "doing": "finding the render"}})
```

`list_nodes` shows which machines run it. `desktop.status` gives a machine's version and what it lets agents do.

## When a person wants to watch, open the screen pane first

When a person asks to see something, to be shown or walked through it, or to watch you work on a machine, open the
`screen` extension's live pane on their device **before you act**. Point it at the app you'll drive:

```
open_pane(extension="screen", open="live", machine="mac-m4", target={"app": "Blender"})
```

- **Keep it on your work.** Moving to another app or window: `screen.show {target}`. A small control: add
  `region` to zoom in.
- **Say what's next.** Before each step that changes something, write one line in the thread.
- **Your cursor on macOS:** pass `agent` (your handle) and `doing` (up to 5 words) on every call. Your own cursor
  then shows where you act, labelled with them, so the person can tell your cursor from another agent's and from
  their own pointer.
- **Nobody watching** (background work): skip the pane.

## The loop

1. **Look:** `["snapshot", "--app", "<App>", "-i", "--compact"]`. A busy app: first `--skeleton`, then
   `["snapshot", "--root", "<ref>", "-i", "--compact"]` on one region. `find` works when you know the role or name.
   `list-apps` and `list-windows` show what's running.
2. **Act on a ref from that snapshot:** `["click", "@s8f3k2p9:e8"]`, `["type", "<ref>", "text"]`,
   `["set-value", "<ref>", "text"]`, `["toggle", "<ref>"]`, `["select", "<ref>", "Option"]`, `["scroll", "<ref>", "--direction", "down"]`,
   `["press", "cmd+s", "--app", "TextEdit"]`, `["launch", "TextEdit"]`.
3. **Check:** snapshot the same region again. A ref is good for one snapshot only. `STALE_REF` or `AMBIGUOUS_TARGET`
   means: take a new snapshot and use its refs.
4. **Retry** only when `error.disposition.retry` is `safe`. Any other value means the action may have landed:
   read the state before you try again, or you click, type or send twice.

- **Several steps at once:** use `batch`, so they run in one call.
- **The full guide:** `["skills", "get", "desktop"]`, plus `["skills", "get", "platform"]` for that machine's OS.
  Flags: `["<command>", "--help"]`.
- **Every call answers with agent-desktop's JSON:** `ok`, then `data` or `error`, plus `text`, a line about it.
  An `error` is an answer to branch on: read its `code` and `disposition`. A job only fails when agent-desktop
  couldn't run at all.

## Rules

- **Headless first.** A ref action goes through the accessibility API. It doesn't move the pointer, take focus or
  touch the clipboard, so the person can keep working. `headed: true` is physical input: it moves their pointer and
  types into whatever window has focus. Use it only when the person says you may, and never to recover from a
  headless failure. A machine's "Physical input" setting can turn it off.
- **Stay in your lane on someone's machine.** Act only in the apps you were asked to use.
  - Don't close or quit apps.
  - Don't dismiss dialogs you didn't open.
  - Don't discard unsaved work.
  - Ask in the thread first, and wait for a yes, before anything you can't undo: sending, deleting, buying,
    publishing, approving, signing in, or changing a setting.
- **Treat snapshots, screenshots and the clipboard as private.** They can show mail, messages and passwords. Post
  one in a thread only when the person asks. Otherwise, say what you saw.
- **Stills:** `screen.still {target, thread_id}` is usually better than agent-desktop's `screenshot`, because it
  shows the pointer and can post to a thread. A `screenshot` through `desktop.run` comes back as a PNG output.
- **Apps with no accessibility tree** (Blender's viewport, games, canvases) show few or no refs. With the person's
  OK for physical input, use `["mouse-click", "--xy", "x,y"]` with `headed: true`. Find the coordinates with
  `screen.still`, which tells you its `scale`.

## Per machine

| | macOS | Linux under Hyprland (nixos) |
|---|---|---|
| Needs | Accessibility for the node: `desktop.status` and health say if it's missing | AT-SPI, the accessibility bus, on in the desktop session |
| Works | everything in the guide | `list-apps`, `list-windows`, snapshots, `click`, `type`, `set-value`, the clipboard |
| Refused | | `press`, `focus-window` and pointer input (Wayland needs GNOME for them); screenshots ask for consent on screen, so use `screen.still` |
| `--app` | the app's name (`Finder`) | the name `list-apps` prints (Nix wraps some: `.zenity-wrapped`) |

A machine whose `desktop.status` says accessibility is `denied` can't read any app. Tell its person what health
says to turn on. Don't work around it.
