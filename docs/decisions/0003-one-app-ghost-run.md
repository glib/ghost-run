# 0003 — One desktop app, Ghost Run, with resolve-ingest merged in

Status: accepted (2026-11-24)

## Context

The timing tool (Rust core, overlay render, auto-markers) has no UI. Hand-placed
markers and committing runs still need the Python script inside Resolve.
`glib/resolve-ingest` is a separate GTK4/libadwaita app for verified card
archives and PCM editing copies. The goal is one app covering the whole loop:
import footage, time runs, compare them, render overlays, and hand them to
Resolve.

## Decision

- **Name:** Ghost Run. You race your own best run, like a ghost lap. "Resolve"
  is Blackmagic's trademark, and the tool is about more than Resolve now.
  - App ID: `io.github.glib.GhostRun`. Binary: `ghost-run`. The repository is
    renamed `glib/ghost-run` (GitHub redirects the old URL).
  - `rtimer` stays the timing CLI for now.
- **One repository:** resolve-ingest is merged in with its history.
  - `crates/ingest` is the headless core.
  - `crates/app` is the GTK app, and the Import page is the old ingest window.
  - resolve-ingest is archived on GitHub. Its settings and journal moved to
    `ghost-run/import` (moved over from the old `resolve-ingest` folders on
    first start), so existing setups carry over.
- **Linux only.** It installs with `make install` to `~/.local`, plus a
  launcher and icon (through `distrobox enter` when built in a distrobox). The
  old Flatpak packaging was dropped: a sandboxed app could only reach Resolve's
  davincibox through `flatpak-spawn --host`, which defeats the sandbox.
- **Pages:**
  - Import.
  - Library: clips, course and marker state.
  - Runs: a leaderboard per course, best/optimal lap, batch edit of course,
    date and ignored.
  - Markers: a frame viewer, hand placement, and auto-markers with confidence.
  - Output: a render queue and Resolve.
- **Data:**
  - The YAML database (schema v2) stays the record of committed runs, written
    with a backup and an atomic replace.
  - A new SQLite file holds what isn't a run yet: library clips, suggested
    markers, course references, and job history.
- **Proxies:** one per clip, ProRes 422 Proxy at half resolution, with the same
  frame rate, frame count and timecode, in `<clip folder>/Proxy/<name>.mov`.
  - Resolve uses them as proxy media (it finds a `Proxy` folder; the bridge
    also links them).
  - The frame viewer scrubs them: intra-only frames seek instantly.
  - Auto-markers reads them instead of decoding 4K HEVC.
- **Resolve:**
  - A small Python bridge runs inside davincibox, reached with
    `distrobox enter`, exchanging JSON over stdin/stdout. It needs Resolve
    Studio, with external scripting set to Local.
  - "Add to open project" puts each overlay above its clip in the current
    timeline.
  - "New project" puts all selected runs back to back: clips on V1, overlays
    on V2, proxies linked, Resolve's default settings. A `.drp` template
    setting is kept for later.
- **One run per clip**, for now.

## Order of work

Each item is its own pull request:

1. Merge the repositories.
2. App shell, Library and proxies.
3. Runs page.
4. Marker editor.
5. Auto-markers in the UI.
6. Render queue.
7. Resolve bridge: add to open project.
8. New Resolve project.
9. Retire the Python Resolve script.

The user tries it on real footage after steps 4 and 7.

## Consequences

- CI installs GTK. The packages are cached, because apt is slow on hosted
  runners.
- While the Python script exists, the parity check stays. Step 9 removes both,
  along with the Windows install scripts.
