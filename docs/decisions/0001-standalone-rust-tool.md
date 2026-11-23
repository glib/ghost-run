# 0001 — Move to a standalone Rust tool; keep the Python Resolve script until it's replaced

Status: accepted (2026-11-23)

## Context

Resolve Timer began as a Python script inside DaVinci Resolve Studio: it reads
source-clip markers from the Media Pool, times runs against a YAML database,
and builds a live Fusion/Text+ overlay. Two pain points drive the next stage:

- Placing Start/S1…/Finish markers by hand on every clip.
- The live Fusion overlay re-evaluates every frame, which makes timelines heavy.

The planned fixes run outside Resolve: an overlay rendered to a single
transparent video file (ProRes 4444 with alpha) that drops into any editor, and
markers found automatically from the video (matching against a reference run),
later from UWB gate hardware. Once those exist, Resolve's scripting API (Python
or Lua only) is the only reason left to use Python. About 2,500 of the ~5,500
lines are Resolve UI, adapter and Fusion code; the core (timing, database,
matching, stats, CLI) has no Resolve dependency.

## Decision

- New work is a standalone **Rust** tool: a Cargo workspace with a core library
  and a CLI. ffmpeg (as a subprocess) decodes and encodes video; overlay frames
  are drawn in Rust. If a desktop UI is needed, it uses GTK4/libadwaita, like
  `resolve-ingest`.
- The **YAML database stays**, schema version 2, read and written compatibly, so
  existing databases carry over unchanged.
- The core is ported with the Python tests as the reference: the same database
  and markers must give the same times, deltas and comparisons.
- The **Python Resolve script stays usable** (bug fixes only) until the Rust tool
  covers the workflow, then it is removed in its own change.

## Why Rust

- Frame-by-frame work (gate matching, rendering 4K overlay frames) needs speed.
- One language with `resolve-ingest` and the planned UWB host tools, so
  ingest → timing → overlay can later become one tool.
- Strict compiler feedback suits agent-driven development; a single binary with
  no Python environment is easier to install, and to sell later.

## Consequences

- For a while two implementations exist; the YAML schema is the contract
  between them, and both must pass tests against the same example database.
- Where markers are reviewed once they're found automatically (Resolve via a
  thin script, or our own UI) is decided with the auto-marker work.
