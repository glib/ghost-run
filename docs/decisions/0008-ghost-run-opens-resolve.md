# 0008 — Ghost Run is where Resolve projects are opened from

Status: accepted (2026-11-28), after a second workflow audit.

## Context

Since 0005 and 0007 Ghost Run knows where every clip is (the NAS, a local
copy, a preview) and can copy footage down and point Resolve at it. But
Resolve was still where work started: every bridge command acts on the
project open in Resolve, and `read_timeline` and `relink` refuse any other.
So opening an old edit meant opening Resolve, finding the project, seeing its
media offline, going back to Ghost Run for Bring back footage… or Link open
timeline, then back to Resolve.

The audit found more friction around the same seam:

1. Removing local copies only protected clips that Ghost Run's own unfinished
   videos use (`videos::freeable`). A project made by hand in Resolve and never
   linked lost its footage without a word.
2. The Library and Tracks pages' Resolve ▾ menu (Add to open timeline / New
   project…) made projects that no video stands for, and recorded no usage.
3. Each build makes a new project ("Title 2", "Title 3").
4. Footage is copied down in four places, each its own way (Library Copy
   here, building a video, sending runs to Resolve, Bring back footage), only
   some of them in the jobs drawer (#52).
5. Fixed local paths only relink by themselves if every machine has the same
   local footage root (the laptop's Resolve uses the same project server).
6. Much of what was built hasn't met real use yet (auto-markers #6, captions
   and cards in Resolve, Strava, Bring back footage, the shared catalog).
   Building goes on alongside; Liam tests later.
7. The Python Resolve script and its parity check still cost every database
   change. Liam no longer starts anything from it.

Resolve's project library is in Postgres on resolve-db, the server the shared
catalog is on (0007).

## Decision

- **Read Resolve's database, read only.** The `ghost_run` role is given
  CONNECT, USAGE and SELECT on Resolve's project databases (any with an
  `SM_Project` table) by `scripts/catalog-db-setup.sh` (`--resolve` for a
  server already set up), now and for tables Resolve adds later; the session is
  also set read only. Nothing in Resolve's schema is ever written. The tables
  read (`SM_Project` → `Sm2Timeline` → `Sm2Sequence` → `Sm2TiTrack` →
  `Sm2TiItem.MediaFilePath`) aren't documented; they're what community tools
  have worked out for Resolve 19. `library::resolve_db` checks them first and
  says what's missing, the setup script prints the same check, and Ghost Run
  then falls back to the bridge (Link open timeline, Bring back footage).
- **The Videos page lists every Resolve timeline.** Videos first, then "In
  Resolve, not videos yet": each timeline with files, newest first, with how
  much of its footage is here (here, on the NAS, to relink, missing). A
  timeline becomes a video with Make it a video (no Resolve needed). Liam
  chose this over listing only linked ones.
- **Usage keeps itself up to date.** Each time Resolve's projects are read, a
  linked video's usage is what its timeline uses now; Read back stays for
  when the database can't be read.
- **Removing local copies asks Resolve first.** Clips a timeline uses, in a
  project saved in the last 60 days and not a published video's, are kept,
  and the message names the projects. Without a shared catalog nothing
  changes.
- **Hydrate and open:** a timeline or a linked video has Open in Resolve
  (Copy footage and open in Resolve when some is on the NAS). From Resolve's
  database it plans what to copy, as Bring back footage does, and asks with
  the size; copies; starts Resolve if it isn't running (its scripting gets
  three minutes to answer); saves the project open there; opens the project
  (found in any project-manager folder) at the timeline on the Edit page (the
  bridge's `open`); and relinks files whose paths have moved. It runs from the
  Videos page like Bring back footage. Making every copy a drawer job is left
  to #52. A linked video's Build in Resolve stays "Build again"; Open is the
  main button.
- **Every project Ghost Run makes is a video.** The Resolve ▾ menu's New
  project… is now New video in Resolve…, and its project is recorded as a
  video with what it used; Add to open timeline stays.
- **The Python script goes** (0001's plan, PR 9): the Resolve script, its
  Fusion overlay, the Windows scripts and the parity check. `rtimer` stays.
- **Machines with different footage roots** (the laptop's Resolve uses the
  same project server) still work, as each open relinks to this machine's
  paths. Doing better is #77.

## Consequences

- With the shared catalog connected, nothing has to be open in Resolve to see
  which projects can be edited here.
- A Resolve update can move the tables. Ghost Run then says so on the Videos
  page and works as before through the bridge; the reader needs updating.
- The same camera name used on several days (GoPro numbering) can't be told
  apart from the database alone (it has no clip length per file): such files
  show as missing, and Bring back footage (which asks Resolve for length and
  timecode) handles them.
