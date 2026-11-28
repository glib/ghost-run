# 0006 — Releases and self-update

Status: accepted (2026-11-28)

## Context

Ghost Run runs on two machines (the Bazzite desktop and laptop), each built
from a checkout inside a Fedora distrobox with `git pull && make install`.
Keeping both current by hand is a chore, and agents shipping changes had no
version to name or bump.

## Decision

- **Versions:** SemVer, computed from Conventional Commit PR titles (squash
  merges make them the commits on `main`) by `scripts/release.py`; never
  edited by hand. A CI check enforces the titles. See `docs/releasing.md`.
- **Releases:** after CI passes on `main`, a job builds in a Fedora container
  matching the distroboxes (repository variable `RELEASE_FEDORA`, default 44)
  and publishes a GitHub release with a tarball and `SHA256SUMS`. GitHub-hosted
  runners, as decision 0002.
- **Updates:** the app checks GitHub's latest release, and on yes (or
  automatically, if set) downloads it, checks the hash, runs the new binary's
  `--version` on this machine, then renames it over the old one; a restart
  runs it.

## Alternatives

- A timer on each machine running `git pull && make install`: no release
  infrastructure, but each machine compiles, and failures are only in a log.
- Flatpak: the most Bazzite-native updates, but Ghost Run reaches outside
  itself everywhere (Resolve's box through `distrobox enter`,
  `distrobox-host-exec`, mount tables, `/sys/class/power_supply`, CIFS shares,
  ffmpeg), so it would need `flatpak-spawn --host` throughout.

## Consequences

- The release binary depends on the box's Fedora: bump `RELEASE_FEDORA` only
  once every machine's box is on it. The pre-install `--version` run turns a
  mismatch into a failed update, not a broken app.
- ffmpeg still comes from the box; releases don't bundle it.
- Releases are public, like the repo; they hold no personal data.
