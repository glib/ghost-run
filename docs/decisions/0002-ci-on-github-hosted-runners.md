# 0002 — CI on GitHub-hosted runners

Status: accepted (2026-11-23)

## Context

Home OS runs CI on self-hosted runners on the desktop because hosted minutes
for private repos ran out. Those runners are registered to `glib/home-os` only,
and the devbox's wake-the-desktop logic only watches that repo.

This repo is public. On public repos, GitHub-hosted runners are free without a
minute limit, and self-hosted runners are unsafe: a pull request from a fork
could run its code on the machine.

## Decision

CI runs on `ubuntu-latest`. No self-hosted runners for this repo while it is
public. A green PR is the gate for merging, as in Home OS.

## Consequences

- No desktop setup or wake-up logic needed.
- If the repo goes private, revisit: either pay for minutes or register
  separate desktop runners for it (generalising `scripts/desktop-ci-runners.sh`
  in Home OS).
- Jobs that need real hardware (UWB kits) can't run here; they belong in the
  separate hardware repo.
