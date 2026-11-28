# 0007 — One catalog for every machine, in Postgres on resolve-db

Status: accepted (2026-11-28). Issue #69; done in #70 (preparation) and the
PR that adds the shared catalog.

## Context

Since 0005 the footage lives on the NAS and each machine keeps a local
catalog: `library.sqlite3` with the clips found, and what only Liam can say
about them (drafts, references, targets, tags, flags, collections, ride names,
features, videos and their usage, Strava). With a laptop as well as the
desktop there were two catalogs that drifted apart, and the nightly copy to
the NAS was named by date only, so whichever machine scanned first each day
took that day's backup.

The resolve-db container (LXC 106) already runs Postgres for DaVinci Resolve
and is backed up with the rest of the host.

## Decision

- **One catalog in Postgres:** a `ghost_run` database and role of their own on
  resolve-db, never tables inside Resolve's databases (Resolve owns those
  schemas and their upgrades). `scripts/catalog-db-setup.sh`, run once as
  root in the container, makes them and lets the LAN in with a password.
- **Same queries, two backends:** all catalog SQL is in the dialect SQLite and
  Postgres share (`library::db`: `$1` parameters, `ON CONFLICT` upserts,
  `RETURNING id`, `lower()`). Only table definitions differ. Without a shared
  catalog set up, everything works as before from `library.sqlite3`.
- **Per machine, still:** each clip's local copy, preview and proxy, and the
  overlays rendered, are files on one machine. They stay in
  `library.machine.sqlite3` on each machine, shared catalog or not (schema 12).
- **Paths that agree:** clips under the NAS footage root are stored by their
  path in it, so machines that mount the NAS in different places agree. Other
  clips carry their machine's name, and only that machine lists or forgets
  them. Tags and the like were already keyed by file name and size.
- **Away from the server:** each machine keeps `library.sqlite3` as a copy of
  the shared catalog, refreshed when it has changed (triggers count changes)
  every two minutes and after each scan. When the server can't be reached the
  Library opens that copy read-only, with a banner, and new clips wait. Edits
  made offline aren't queued: that would need merge rules for very little.
- **Joining:** Settings > Shared catalog. The first machine fills the empty
  shared catalog from its own (the desktop's, decided), and the others join
  it, their own catalog put aside as `library-before-shared-<date>.sqlite3`.
  Stop sharing makes the machine's copy its own catalog again.
- **Credentials:** server, database, user and password in
  `<data dir>/shared-catalog.json` (0600), never in settings.toml, as with
  Strava's tokens. No TLS: the server is on the LAN.
- **Backups:** Ghost Run doesn't back up the shared catalog: the container is
  backed up. Each machine's backups (run database, and its own catalog while
  it isn't shared) are named `<kind>-<machine>-<date>`, 30 kept per machine.

## Consequences

- Both machines see the same drafts, tags, videos and targets while at home.
  The laptop away from home can time and view, but can't change the catalog.
- CI runs the shared catalog's tests against Postgres 13 in a service
  container. Locally they're skipped without `GHOST_RUN_TEST_POSTGRES`.
- A newer Ghost Run's schema refuses to be opened by an older one, on either
  backend. Schema changes now need a Postgres definition as well as the
  SQLite migration.
- Strava disconnect clears the shared catalog's Strava data for every machine.
