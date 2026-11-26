# 0005 — Workspaces for each job, footage kept on the NAS and hydrated to edit

Status: accepted (2026-11-26). Revised the same day after first use: the
import flow (local first, then the NAS, then dehydrate), a one-off tidy of the
NAS and local footage instead of an Archive to NAS job, a video is a Resolve
timeline, and the work in three stages.

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

- **Import** pulls the card to local disk, makes the catalog files, the
  editing copy and the proxy there (fast, and off the card), copies each
  original to the NAS and verifies it, then **dehydrates**: the local copies go
  unless **Keep for editing** is on, for the whole import or the clips picked
  (for a video started straight after the ride). The card is **safe to wipe**
  once every original is verified on the NAS; a local copy alone never counts.
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
- A clip counts as **on the NAS** when its file is under the footage root, as
  catalogued, not only when the import journal copied it there.
- Footage already on local disk (about 20 GB, much of it probably on the NAS
  already) and the existing NAS folder (finished videos, inputs in
  `video_in`, old branding, all mixed) are sorted out once by a Claude agent
  on the desktop, which has the NAS mounted:
  [`docs/agents/tidy-footage.md`](../agents/tidy-footage.md). It puts footage
  in the layout below, matches local files to it, copies what's missing,
  verified, and then clears the local copies. No Archive to NAS job is built.

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
- Beside the footage root, in the same share: `Exports/` (finished videos,
  `YYYY-MM-DD Title.ext`), `Brand/` (logos, intro animations, music, fonts,
  title templates), `Projects/` (Resolve project exports), `Other/`, and
  `_review/` for whatever the tidy couldn't place. Ghost Run reads only the
  footage root.

### Catalog

`library.sqlite3` stops being rebuildable by rescanning: it now holds things
only Liam can say. New there:

- **Rides**: clips grouped by capture time (a gap of over 3 hours starts a new
  ride), with a name.
- **Tags** (gear for reviews, people, conditions), **flags** (pick, reject).
- **Collections**: hand-made groups across rides (a race build-up).
- **Features**: a spot (a jump, a drop) rather than a stretch of trail, with
  attempts found the way auto-place finds runs, and a landed/crashed flag.
- **Videos**: a video is one Resolve timeline (the one rendered for
  YouTube). Kind, input, status (planned → in Resolve → published, with its
  link and date, typed in by hand) and **usage**: the clip ranges in it,
  recorded when Ghost Run builds it and read back from the final timeline, so
  what was kept counts. An existing timeline can be **linked** as a video, so
  videos already published count their footage as released.
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

Three stages. Each finishes a workflow end to end and is a few pull requests;
Liam tries a stage as a whole rather than each piece.

**Stage A: all footage on the NAS, in the Library, imported the new way**
(#32)

1. The one-off tidy, run by Liam on the desktop
   ([`docs/agents/tidy-footage.md`](../agents/tidy-footage.md)).
2. The catalog: the Library lists every clip under the footage root,
   NAS-only ones too, with thumbnails, the ~540p preview (timing reads it),
   auto-place's frames and 16 kHz audio made locally; "on the NAS" means under
   the footage root.
3. Hydrate and dehydrate on clips: the editing copy and proxy at a fixed local
   path, a hydrated-footage total, and the suggestion past 50 GB.
4. The new import: card → local → catalog files, editing copy and proxy → NAS,
   verified → dehydrate, with Keep for editing.
5. Inbox, Library, Videos (empty until stage B), Tracks in the sidebar; a jobs
   drawer instead of the Output page.
6. After an import, the course found and markers placed as drafts, ending on
   what needs checking, flagged by confidence (#6 tunes it).

**Stage B: make a video, and know what's been used** (#34, #33, #36's first
part)

1. Videos: a video of chosen clips, hydrated and built in Resolve. Clips go in
   as they are; overlays render only for the timed runs that want one (today
   nothing reaches Resolve without one). Status, the YouTube link, usage read
   from the final timeline, dehydrate on publishing.
2. Link an existing Resolve timeline as a video.
3. Library: rides, tags, flags, collections, and the unused / in a project /
   released filters.
4. The first two kinds: trail (no talking) and speed progression. Both are
   cuts built through the bridge, so they don't wait on `edit_check.py`.

**Stage C: richer videos** (#36's rest, #38, #37)

1. Ridelog, gear review, story, feature progression (with Features).
2. 0004's first edit (suggested sections, intro, padding) and thumbnails,
   once `edit_check.py` has been tried in Resolve.
3. Overlay styles: ghost comparison, PB chart card, attempt counter, brand-kit
   title cards, vertical.
4. Strava: activities matched to clips, course suggestions, segment times on
   Tracks, stats for overlays.
5. Retire the Python script (0003 step 9), when Liam is ready.

## Consequences

- **The NAS holds the only copy of the originals** once a card is wiped and
  local copies are dehydrated. This footage isn't in the off-site backup, by
  choice (it's large and less crucial); the Synology's own disk redundancy is
  the protection. Synology snapshots of the footage folder would guard
  against deleting by mistake, at little cost.
- Editing needs the NAS reachable to hydrate; everything else works without it.
- Import ends with the footage dehydrated by default, so an edit started the
  same day needs Keep for editing ticked (or a hydrate afterwards).
- Ghost Run doesn't keep Resolve markers on a new project; they were only for
  the old script, and overlays now come from stored runs.
- The catalog has to be backed up, and a lost catalog loses names, tags and
  usage (not footage or runs).
- The Library page becomes a view on the catalog, not a scan of local folders;
  local footage folders remain as the hydration target and for footage not yet
  archived.
