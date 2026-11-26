# 0005 — Workspaces for each job, footage kept on the NAS and hydrated to edit

Status: accepted (2026-11-26)

## Context

After 0003 and 0004 the app has the right pieces: a verified import, a
library with proxies, timing and auto-place, leaderboards, an overlay render
queue and a Resolve bridge. But it is laid out by tool (Import, Output) rather
than by the jobs Liam does, and the steps don't connect:

- After an import, the editing folder has to be added to the Library by hand,
  rescanned, and given proxies.
- Imports land in `DATE/UUID/clips/` (a UTC date) with every capital letter
  escaped (`f-~44~4a~49_…`), which is what the Library and Resolve then show.
- The Library can't tell whether a clip is safely archived, though the import
  journal knows.
- There is one kind of video (0004's edit), no way to say what footage a kind
  of video needs, and no record of which footage went into which video.
- Leaderboards are a table per course, with no trend over time.

The jobs, as Liam describes them:

1. Bring new footage in, backed up, with proxies.
2. Make videos of several kinds from footage already filmed.
3. Overlays where a video needs them.
4. Keep improving on trails ridden a lot (timing them breaks up the routine).
5. See which footage hasn't been used, which is in a project not yet
   published, and which has been released.

Similar tools borrowed from: Lightroom and darktable (one catalog, a module per
job, flags and smart collections), Hedge/OffShoot (verified offload, "safe to
wipe"), Final Cut Pro's used-media ranges and Resolve's Usage smart bins
(unused footage), RaceRender and VBOX Circuit Tools (lap overlays, side-by-side
runs), GoPro Quik (templated edits), Strava segments (effort history).

## Decision

### Workspaces

The sidebar becomes the jobs, over one catalog:

- **Inbox** — new footage: card to NAS, verified, then everything that can be
  done without the full-size files (course found, markers auto-placed as
  drafts), ending on what needs checking. The old Import page's other modes
  stay reachable from here.
- **Library** — browse everything by ride, course, feature, collection or tag,
  with thumbnails, flags and usage.
- **Videos** — a video is a kind (template) plus its input; the app suggests
  footage, unused first, hydrates it and builds it in Resolve.
- **Tracks** — the leaderboards, led by the courses ridden most.

Renders, proxies, hydration and Resolve sends are background **jobs** in a
drawer any page can open; the Output page goes. Settings stay one dialog, and
the Import settings join it.

### Footage lives on the NAS; the machine keeps a catalog

Three tiers:

| Tier | What | Where |
|---|---|---|
| Original | camera files, verified on copy | the NAS footage root (a folder on the Synology), the only home |
| Catalog | library database, thumbnails and filmstrips, a ~540p preview for timing, auto-place's 64×36 frames, 16 kHz mono audio for suggestions | always local, ~2–3% of the originals |
| Hydrated | editing copy (original video, PCM audio, which Resolve on Linux needs) and ProRes proxy | local, only while a video uses it |

- **Import** copies card → NAS and verifies it. The catalog files are made in
  the same pass, from the card, so the card is **safe to wipe** as soon as the
  Inbox says so. No editing copy is made at import any more.
- Timing, auto-place, finding the course, leaderboards, audio suggestions and
  overlay renders all work on the catalog alone: none of them need the
  full-size file (overlays need only the clip's size, rate and timecode).
- **Hydrate** is per video: pulling only that video's clips from the NAS. A
  clip always hydrates to the same local path (its NAS path under the local
  footage folder, `Proxy/` beside it), so a Resolve project relinks by itself
  after a later re-hydrate. It reuses the import's "convert from a verified
  archive" path.
- **Dehydrate** when a video is published, or on demand: remove local copies no
  unpublished video needs. Only files that can be made again from a verified
  NAS original are ever removed.
- Ghost Run suggests dehydrating once hydrated footage passes **50 GB**
  (a setting).
- Footage already on local disk shows as "local only" until an **Archive to
  NAS** job copies it to the NAS layout, verified.

### NAS layout

- `<footage root>/<YYYY>/<YYYY-MM-DD>/<original file name>`, the day by the
  camera's clock (as the Library dates clips today), not UTC.
- File names are kept as the camera wrote them. Only characters that aren't
  safe on SMB are escaped. A name already taken that day (ignoring case:
  Synology SMB shares are case-insensitive) goes in a numbered sub-folder
  (`2/`). Nothing is renamed or overwritten.
- Rides are named in the catalog, not in folder names, so naming or regrouping
  a ride never moves files on the NAS.
- Earlier imports stay where they are and are read as before.

### Catalog

`library.sqlite3` stops being rebuildable by rescanning: it now holds things
only Liam can say. New there:

- **Rides**: clips grouped by capture time (a gap of over 3 hours starts a new
  ride), with a name.
- **Tags** (gear for reviews, people, conditions), **flags** (pick, reject).
- **Collections**: hand-made groups across rides (a race build-up).
- **Features**: a spot (a jump, a drop) rather than a stretch of trail, with
  attempts found the way auto-place finds runs, and a landed/crashed flag.
- **Videos**: kind, input, status (planned → in Resolve → published, with
  its link and date) and **usage**: the clip ranges in it, recorded when Ghost
  Run builds it and again from the final Resolve timeline, so what was kept
  counts.
- Import history: each clip's NAS path and verification.

The catalog and the run database are backed up to the NAS
(`<footage root>/.ghost-run/backups/`, a dated `VACUUM INTO` copy, 30 kept).
`timer_db.yaml` stays the record of committed runs (schema v2, unchanged).

### Video kinds

A kind is a TOML template (kept with the brand kit, outside this repo; the repo
carries neutral examples): what input it takes, the order of sections, which
overlays, landscape or vertical. The first six:

| Kind | Input | Sections | Overlays |
|---|---|---|---|
| Trail, no talking | one run | title → whole run → end | timer optional |
| Gear review | clips tagged with the gear | hook → intro → talking and riding alternating → verdict | title cards |
| Progression, feature | a feature's attempts | attempts in order, with day and attempt number | attempt counter |
| Progression, speed | runs on one course | first → each PB → latest, a PB chart card | timer, ghost comparison |
| Story | a collection | rides in order, race runs with overlays | timer, title cards |
| Ridelog | a ride | talking and riding in order (0004's suggestions) | Strava stats |

0004's edit becomes the "ridelog"/"speed" structure; its audio suggestions and
thumbnails carry over unchanged.

### Strava

The cameras record no GPS, but a Strava activity is usually running. Matching
the camera's clock to the activity's per-second streams says where and how
fast each clip was filmed:

- suggest the course or feature, and group clips into rides;
- show Strava's segment times beside runs on Tracks;
- speed, heart rate, distance and climb for overlays;
- correct a camera clock that's a little off, using each run's start.

Ghost Run uses its own Strava API application (Liam registers it), with the
token stored locally. Strava data stays in the catalog, never in the repo.

### Tracks

Courses ridden most in recent weeks come first. Each shows the PB, the gap to
it and the time since, a PB trend chart, the sector that loses most against the
optimal lap, and an optional target time.

## Order of work

Each is its own pull request (or a few), with an issue:

1. **Inbox** (#32): import straight to the NAS layout, catalog files from the card,
   "safe to wipe", catalog backups.
2. **Library** (#33): rides, tags, flags, collections, thumbnails, usage filters
   (unused / in an unpublished project / released).
3. **Videos (basic)** (#34): a video of chosen clips, hydrate and dehydrate, the
   existing new-project bridge, usage read back from Resolve, published status.
   Needs #18 (real Resolve) checked first.
4. **Tracks** (#35): ridden-most order, PB trend, sector gaps, targets.
5. **Video kinds** (#36): speed progression and trail first, then ridelog, gear
   review, story, feature progression (with Features). Kinds with dissolves
   wait on the `edit_check.py` result (0004).
6. **Strava** (#37): match activities, course suggestions, segment times, stats.
7. **Overlay styles** (#38): ghost comparison, PB chart card, attempt counter,
   brand-kit title cards, vertical layouts.
8. Retire the Python script (0003 step 9), when Liam is ready.

## Consequences

- **The NAS holds the only copy of the originals** once a card is wiped and
  local copies are dehydrated. This footage isn't in the off-site backup, by
  choice (it's large and less crucial); the Synology's own disk redundancy is
  the protection. Synology snapshots of the footage folder would guard
  against deleting by mistake, at little cost.
- Editing needs the NAS reachable to hydrate; everything else works without it.
- The catalog has to be backed up, and a lost catalog loses names, tags and
  usage (not footage or runs).
- The Library page becomes a view on the catalog, not a scan of local folders;
  local footage folders remain as the hydration target and for footage not yet
  archived.
