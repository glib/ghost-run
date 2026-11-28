# Ghost Run

Self lap timing for mountain-bike racing (downhill, enduro): film your runs,
time them by sector against your earlier runs, and watch them back with a split
timer burned into the video.

It is becoming one Linux desktop app, **Ghost Run**: import footage, place
Start/S1…/Finish markers (automatically, from a reference run), compare runs per
track, render overlays and hand them to DaVinci Resolve. Today the app has:

- **Library**: every clip in your footage folders (Folders… to add them) and
  Import's editing folder, when it was recorded, which stored run it is, and
  whether its original is verified on the NAS (**Not on the NAS** lists the
  ones that aren't). Once a day it also copies its catalog and the run
  database to the NAS (`.ghost-run/backups/` in Import's archive folder,
  named after the machine, the last 30 days of each kept) (settings are under
  the gear in the sidebar). With **Settings > Shared catalog** the desktop and
  the laptop use one catalog in Postgres (a `ghost_run` database made by
  `scripts/catalog-db-setup.sh` on the server); away from it, each opens its
  last copy read-only. **Make missing proxies** writes a
  half-size ProRes Proxy copy of each clip into a `Proxy` folder beside it, which
  Resolve picks up as proxy media and the Time page steps through. Double-click
  a clip (or tick it and press **Time it**) to time it.
- **Time**: step through a clip frame by frame (← →, Shift for ten, Page Up/Down
  for a second), put Start, the sector splits and Finish on the right frames
  (**M** sets the next one), and watch each sector and the lap against the
  course's best as you go. **Commit run** saves it to the run database the same
  way the Resolve script does (same run ids and fields). Markers not yet
  committed are kept as a draft, so closing the app loses nothing.
  **Auto-place** puts the markers in for you by matching the clip's footage with
  the course's reference run (the best lap with footage, or one you pick with
  **Use this run as the reference**); **Find the course** tries every course.
  Markers it is less sure of are highlighted to check by eye. In the Library,
  **Auto-place ticked** does this for many clips at once, as drafts to check.
- **Library**: tick clips and a bar appears over the table with what you can
  do with them: **Auto-place**, **Time it**, move their runs to another
  **Course…**, **Render overlays**, or **Resolve** (add to the open timeline,
  or a new project). Rendering and Resolve use the ticked clips' runs, so a
  clip has to be timed first. Courses are added and renamed in the Courses
  panel.
- **Tracks**: your courses, the ones you've ridden most in the last eight
  weeks first. For each: your personal best and how long it has stood, your
  latest run against it, a target time to chase (**Target…**), a chart of
  every counted run with your PB as a step line, and the sector where you've
  lost most time lately against your fastest. Below it, the course's
  leaderboard (best lap, optimal lap, sector times with the fastest
  highlighted). Tick runs to move them to another course, ignore them, delete
  them, render their overlays or send them to Resolve. Each change is saved at
  once, with a backup in `timer_db.yaml.backups/`, and never over changes the
  Resolve script made meanwhile.
- **Output**: tick clips in the Library (or runs on Tracks) and press **Render overlays** (or
  **Add runs without overlays** here) to render each run's timer overlay: a
  transparent ProRes 4444 `.mov` with the clip's timecode, in an `Overlays`
  folder beside the clip (or the folder set in Settings). It compares each
  sector with the best lap so far, or the optimal lap so far. Renders run one at
  a time on all the cores the machine's memory allows; Stop leaves no half-written
  file. **Add to open timeline** puts each finished overlay on the track above
  its clip in DaVinci Resolve's current timeline, lined up frame for frame
  (trimmed to the part of the clip that's on the timeline; never twice). This
  needs Resolve Studio running with Preferences > System > General > External
  scripting using set to **Local**; Ghost Run reaches it in the `davincibox`
  Distrobox (another name can be set in Settings), or directly if Resolve is
  installed on the system. **Check** shows whether Resolve answers. **New
  project…** starts a Resolve project with the finished runs back to back, in
  the order ridden: each clip (with its audio) from a second before Start to the
  end of its overlay on V1, the overlay above it on V2, and the clip's proxy
  linked, at the first clip's frame rate and size. (A project template can be
  chosen in Settings for later; it isn't used yet.)
- **Import**: put the camera card in and it's listed straight away (every
  time, whatever path it mounts at), clips grouped by day with thumbnails,
  and clips already on the NAS marked and left out. **Import** copies each
  original to the NAS, checks it byte for byte, makes an editing copy, files
  both by recording day under the camera's own names, and ends with a clear
  **you can wipe the card** (or why not). Where they go is set in Settings →
  Import. **History and recovery** has earlier imports, retries, cleanup,
  reports and converting files already on disk (formerly
  [resolve-ingest](docs/ingest/import.md)).

The rest is being built
(`docs/decisions/0003-one-app-ghost-run.md`). Until then, timing lives in the
older Resolve script described below, and the `rtimer` command line.

## Install the app (Linux)

