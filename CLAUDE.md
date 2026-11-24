# Ghost Run (formerly Resolve Timer)

Self lap timing for mountain-bike racing: Start/S1…/Finish markers on a clip give
sector and lap times, compared against earlier runs (best lap, optimal lap) and
shown as a video overlay. One user (Liam), public repo, no deploy, Linux only.

It is becoming one GTK4/libadwaita desktop app, `ghost-run`
(`docs/decisions/0003-one-app-ghost-run.md` has the plan and the order of PRs):
Import (the merged resolve-ingest app), Library, Runs, Markers, Output/Resolve.
The timing workflow is still a Python script inside DaVinci Resolve Studio. It is moving to a
standalone Rust tool (`docs/decisions/0001-standalone-rust-tool.md`): an overlay
rendered to one transparent video file, then markers found automatically from
the video. The Python script stays usable, bug fixes only, until Rust covers the
workflow.

## Commands
- `make install` — builds and installs `ghost-run` + `rtimer` to `~/.local/bin` with a launcher and icon (`Exec` goes through `distrobox enter $CONTAINER_ID` when built in a distrobox).
- `cargo test` — Rust tests (the app needs `libgtk-4-dev libadwaita-1-dev`; screenshots under `xvfb-run` with `GSK_RENDERER=cairo` and a throwaway `XDG_CONFIG_HOME`/`XDG_DATA_HOME`). Before pushing: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`.
- `cargo build -p timer-cli && scripts/parity.sh` — runs the Python and Rust CLIs on `examples/` and `tests/fixtures/` databases and fails on any difference in output or in the files they write.
- `python -m unittest discover -s tests` — Python tests (needs `PyYAML`, `Pillow`: `pip install -r requirements-resolve.txt`). The devbox has no pip: unpack the two wheels from PyPI into a temp dir and set `PYTHONPATH` to it.
- CI (`.github/workflows/ci.yml`, GitHub-hosted runners) runs Python 3.10 and 3.13 tests, Rust fmt/clippy/tests, and the parity check. CI is the gate.

## Shipping a change
When asked to build something, take it through to `main` without checking in.
**This is standing permission to push your branch, merge your own PR once CI is
green, and delete your own branch.** Stop early only for a real question, or
when asked for a draft PR or a review first.

Ask, then wait, only when you can't reasonably decide: choices that change what
the user sees and are costly to undo, changes that break existing `timer_db.yaml`
files, new paid services or accounts, or changes to CI security (e.g.
self-hosted runners). Ask everything in one go, up front. Otherwise pick the
sensible option and list assumptions in the PR description.

1. **Branch** off fresh `origin/main` (a worktree, or a branch in your own clone). Never commit to `main`.
2. **Build** with tests. Run the tests you touch while working; the full suite before pushing.
3. **PR:** commit, `git push -u origin HEAD`, `gh pr create` (what changed, assumptions, how it was tested).
4. **Watch CI:** `gh pr checks <N> --watch`, on its own. Never pipe it or chain the merge after it with `&&`.
5. **Red:** `gh run view <run-id> --log-failed`, fix, push, watch again. Flaky: rerun the failed jobs once and open an issue. After three failed fixes, or if the fix needs a decision, stop and report.
6. **Green:** if `main` moved on, merge `origin/main` in and watch again. Then `gh pr merge <N> --squash --delete-branch`. Never merge a red or pending PR.
7. **Clean up:** confirm `gh pr view <N> --json state` says `MERGED`, remove your worktree or branch, `git fetch --prune`.
8. **Report:** PR number, merge commit, assumptions, issues filed.

## Layout
- `crates/app/` — the desktop app (`ghost-run`): no arguments opens the window; subcommands are the headless import commands. `shell.rs` is the sidebar + page stack; each page is a module with its own header bar and a worker thread it sends commands to and polls replies from every 100–150 ms (`library_page.rs`, `time_page.rs`, `runs_page.rs`, `output_page.rs`; `src/ui/` is the old resolve-ingest window, now the Import page). Pages share settings and change notices through `shared.rs` (`Shared::update` saves and notifies; `Change::Runs` after a database save; `Shared::time_clip` opens a clip on the Time page, `Shared::render_runs` queues overlays on the Output page); `resolve.rs` + `resolve_bridge.py` are the Resolve bridge: the Python (stdlib only, built into the binary with `include_str!`, written to the data dir) runs with Resolve's Python via `distrobox enter -n davincibox` (`distrobox-host-exec` first when the app is itself in a box; directly if `/opt/resolve` is local), one JSON request on stdin, the reply as the last JSON line of stdout; `status` and `add_overlays` (each overlay on a free track above its clip's timeline items, trimmed to what's on the timeline, imported once into a "Ghost Run overlays" bin). Its placement maths and a fake Resolve object model are tested in `tests/test_resolve_bridge.py`; `resolve.rs` tests the round trip with a stand-in `DaVinciResolveScript` module. Never tested here against a real Resolve (none on the devbox). `settings_dialog.rs` is the one settings dialog (sidebar gear). `GHOST_RUN_PAGE=runs` opens on a page and `GHOST_RUN_CLIP=<path>` opens a clip on the Time page (screenshots). The Time page (`time_page.rs`) reads frames on a latest-wins decoder thread (`library::frames`, proxy first), keeps uncommitted markers as drafts in the library, previews timing with `timer_core::preview` and commits through `timer_core::commit` + `DatabaseFile`.
- `crates/library/` — the footage library, headless: `settings` (the app's own `~/.config/ghost-run/settings.toml`: footage folders, run database, Resolve template), `store` (SQLite `~/.local/share/ghost-run/library.sqlite3`, rebuildable by rescanning), `scan` (walks the folders, skips `Proxy`/Resolve cache/hidden folders and originals with a `name.ext.mov` editing copy beside them, reprobes only changed files), `probe` (ffprobe), `proxy` (ProRes 422 Proxy at half size in `<clip dir>/Proxy/<stem>.mov`, `-noautorotate` so frames and the rotation flag match the clip; `verify` checks frame count, rate, timecode and rotation before a proxy counts), `frames` (one frame by number via ffmpeg, upright, exact on long-GOP originals too; tested against ffmpeg's `select`), drafts (`Library::draft`/`set_draft`: markers not yet committed), `auto` (auto-markers for the app: a course's reference run is the one chosen (`course_references` table) else its fastest with findable footage; `place` aligns a clip with it via `timer_align`, `match_courses` ranks every course's; reads proxies first, frames cached in `~/.cache/rtimer`; tested on generated footage in `tests/auto.rs`), `overlays` (`<stem>.overlay.mov` in the chosen folder or `Overlays/` beside the clip; scans skip both), renders (`Library::rendered`: each run's overlay, its first clip frame and timecode, and the run's fingerprint when rendered, so the Resolve bridge can place it and stale ones can be found).
- `crates/ingest/` — import core, merged from `glib/resolve-ingest` with its history (MIT, its own `LICENSE`): discovery, verified CIFS archive copies, PCM editing MOVs, SQLite journal, child-process supervision. Settings/journal paths keep the `resolve-ingest` names so existing setups carry over. Docs and validation evidence: `docs/ingest/`.
- `packaging/` — launcher `.desktop` (`@EXEC@` filled in by the Makefile) and icon.
- `crates/timer-core/` — Rust core: `models` (courses, runs), `database` (YAML v2 load/save; `yaml.rs` writes PyYAML's exact layout and quoting), `capture_time`, `markers`, `timing`, `matching` (clip → stored run; the fingerprint hash matches Python's byte for byte), `stats` (best/optimal lap), `validation`, `py` (Python-compatible coercions, `repr`, JSON escaping, half-even rounding).
- `crates/timer-core/src/board.rs` (a course's leaderboard: ranks, sectors, fastest, why a run doesn't count), `edit.rs` (batch edits that apply whole or not at all; moving runs needs the same sector count), `file.rs` (`DatabaseFile`: saves only if the file is unchanged since loading, never into a newly invalid database, with a timestamped backup in `<name>.backups/`, 20 kept) — what the app's Runs page uses. `commit.rs` (markers → a committed run, ids and fields as the Python tool writes them; updates the clip's existing run) and `preview.rs` (partial timing while markers are placed, against the course's best) are the Time page's.
- `crates/timer-core/src/overlay.rs` — what the overlay shows for a clip (`overlay_payload`: compared only with runs captured before it, never its own stored run), its rows, text and delta colours.
- `crates/timer-render/src/job.rs` — rendering a run's overlay file (frame range, split across ffmpeg processes per `encode::plan`, joined, the clip's timecode; written to a hidden `.<name>.partial.mov` and renamed), with progress and cancel: shared by `rtimer render` and the Output page.
- `crates/timer-render/` — draws overlay frames (the Fusion comp's layout and colours, minus the panel blur, which a separate file can't do; bundled JetBrains Mono, OFL) and encodes them with ffmpeg: only the panel is drawn and piped, and ffmpeg pads it to the full transparent frame (`Output::region`); ProRes 4444 with alpha (`prores_aw`, ~4x faster than `prores_ks`; ffmpeg's pad step is single-threaded at ~90 fps at 4K, so `encode::plan` splits a render across ffmpeg processes, each encoding a stretch, joined losslessly by `concat`; processes × threads are planned to fit half the free memory (~660 MB a process + ~230 MB a thread at 4K): unplanned parallel ffmpegs once ran the 8 GB devbox out of memory. Never test heavy renders there without a memory watchdog) carrying the clip's timecode, or an image sequence numbered by clip frame.
- `crates/timer-align/` — places a clip's markers from a reference run (`auto-markers`): frames read at 64×36 grey through ffmpeg (`video.rs`, cached by path/size/mtime), coarse subsequence DTW at ~10 samples/s (speed ratio limited to 3×), then per-marker refinement on full-rate frames with a confidence. Tested on made-up "trail" footage (a pan over random texture); no real footage in the repo.
- `crates/timer-cli/` — `rtimer`: the Python CLI's database and overlay commands (`courses`, `runs`, `stats`, `validate-db`, `normalize-db`, `overlay-payload`, `overlay-text`) with identical arguments and output, plus `render` and `auto-markers` (Rust only). `tests/render.rs` and `tests/auto_markers.rs` need ffmpeg: they skip without it locally, and fail in CI (`REQUIRE_FFMPEG`).
- `tests/fixtures/` — made-up databases and marker CSVs for the parity check.
- `src/resolve_timer/` — Python package. Resolve-free core: `timing.py`, `database.py` (YAML, schema v2), `matching.py`, `stats.py`, `markers.py`, `models.py`, `validation.py`, `service.py`, `cli.py`, `summary_card.py`. Resolve-only: `resolve_adapter.py`, `ui.py`, `ui_controller.py`, `runtime_support.py`, and the Fusion part of `overlay.py`.
- `scripts/` — Resolve launcher and installers (Linux davincibox, Windows).
- `tests/` — unittest; Resolve is faked, so everything runs without it.
- `examples/` — example database and markers CSV (made-up runs).
- `docs/decisions/` — short ADRs for significant choices. Add one when you make one.
- `PLAN.md` — the original design and status of the Python tool.

## Conventions
- **The YAML database is the contract** between the Python and Rust tools: schema version 2, same field names and meaning. Never break existing databases; a schema change needs a migration and a version bump.
- **Parity:** while both tools implement a command, `scripts/parity.sh` must stay green. Porting a command means adding it to `rtimer` and to the script. Python quirks are matched on purpose (PyYAML 1.1 quoting, `round()` half to even, `str()`/`int()`/`bool()` coercions); where Rust deliberately differs (it rejects a NaN frame rate), say so in a comment.
- Timing is frame-based: `seconds = frame_delta / source_fps`, using the source clip's frame rate.
- No real footage or personal databases in the repo (`timer_db.yaml` is gitignored). Test fixtures are made up or tiny generated clips.
- **Loose ends go in GitHub issues** (`gh issue create`), referenced from the PR.
