# Resolve Timer

Timing tool for mountain-bike runs: Start/S1…/Finish markers on a clip give
sector and lap times, compared against earlier runs (best lap, optimal lap) and
shown as a video overlay. One user (Liam), public repo, no deploy.

Today it's a Python script inside DaVinci Resolve Studio. It is moving to a
standalone Rust tool (`docs/decisions/0001-standalone-rust-tool.md`): an overlay
rendered to one transparent video file, then markers found automatically from
the video. The Python script stays usable, bug fixes only, until Rust covers the
workflow.

## Commands
- `python -m unittest discover -s tests` — Python tests (needs `PyYAML`, `Pillow`: `pip install -r requirements-resolve.txt`).
- CI (`.github/workflows/ci.yml`) runs them on Python 3.10 and 3.13 on GitHub-hosted runners. CI is the gate.

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
- `src/resolve_timer/` — Python package. Resolve-free core: `timing.py`, `database.py` (YAML, schema v2), `matching.py`, `stats.py`, `markers.py`, `models.py`, `validation.py`, `service.py`, `cli.py`, `summary_card.py`. Resolve-only: `resolve_adapter.py`, `ui.py`, `ui_controller.py`, `runtime_support.py`, and the Fusion part of `overlay.py`.
- `scripts/` — Resolve launcher and installers (Linux davincibox, Windows).
- `tests/` — unittest; Resolve is faked, so everything runs without it.
- `examples/` — example database and markers CSV (made-up runs).
- `docs/decisions/` — short ADRs for significant choices. Add one when you make one.
- `PLAN.md` — the original design and status of the Python tool.

## Conventions
- **The YAML database is the contract** between the Python and Rust tools: schema version 2, same field names and meaning. Never break existing databases; a schema change needs a migration and a version bump.
- Timing is frame-based: `seconds = frame_delta / source_fps`, using the source clip's frame rate.
- No real footage or personal databases in the repo (`timer_db.yaml` is gitignored). Test fixtures are made up or tiny generated clips.
- **Loose ends go in GitHub issues** (`gh issue create`), referenced from the PR.