Needs Rust, GTK 4.12+, libadwaita 1.5+ and FFmpeg. On Bazzite, build it in a
Fedora distrobox:

```bash
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel   # Debian/Ubuntu: libgtk-4-dev libadwaita-1-dev
make install
```

This installs `ghost-run` and `rtimer` in `~/.local/bin`, and a **Ghost Run**
launcher with its icon in your app menu. Built inside a distrobox, the launcher
starts it through `distrobox enter <box>`, since it uses the box's GTK.
Installing also refreshes the desktop's menu cache (on KDE, `kbuildsycoca6`,
run on the host from inside a distrobox). If the menu entry still has no icon,
run `kbuildsycoca6` on the host yourself, or log out and back in.
`make uninstall` removes them. With no arguments `ghost-run` opens the app;
`ghost-run --help` lists the headless import commands.

## The Resolve script (older tool)

The Python script runs inside DaVinci Resolve Studio: it reads markers from a
Media Pool clip and builds Fusion/Text+ overlays. It keeps working until the app
covers the workflow, then it will be removed. Resolve-specific code sits behind
small adapter, UI, and overlay modules; the timing, database, matching, and CLI
code can be tested outside Resolve with the standard Python test runner.

### What It Does

- Reads timing markers from one selected Media Pool clip.
- Previews sector and lap times for a selected course.
- Commits, updates, ignores, unignores, and deletes runs in a YAML database.
- Compares the current run against prior-captured runs for best lap or optimal lap.
- Creates or updates a Fusion/Text+ overlay on the timeline clip under the
  playhead, or across all valid video clips in the current timeline.

## Marker Rules

Markers are read from the source media, not from the timeline. Timeline
selection and playhead position do not affect the Media Pool timing preview.

For a course with `sector_count: N`, add these source clip markers:

- `Start`
- `S1` through `S(N-1)`
- `Finish`

Timing uses the source clip frame rate:

```text
seconds = frame_delta / source_fps
```

## Install For Resolve

### Bazzite / davincibox

From this checkout in a host terminal (or a development Distrobox), run:

```bash
bash scripts/install-resolve-linux.sh --dry-run
bash scripts/install-resolve-linux.sh
```

The installer defaults to the `davincibox` container. Use `--container NAME` for
a different name and `--python /path/to/python` if Resolve uses a different
Python interpreter. Python 3.10+ with `venv` and `ensurepip` is required inside
the container. The checkout must be accessible there at the same absolute path.

Dependencies are downloaded into a temporary environment using the container's
Python, checked, and installed into `.resolve_deps`. No host system packages are
installed. The launcher is written under the container user's
`~/.local/share/DaVinciResolve/Fusion/Scripts/Utility/Resolve Timer` directory.
Repeat installs preserve your database and preferences; rerun after changing
the container's Python version or moving the checkout. Keep the checkout in place.
Avoid running the installer while the timer window is open.

Restart Resolve and select `Resolve Timer` under `Workspace > Scripts`.
Check `resolve_timer_startup.json` and `resolve_timer.log` in this checkout if
startup fails. The Python version in startup diagnostics should match the
installer's output. Installation checks imports and database loading; opening
the timer and previewing a marked clip in Resolve verifies the UI integration.

### Windows

Run these from the project root in PowerShell:

```powershell
.\scripts\Install-ResolveDependencies.ps1
.\scripts\Install-ResolveScripts.ps1
```

Then restart Resolve and run `Resolve Timer` from `Workspace > Scripts`.

The installer creates lightweight Resolve launchers that point back to this
checkout, so code changes are picked up without reinstalling. On first install
it also creates `timer_db.yaml` from `examples/timer_db.yaml`; later installs do
not overwrite your database.

## Using The Resolve Tool

1. Select exactly one marked clip in the Media Pool.
2. Choose a course in Resolve Timer.
3. Use `Refresh Preview` to read markers and calculate timing.
4. Commit a new run, update the matching run, or manage existing runs.
5. Put the timeline playhead over a matching video clip and use `Update Clip
   Under Playhead` to write the overlay.
6. Use `Update All Timeline Clips` to update every valid video clip in the
   current timeline.

The overlay starts at the source `Start` marker, advances using Fusion item
time, and freezes at `Finish`. Sector rows reveal at their marker crossings, and
the lap row reveals at `Finish`.

## Project Files

- `timer_db.yaml`: your local course and run database.
- `resolve_timer_preferences.json`: last selected course, comparison mode, and summary output folder.
- `resolve_timer_startup.json`: latest Resolve startup diagnostics.
- `resolve_timer.log`: unexpected startup or controller tracebacks.
- `examples/timer_db.yaml`: starter database template.

Local runtime files are ignored by Git.

## CLI

Install the package in a Python environment, then run:

