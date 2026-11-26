# Tidy the footage: a one-off job for a Claude agent on the desktop

Ghost Run keeps camera originals only on the NAS, under one footage root, and
pulls them to local disk while a video is being edited (decision 0005). Before
that works, two things need sorting once:

- the NAS folder Liam keeps this footage in, which is a mix of footage, finished
  videos, inputs in a `video_in` folder, old branding and more;
- about 20 GB of footage on local disk, much of it probably on the NAS already
  but never matched up.

Run this on the desktop, where the NAS is mounted, from a checkout of
ghost-run:

```sh
claude "$(cat docs/agents/tidy-footage.md)"
```

Everything below the line is the prompt.

---

You're tidying the video footage for Ghost Run, a desktop app that times
mountain-bike runs and builds videos in DaVinci Resolve. Read `CLAUDE.md` and
`docs/decisions/0005-workspaces-and-hydrated-footage.md` in this checkout
first. You'll work in two phases, each ending with a plan Liam approves before
anything moves. Nothing on the NAS is ever deleted, and no local file is
deleted until a byte-identical copy (or the original it was made from) is
verified on the NAS.

## Find the places

- **Ghost Run's settings.** The import settings are in
  `~/.config/ghost-run/import/settings.toml`: `archive.path` is the
  **footage root** on the NAS, and `editing.path` is the local folder import
  puts editing copies in. The app's own `~/.config/ghost-run/settings.toml`
  lists `footage_folders` (local folders the Library scans) and
  `run_database`.
- **The share.** The footage root sits inside Liam's Resolve folder on the
  Synology share. Call that folder the **share folder**, and confirm it with
  Liam. If `archive.path` isn't set, ask Liam where the footage root should
  be. The default is `<share folder>/footage`.
- **Check the mount.** Use `findmnt -T <path>`: the footage root must be on a
  CIFS/SMB mount. Stop and say so if the share isn't mounted, rather than
  writing into an empty local folder.
- **Is Ghost Run running?** If so, ask Liam to close it, so the Library and
  the import journal aren't in use.

## Before anything moves

- Copy the run database (`run_database`) and
  `~/.local/share/ghost-run/library.sqlite3` to
  `<share folder>/.ghost-run/tidy-backup-<date>/`.
- Ask Liam to commit any markers still in draft in Ghost Run, since drafts are
  keyed by the clip's local path.
