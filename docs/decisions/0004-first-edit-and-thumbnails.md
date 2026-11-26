# 0004 — The first edit and the thumbnail, made by Ghost Run

Status: accepted (2026-11-26). The timeline-file format is waiting on a check
in real Resolve (`scripts/edit_check.py`).

## Context

Ghost Run already sends timed runs to Resolve: the clips back to back, each
with its timer overlay above it (0003). Making a video still means cutting
the rest by hand: a hook of quick action shots, the channel's intro
animation, a talking intro, the timed run, a wrap-up and an ending. Each
video also needs a thumbnail in the channel's style.

The goal is for Ghost Run to make that first edit and the thumbnail. Liam
then does only colour, the final audio mix and the export in Resolve.

## Decision

### The edit

- **Order:** stingers → intro animation → talking intro → timed run(s) →
  wrap-up → fade to black.
  - **Several runs:** the runs follow each other, each with its own overlay,
    between one talking intro and one wrap-up.
- **Built in Resolve, not rendered.** Ghost Run writes the edit as a
  timeline file, and the bridge imports it into a new project.
  - Rendering the edit with ffmpeg would bake in the cuts and leave nothing
    to adjust while grading.
  - Resolve's scripting API can place clips but can't add transitions, so a
    bridge-only edit would have hard cuts and no fade.
  - A timeline file can also be tested without Resolve, which the devbox
    doesn't have.
  - Format: FCPXML 1.9, or OpenTimelineIO, whichever Resolve imports
    faithfully. `scripts/edit_check.py` writes the same test edit both ways
    for checking in Resolve.
  - Fallback if neither imports well: the bridge places the clips with hard
    cuts, and the fades are added by hand.
- **Tracks:**
  - V1: the footage.
  - V2: the intro animation and each run's timer overlay.
  - A1: the clips' own audio.
- **Intro animation:** one transparent, silent file, set in Settings. It sits
  on V2 across the cut from the last stinger into the talking intro:
  - it builds over the end of the stingers;
  - its hold lands on the cut;
  - it blows off over the start of the talking intro.
  - Settings give the frame of the animation that lands on the cut: the
    middle of the hold, frame 94 of the Work From Loam intro, whose hold is
    frames 78–111.
  - No blur under it to start with. Blur can be added if the logo is hard to
    read over busy footage.
- **Joins:**
  - Hard cuts between stingers.
  - A short dissolve from the talking intro into the run.
  - A 1 s fade to black at the end of the wrap-up.
- **The run:** from a little before Start to a little after Finish. Defaults
  are 2 s and 3 s, both in Settings.

### Suggested sections, not hand-marked ones

Ghost Run suggests every section from the clips' audio. Liam accepts,
rejects or nudges each one before the edit is written.

- **Analysis:** audio is decoded once per clip (mono, 16 kHz) and cached like
  the auto-markers frames.
  - Loudness counts against its local baseline (a rolling median of the
    surrounding seconds), not against a fixed level, because GoPro wind
    noise is loud for the whole run.
- **Telling speech from bike noise:** mostly by where a spike is.
  - Inside a run (Start to Finish), loud moments are action.
  - Outside a run, and in clips with no run, steady sound in the voice band
    while the picture hardly moves is talking.
  - The frame cache gives the motion.
  - If this misjudges real clips, a small voice-activity model (e.g.
    Silero VAD) comes next.
- **Intro and wrap-up:** talking can be in the run's clip (before Start or
  after Finish) or in a clip of its own. Both are supported:
  - The intro is speech before the first run's Start, or the mostly-speech
    clip recorded shortly before it (by capture time).
  - The wrap-up is the same search after the last run's Finish, or in the
    clip recorded after it.
  - A section starts just before the first word and ends at a pause.
- **Stingers:**
  - **Total length:** 10–20 s, scaled at about 15% of the total run time.
  - **Cuts:** 1–2 s each, with the spike about 60% of the way into the cut.
  - **Spread:** across the run, and across all runs in a multi-run video.
  - **Order:** smallest moment to biggest, so the hook builds into the intro
    animation.
- **Where suggestions are kept:** in the app's SQLite library, not in
  `timer_db.yaml`, so the YAML contract (schema v2) is unchanged.

### Thumbnail

- **Brand kit:** a `brand.toml` with its colours, fonts and logos, kept
  outside this repo. Ghost Run is general and public; a channel's kit is
  its own. Settings hold the path to it. Work From Loam's kit lives in
  `glib/work-from-loam` under `brand/`.
  - **Format version 1:**
    - `[colors]` holds `text`, `background`, `accent`, `faster`, `slower`
      and `pb`, plus optional `level` and `neutral`.
    - `[fonts]` holds `headline`, `label` and `numbers`. Each is a static
      TTF: `fontdue` can't set variable-font axes.
    - `[logo]` holds `full`, `small`, `square` and `mark`. Each is an SVG
      in one colour with its text as outlines, drawn with `resvg`.
    - `[thumbnail]` holds the size and the layout. Boxes are `[x, y, w, h]`
      fractions of the canvas, and sizes are fractions of its height.
  - Paths are relative to the file, and unknown keys are ignored.
  - The repo will carry a neutral placeholder kit for tests.
- **Frames:** the sharpest frame near each of the biggest stinger moments,
  4–6 candidates. Liam picks one in the app.
- **Picture:** the frame fills the canvas, graded a little. On top of it,
  in order:
  - a scrim;
  - the logo;
  - the course name (shrunk to fit two lines);
  - a badge sized to its label: "PB", a delta like "-1.24" coloured faster
    or slower, or "3 RUNS";
  - the time.
- **Output:** a 1920×1080 JPEG under 2 MB, saved next to the edit.
- **Overlay:** the timer overlay keeps its own look for now. It could take
  the kit's colours and number font later.

## Order of work

1. `scripts/edit_check.py`, so Liam can import its two test edits into
   Resolve and report what survives. This decides the file format.
2. Audio and motion analysis, plus `rtimer suggest`, which outputs the
   suggestions as JSON. Tuned on Liam's audio extracts, which stay out of
   the repo.
3. An Edit page: pick runs, review and adjust suggestions, and the
   intro-animation and padding settings.
4. The timeline writer in Rust, plus the bridge import (new project, proxies
   linked).
5. The brand kit reader, the thumbnail renderer and the frame picker.

## Consequences

- The edit depends on how Resolve imports a timeline file, which only a real
  Resolve can show. `edit_check.py` tests that first, before any of the
  writer is built.
- Suggestions will be wrong sometimes, so the review step is part of the
  design, not an extra.
- A second channel or a new look means a different brand kit, not a code
  change.
