# Ghost Run

Self lap timing for mountain-bike racing (downhill, enduro): film your runs,
time them by sector against your earlier runs, and watch them back with a split
timer burned into the video. And the footage side of making videos of them:
import, a catalog of everything filmed, and DaVinci Resolve projects opened
with their footage ready.

One Linux desktop app, **Ghost Run** (GTK 4 / libadwaita), plus the `rtimer`
command line. The plan and its history are in [`docs/decisions/`](docs/decisions/).

## What it does

- **Inbox**: put the camera card in and it's listed straight away, clips by day
  with thumbnails, the ones already on the NAS marked. **Import** copies each
  original to the NAS, checks it byte for byte, files it by recording day under
  the camera's own name, makes the catalog's preview, and says when the card is
  **safe to wipe**. Local copies go unless you keep them to edit now.
  **History and recovery** has earlier imports, retries and reports
  ([docs/ingest](docs/ingest/import.md)).
- **Library**: every clip (on the NAS, or on this machine) with its course, lap
  and place, NAS state, flags and tags; views for not timed yet, drafts to
  check, local copies, picks, collections, features, rides, and whether a clip
  is unused, in a video being made, or released. Tick clips for **Auto-place**,
  **Time it**, **Course…**, **Render overlays**, **Organise** (flag, tag,
  collect, add to a video) and **Resolve** (add to the open timeline, or a new
  video in Resolve). **Copy here** / **Remove local copy** hydrate and
  dehydrate NAS clips. Removing never takes footage a Resolve project saved
  in the last 60 days uses.
- **Time** (double-click a clip): step frame by frame (← →, Shift for ten, Page
  Up/Down for a second), put Start, the sector splits and Finish on the right
  frames (**M** sets the next one) and watch each sector against the course's
  best. **Auto-place** puts the markers in by matching the clip with the
  course's reference run; **Find the course** tries every course. Markers not
  committed are kept as drafts. **Commit run** saves to the run database.
- **Videos**: Ghost Run's videos (made from kinds such as trail, speed
  progression, ridelog, gear review, story, feature progression, or from
  picked clips), and every **Resolve timeline**, read from Resolve's project
  database, with how much of its footage is here. **Open in Resolve** copies
  what's on the NAS (asking first), starts Resolve if it isn't running and opens
  the project at that timeline. **Build in Resolve** makes a video's project
  (clips on V1, timer overlays on V2, captions on V3, cards). **Make it a
  video** gives a timeline a status and a YouTube link, and its clips then
  count as used. **Bring back footage…** relinks an old project open in
  Resolve.
- **Tracks**: the courses you've ridden most lately first, each with your PB,
  latest run, a target, a trend chart, the sector you're losing most in, and its
  leaderboard (best and optimal lap). Tick runs to move, ignore, delete, render
  or send them to Resolve.
- **Jobs** (foot of the sidebar): overlay renders, transparent ProRes 4444
  `.mov`s with the clip's timecode, optionally with a live ghost gap; and Resolve
  sends.
- **Settings** (the gear): footage folders, the run database, Import's
  locations, previews, Resolve, the shared catalog, Strava, brand kit, video
  kinds folder and updates.

Resolve needs **Resolve Studio** with Preferences > System > General >
External scripting using set to **Local**. Ghost Run reaches it in the
`davincibox` Distrobox (another name can be set in Settings), or directly if
it's installed on the system.

### Shared catalog and Resolve's projects

With **Settings > Shared catalog**, every machine uses one catalog in
Postgres; away from the server, each opens its last copy read-only. Set it up
once on the server (resolve-db, which also holds Resolve's project library):

```bash
pct push 106 scripts/catalog-db-setup.sh /root/catalog-db-setup.sh
pct exec 106 -- sh /root/catalog-db-setup.sh            # --resolve: only the Resolve part
```

It makes the `ghost_run` role and database, and lets that role **read**
Resolve's project databases, which is how the Videos page lists Resolve's
projects without Resolve running. It prints what to type into Settings, and
whether the tables Ghost Run reads are there.

## Install (Linux)

Needs Rust, GTK 4.12+, libadwaita 1.5+ and FFmpeg. On Bazzite, build it in a
Fedora distrobox:

```bash
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel   # Debian/Ubuntu: libgtk-4-dev libadwaita-1-dev
make install
```

This installs `ghost-run` and `rtimer` in `~/.local/bin`, and a **Ghost Run**
launcher with its icon (started through `distrobox enter <box>` when built in a
box). After that Ghost Run updates itself from this repo's releases (Settings →
Updates; `docs/releasing.md`). `make uninstall` removes it. `ghost-run --help`
lists the headless import commands.

## Markers and timing

For a course with `sector_count: N`, a run has the markers `Start`, `S1` …
`S(N-1)` and `Finish`. Times come from the source clip's frame rate:

```text
seconds = frame_delta / source_fps
```

Runs keep their `capture_time`, so each is compared with runs captured before
it. DJI file names (`YYYYMMDDHHMMSS`) give the time; otherwise it's the clip's
recorded time as the Library knows it, else the run's date.

The run database is `timer_db.yaml` (schema version 2; `examples/timer_db.yaml`
is a starter). The app saves it only if it hasn't changed since loading, with a
backup in `timer_db.yaml.backups/`.

## `rtimer`

```bash
rtimer --db timer_db.yaml courses
rtimer --db timer_db.yaml runs --course lower_whistler_a_line
rtimer --db timer_db.yaml stats --course lower_whistler_a_line [--json]
rtimer --db timer_db.yaml validate-db
rtimer --db timer_db.yaml normalize-db
rtimer --db timer_db.yaml overlay-text --course ID --markers markers.csv --filename GX010123.MP4 --fps 59.94 --mode best_lap
rtimer --db timer_db.yaml render --course ID --markers markers.csv --filename GX010123.MP4 --fps 59.94 --source GX010123.MP4 --out GX010123_timer.mov
rtimer --db timer_db.yaml auto-markers --reference-run RUN_ID --clip GX010150.MP4 --out GX010150_markers.csv
```

Marker CSVs have `name,frame` columns (`auto-markers` adds `confidence`).

- `render` writes the overlay as one transparent ProRes 4444 `.mov` with the
  clip's timecode, from a second before Start (`--pre-roll`) to five after
  Finish (`--hold`), so **Auto Align Clips → Based on Timecode** lines it up.
  `--out overlay_%05d.png` writes a PNG sequence; `--still FRAME` one frame.
  At 4K it's about 1.2 GB a minute, split across ffmpeg processes planned
  from your cores and half the free memory (`--jobs`, `--threads` override).
- `auto-markers` lines a clip up with a run whose markers are placed (64×36
  greyscale frames, cached; up to 3× speed difference) and writes each marker
  with a confidence; low ones are marked to check.

## Development

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
python -m unittest discover -s tests   # the Resolve bridge and scripts (stdlib only)
```

The workspace: `crates/app` (the app, binary `ghost-run`), `crates/library`
(the catalog, footage, Resolve's database), `crates/ingest` (import),
`crates/timer-core` (timing and the YAML database), `crates/timer-render`
(overlay video), `crates/timer-align` (auto-markers), `crates/timer-cli`
(`rtimer`). Media tests need FFmpeg; shared-catalog tests need Postgres (see
CLAUDE.md). `scripts/golden.sh` holds `rtimer`'s outputs to
`tests/fixtures/golden.txt`, recorded when they matched the retired Python
tool byte for byte.
