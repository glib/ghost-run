# Releasing and versions

Ghost Run follows [Semantic Versioning 2.0](https://semver.org) and
[Conventional Commits 1.0](https://www.conventionalcommits.org/en/v1.0.0/).
Nobody edits a version number: the **PR title** says what kind of change it
is, the squash merge makes that title the commit on `main` (the repository's
squash commit title is set to "Pull request title"), and after CI
passes there the release job works out the next version from the commits
since the last tag (`scripts/release.py`), builds it and publishes it as a
GitHub release. The app updates itself from those (Settings → Updates).

## Pick the PR title

`<type>[(scope)][!]: <what changed, in plain words>`, e.g.
`feat(videos): build a video from a tag`, `fix(import): a card with no DCIM folder`.

| The change | Type | Next version |
| --- | --- | --- |
| Something you can now do, see or set that you couldn't (a page, a setting, a command, a kind) | `feat` | minor: 1.4.2 → 1.5.0 |
| Something that was wrong now works, including dependency fixes (`fix(deps): ...`) | `fix` | patch: 1.4.2 → 1.4.3 |
| The same, faster or lighter | `perf` | patch |
| Undoing an earlier change | `revert` | patch |
| **Breaking**: an older Ghost Run can't read what this one writes (`timer_db.yaml`, a library schema older builds refuse), a setting, command, flag or file is removed or means something else, or you have to do something by hand after updating | add `!` (`feat!:`, `fix!:`), and say what to do in the PR description | major: 1.4.2 → 2.0.0 |
| Only docs, tests, CI, the build, formatting, tidying, or code moved around with no visible difference | `docs`, `test`, `ci`, `build`, `chore`, `style`, `refactor` | no release (the next one includes it) |

When a PR does several things, title it by the biggest. Scopes are optional
and free-form; use the area (`import`, `library`, `videos`, `tracks`, `time`,
`render`, `resolve`, `strava`, `cli`, `updates`). A schema migration that only
adds and that older builds still open is not breaking. If unsure between
`feat` and `fix`: did the user ask for something new (`feat`) or report
something broken (`fix`)?

The **PR title** check (`.github/workflows/pr-title.yml`) refuses other
titles; `python3 scripts/release.py check-title "feat: ..."` checks one
locally and says what it bumps. Fix a refused title with `gh pr edit <N> --title "..."`.

## What happens on main

1. CI's tests pass on the merge commit.
2. **Next version** runs `scripts/release.py next`: the highest bump since the
   latest `vX.Y.Z` tag (none: `1.0.0`). Commits from before this scheme, or
   anything else that isn't a Conventional Commit, count as a patch. Nothing
   but no-release types: no release, and the job says so.
3. **Build release** builds `ghost-run` and `rtimer` in a
   `registry.fedoraproject.org/fedora:44` container (the Fedora the app's
   distrobox runs: the binary links that box's GTK, libadwaita and glibc) with
   `GHOST_RUN_VERSION` set, checks `ghost-run --version`, and packs
   `ghost-run-X.Y.Z-x86_64-linux.tar.gz` (`bin/`, the icon, the launcher
   template) and `SHA256SUMS`. This job also runs on every PR, as `0.0.0-ci`,
   with the tarball kept for a week as the run's `release` artifact.
4. **Publish release** tags the commit `vX.Y.Z` and makes the GitHub release
   with notes grouped from the commit titles (Breaking changes, New, Fixes…).

Runs on `main` queue one at a time (CI's concurrency group), and each works
from the tags already made, so two quick merges make two releases or one.

When the distroboxes move to a newer Fedora, set the repository variable
`RELEASE_FEDORA` to it (`gh variable set RELEASE_FEDORA --body 45`), after
*every* machine's box has moved: a binary built on a newer Fedora won't start
on an older one. The app tries the new binary before installing it, so a
mismatch fails the update with a message rather than breaking the app.

## The version in a build

`crates/app/build.rs` bakes in `GHOST_RUN_VERSION`: the release's in CI,
otherwise from `git describe` (`1.4.2` on a tagged commit, `1.4.2+3.gabc1234`
three commits on, `.dirty` with uncommitted changes, `0.0.0+dev` without tags).
`ghost-run --version` and Settings → Updates show it. The `version` fields in
the `Cargo.toml` files are not used and stay `0.1.0`.

## Updates in the app

`library::update` + `crates/app/src/updates.rs`. A few seconds after start
and every 6 hours the app asks GitHub for the latest release (only its name
and version in the request). If it's newer:

- **Ask before installing** (default): a banner over the pages; Update… shows
  the notes, then downloads the tarball, checks it against `SHA256SUMS`,
  unpacks it under `~/.local/share/ghost-run/updates/`, runs the new
  `ghost-run --version` to be sure it starts on this machine, and swaps the
  binaries and icon in beside the running ones (written beside, then renamed
  over). Restart… closes the window (an import that's running still asks)
  and starts the new one.
- **Install automatically**: the same without asking, then the Restart banner.
  Only for release builds: a build of your own branch is only offered them.
- **Don't check**: no requests; Check now still works.

Copies run from a Cargo `target` folder don't update themselves (install with
`make install`). The launcher isn't touched by updates; `make install` wrote it
for the machine's box.

## First install on a machine from a release

```bash
# inside the Fedora distrobox
cd "$(mktemp -d)" && gh release download --repo glib/ghost-run --pattern '*.tar.gz' --pattern SHA256SUMS
sha256sum -c SHA256SUMS && tar -xzf ghost-run-*.tar.gz && cd ghost-run-*/
install -Dm755 bin/ghost-run bin/rtimer -t ~/.local/bin
install -Dm644 share/icons/hicolor/scalable/apps/io.github.glib.GhostRun.svg -t ~/.local/share/icons/hicolor/scalable/apps
sed "s|@EXEC@|distrobox enter $CONTAINER_ID -- $HOME/.local/bin/ghost-run|" share/applications/io.github.glib.GhostRun.desktop.in > ~/.local/share/applications/io.github.glib.GhostRun.desktop
```

Or `make install` from a checkout once; updates then come from releases.