- Note that Resolve projects pointing at files that move will need relinking
  (Resolve's Relink Media finds them by name). Tell Liam this up front.

## Rules for every move and copy

- Work in a log file: `<share folder>/.ghost-run/tidy-<date>.csv` with columns
  `action, from, to, bytes, sha256, reason`. Append each action as it happens,
  so the log is always a complete record and a way to undo.
- **Moves on the NAS** within the share are renames (`mv` on the same mount):
  fast, and they keep the bytes. Never overwrite: if the target exists, compare
  hashes. If they're identical, move the newcomer to
  `<share folder>/_duplicates/<its old path>`. If they differ, use the next
  free numbered folder (`2/`, `3/` …) beside the target.
- **Copies to the NAS:** write to `<target>.partial`, `fsync`, compare SHA-256
  of source and copy, then rename into place. A mismatch is retried once, then
  reported.
- **Leave these alone:**
  - `#recycle`, `@eaDir` and other `@`-prefixed Synology folders,
    `.ghost-run/`, hidden files;
  - everything under the footage root already in the `YYYY/YYYY-MM-DD/` layout
    (Ghost Run imported it, and its journal records those paths).
- Don't follow symlinks. The share is case-insensitive: treat names that
  differ only in case as the same name.
- Don't rename camera files. Ghost Run matches stored runs to clips by file
  name.

## Phase 1: sort the share folder

Walk the share folder (skipping the above) and classify every file. Use
`ffprobe` for videos: duration, codec, resolution, the `creation_time` tag and
`timecode`.

- **Footage**: camera originals and their sidecars.
  - Recognise them by camera naming, e.g. `DJI_YYYYMMDDHHMMSS_NNNN_D.MP4`
    with `.LRF`/`.SRT` sidecars, GoPro `GX`/`GH`/`GOPR` names, Insta360
    `VID_`, and phone `YYYYMMDD_HHMMSS`.
  - Also by codec and timecode, or by sitting in `video_in` or a dated shoot
    folder.
  - Each goes to `<footage root>/<YYYY>/<YYYY-MM-DD>/<original name>`. Sidecars
    go beside their video.
  - The day is the camera clock's day, worked out exactly as Ghost Run does:
    1. a 14-digit `YYYYMMDDHHMMSS` group in the file name (or `YYYYMMDD`
       followed by a 6-digit group);
    2. else the `creation_time` tag, taken as UTC;
    3. else the file's modification time, in local time.
  - Names keep their spelling. Only `\ : * ? " < > |` and `~` are escaped, as
    `~hh`.
- **Converted copies**: Ghost Run's or Resolve's editing copies of an original,
  such as `NAME.MP4.mov`, ProRes or DNxHR files with a camera name, and the old
  `f-~44~4a~49…` escaped names under `DATE/UUID/clips/`.
  - If the original is on the share (same name stem, about the same duration),
    the copy can be made again from it: move it to `<share folder>/_regenerable/`.
  - If the original isn't on the share, treat the copy as footage.
- **Proxies and caches**: `Proxy/`, `CacheClip`, `.gallery`, `OptimizedMedia`,
  Ghost Run's `*.overlay.mov` and `Overlays/`. Move them to `_regenerable/`.
- **Exports**: finished videos (H.264/H.265, usually longer, names that aren't
  camera names, often in an output folder).
  - Each goes to `<share folder>/exports/YYYY-MM-DD Title.ext`, with the date
    from the name, else `creation_time`, else the modification time.
  - The title is from the file name, made readable. Keep the old name in the
    log.
- **Brand**: logos, intro/outro animations, stingers, music, sound effects,
  fonts, LUTs, title templates, `.drfx`/`.setting` Fusion files and images.
  They go to `<share folder>/brand/` in subfolders by kind.
- **Projects**: `.drp`, `.dra` and Resolve project backups go to
  `<share folder>/projects/`.
- **Anything else** goes to `<share folder>/other/`, keeping its folder. If
  you're unsure what something is, it goes to `<share folder>/_review/` with
  the reason in the plan.

Then write the plan to `<share folder>/.ghost-run/tidy-plan-1.csv` (same
columns as the log, `action` = `move`), and show Liam a summary:
- counts and sizes per class;
- every `_review` item with its reason;
- duplicates found;
- 20 sample footage moves with their day and how the day was found.

Ask Liam to approve, adjust or exclude. Carry out the plan only after Liam
says yes. Afterwards, check that every planned target exists with the
recorded size, and report anything that didn't move.

## Phase 2: match local footage, copy what's missing, clear local copies

1. **List the local video files.** Look in every folder in `footage_folders`,
   in `editing.path`, and anywhere else Liam names. Include their `Proxy/` and
   `Overlays/` folders.
2. **Use the import journal.** It is at
   `~/.local/share/ghost-run/import/jobs.sqlite3`. Open it read-only, e.g.
   `sqlite3 'file:…?mode=ro' ...`, and read its schema first. Each batch item
   records its source, archive path and editing path. An editing copy whose
   archive original is verified and still on the NAS is regenerable.
3. **Classify each local file:**
   - **On the NAS already**: the same name (ignoring case) and size under the
     footage root, confirmed by SHA-256.
   - **Regenerable**: an editing copy, proxy or overlay whose original is on
     the NAS (from the journal, or by name stem and duration).
   - **Missing from the NAS**: an original, or a converted copy whose original
     exists nowhere. Plan a verified copy to
     `<footage root>/<YYYY>/<YYYY-MM-DD>/<name>` by the same day rule.
4. **Write the plan.** Save it to `<share folder>/.ghost-run/tidy-plan-2.csv`
   and give Liam a summary: per class, counts, sizes, and the space freed.
5. **Copy.** After Liam approves, copy the missing files, verified.
6. **Delete local copies.** Then ask Liam a second time before deleting local
   files: delete only what is on the NAS (verified this run), or regenerable.
   Log each deletion with its hash.
7. **Leave everything else.** Leave settings, the run database and the library
   database as they are, and leave empty folders unless Liam asks.

## When you finish

Report:
- what moved, was copied and was deleted, with sizes;
- where the log is;
- what's left in `_review/` and `_regenerable/`, which Liam can delete once
  happy;
- anything that failed.

Remind Liam that Resolve projects may need Relink Media, and that Ghost Run's
Library will list NAS-only footage once stage A of 0005 ships (until then,
clips that were only local disappear from the Library, and their runs are kept
in the run database).