```powershell
resolve-timer --db examples/timer_db.yaml courses
resolve-timer --db examples/timer_db.yaml validate-db
resolve-timer --db examples/timer_db.yaml normalize-db
resolve-timer --db timer_db.yaml backfill-capture-times --dry-run
resolve-timer --db timer_db.yaml backfill-capture-times --media-root D:\Media\Runs --dry-run
resolve-timer --db examples/timer_db.yaml preview --course lower_whistler_a_line --markers examples/markers.csv --filename GX010123.MP4 --fps 59.94
resolve-timer --db examples/timer_db.yaml commit --course lower_whistler_a_line --markers examples/markers.csv --filename GX010123.MP4 --fps 59.94
resolve-timer --db examples/timer_db.yaml runs --course lower_whistler_a_line
resolve-timer --db examples/timer_db.yaml stats --course lower_whistler_a_line
resolve-timer --db examples/timer_db.yaml overlay-text --course lower_whistler_a_line --markers examples/markers.csv --filename GX010123.MP4 --fps 59.94 --mode best_lap
```

Marker CSV files need `name,frame` columns.

## Placing markers from a reference run (Rust `rtimer`, in progress)

`rtimer auto-markers` places a new clip's markers for you. It lines the new
clip's footage up with a run of the same course whose markers are already
placed, and puts each marker where the new clip passes the same spot. Needs
ffmpeg on `PATH`.

```powershell
target\release\rtimer --db timer_db.yaml auto-markers --reference-run run_2026_05_31_001 --clip D:\Media\Runs\GX010150.MP4 --out GX010150_markers.csv
```

- The reference is a stored run (its `source_path` is its clip, or give
  `--reference-clip`), or `--reference-markers CSV --reference-clip CLIP --course ID`.
- It writes a marker CSV (`name,frame,confidence`) that `overlay-text` and
  `render` read, and prints how well the clip matched, each marker's
  confidence (low ones are marked `<- check`), and the sector times against
  the reference run.
- The new run can be at a different speed (up to 3× either way at any point),
  frame rate or exposure, and anywhere in the clip.
- Both clips are read at 64×36 greyscale; that's the slow part for long 4K
  clips. Decoded clips are cached (in your cache folder, or `--cache-dir`), so a
  reference clip is only read once. ffmpeg uses a GPU decoder when it finds one.
- Use a reference ridden on a similar line with the camera in the same place.
  The match is on what the camera sees, so a very different line, or a
  different camera mount, matches less well.

## Overlay as a video file (Rust `rtimer`, in progress)

`rtimer render` writes the overlay as one transparent video to lay over the clip,
instead of a Fusion comp, so the timeline stays light. It takes the same
arguments as `overlay-text`, and needs [ffmpeg](https://ffmpeg.org/) on `PATH`:

```powershell
cargo build --release -p timer-cli
target\release\rtimer --db timer_db.yaml render --course lower_whistler_a_line --markers markers.csv --filename GX010123.MP4 --fps 59.94 --source D:\Media\Runs\GX010123.MP4 --out GX010123_timer.mov
```

- The `.mov` is ProRes 4444 with alpha, the clip's size and frame rate, from one
  second before `Start` (`--pre-roll`) to five seconds after `Finish` (`--hold`).
  It carries the clip's timecode, so selecting it and the clip and using
  **Auto Align Clips → Based on Timecode** lines it up, however the clip is trimmed.
- `--fps` must be the clip's frame rate (it turns marker frames into seconds);
  with `--source`, a different rate is an error that names the right one.
- Without `--source`, it's 3840×2160 (`--size`) at `--fps`, and the command
  prints the clip frame it starts at.
- `--out overlay_%05d.png` writes a PNG sequence instead (about a sixth of the
  size), numbered by clip frame. `--still FRAME --out frame.png` writes one frame
  to check the look.
- It looks like the Fusion overlay, except the panel can't blur the video behind
  it, so it's a plain translucent panel.
- ProRes 4444 at 4K is large: about 1.2 GB a minute. The render is split
  across a few ffmpeg processes (one ffmpeg feeds only about 8 encoder threads
  at 4K) and joined without re-encoding, planned from your cores and to fit in
  half the free memory (about 660 MB per process and 230 MB per thread at 4K).
  `--jobs N` and `--threads N` (per process) override the plan; the last line
  printed says what was used.

Runs store `capture_time` so comparisons are chronological by source clip capture
time. DJI filenames containing `YYYYMMDDHHMMSS` are the primary source because
their timestamp survives copying. Other filenames fall back to filesystem
creation time when the source file is available. Use `backfill-capture-times`
to correct existing DJI records; add `--media-root` to also resolve other names.

## Development

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test   # Rust: the app, ingest, timing, render
python -m unittest discover -s tests                                        # Python (needs PyYAML, Pillow)
```

The Rust workspace: `crates/app` (the GTK app, binary `ghost-run`),
`crates/ingest` (import), `crates/timer-core` (timing and the YAML database),
`crates/timer-render` (overlay video), `crates/timer-align` (auto-markers),
`crates/timer-cli` (`rtimer`). The render, auto-markers and ingest media tests
need FFmpeg.
