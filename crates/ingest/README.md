# ingest

The core of Ghost Run's Import page, formerly the separate
[`glib/resolve-ingest`](https://github.com/glib/resolve-ingest) app (merged here
with its history): verified copies of camera cards to a CIFS archive, and MOV
editing copies with unchanged video and 24-bit PCM audio, journalled in SQLite.
The desktop UI is in `crates/app`; the headless commands are
`ghost-run plan|ingest|retry|history|report|cleanup|doctor`.

How it works, what has been validated and what hasn't: [docs/ingest](../../docs/ingest/import.md).
MIT licensed (`LICENSE`).
