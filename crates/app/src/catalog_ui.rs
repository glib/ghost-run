//! The shared catalog in the app (#69, `library::shared`): connecting from
//! Settings, filling the shared catalog from this machine or joining one
//! another machine filled, stopping, and keeping this machine's copy (what
//! the Library opens read-only while the server can't be reached) up to
//! date in the background.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use library::shared::{self, Config, Started, Summary};

use crate::shared::{Change, Shared};

/// How often this machine's copy is checked against the shared catalog.
const COPY_EVERY: Duration = Duration::from_secs(120);

/// Runs `work` on a thread and hands its answer to `done` on the main thread.
fn in_background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let mut done = Some(done);
    glib::timeout_add_local(Duration::from_millis(150), move || match rx.try_recv() {
        Ok(answer) => {
            if let Some(done) = done.take() {
                done(answer);
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

fn library_path() -> PathBuf {
    library::settings::data_dir().join("library.sqlite3")
}

/// The NAS footage root, as the library uses it.
fn nas() -> Option<PathBuf> {
    ingest::settings::Settings::load()
        .ok()
        .and_then(|s| s.archive)
        .map(|a| a.path)
}

/// Brings this machine's copy up to date if the catalog is shared and has
/// changed. Quiet: a failure (the server out of reach) only goes to the log.
pub fn refresh_copy_quietly() {
    if let Ok(Some(cfg)) = Config::load()
        && let Err(e) = shared::refresh_copy(&cfg, &library_path(), false)
    {
        eprintln!("Shared catalog: this machine's copy wasn't refreshed: {e:#}");
    }
}

/// Keeps this machine's copy up to date while the app runs: now, then every
/// [`COPY_EVERY`].
pub fn keep_copy() {
    std::thread::spawn(|| {
        loop {
            refresh_copy_quietly();
            std::thread::sleep(COPY_EVERY);
        }
    });
}

/// The Settings group.
pub fn group(shared: &Shared) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Shared catalog");
    group.set_description(Some(
        "Keep the catalog (clips, drafts, tags, collections, videos, targets, Strava) in a Postgres database every machine uses, such as a ghost_run database on the resolve-db container (scripts/catalog-db-setup.sh makes one). Each machine keeps a copy, opened read-only while the server can't be reached. Local copies, previews, proxies and overlays stay each machine's own. The password is kept in shared-catalog.json in Ghost Run's data folder, readable only by you.",
    ));
    let rows: Rows = Default::default();
    fill(shared, &group, &rows, None);
    group
}

type Rows = std::rc::Rc<std::cell::RefCell<Vec<gtk::Widget>>>;

/// (Re)fills the group for shared or not, with `note` in its status row.
fn fill(shared: &Shared, group: &adw::PreferencesGroup, rows: &Rows, note: Option<String>) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    let add = |row: gtk::Widget| {
        group.add(&row);
        rows.borrow_mut().push(row);
    };
    match Config::load() {
        Ok(Some(cfg)) => {
            let row = adw::ActionRow::builder()
                .title(format!("Shared: {}", cfg.describe()))
                .subtitle(note.unwrap_or_else(|| "Checking the server…".into()))
                .subtitle_lines(3)
                .build();
            let stop = gtk::Button::with_label("Stop sharing");
            stop.set_valign(gtk::Align::Center);
            row.add_suffix(&stop);
            add(row.clone().upcast());
            let w = row.clone();
            let cfg2 = cfg.clone();
            in_background(
                move || {
                    shared::retry_now();
                    shared::reachable(&cfg2)
                },
                move |r: Result<()>| {
                    if w.subtitle().is_some_and(|s| s == "Checking the server…") {
                        w.set_subtitle(&match r {
                            Ok(()) => "Connected. This machine keeps a copy for when it isn't."
                                .to_owned(),
                            Err(e) => format!(
                                "Can't reach it now: the Library shows this machine's copy, read-only. ({e:#})"
                            ),
                        });
                    }
                },
            );
            let (s, g, r) = (shared.clone(), group.clone(), rows.clone());
            stop.connect_clicked(move |_| confirm_stop(&s, &g, &r, cfg.clone()));
        }
        Ok(None) | Err(_) => {
            let defaults = Config::default();
            let host = adw::EntryRow::builder().title("Server").build();
            let port = adw::EntryRow::builder()
                .title("Port")
                .text(defaults.port.to_string())
                .build();
            let database = adw::EntryRow::builder()
                .title("Database")
                .text(&defaults.database)
                .build();
            let user = adw::EntryRow::builder()
                .title("User")
                .text(&defaults.user)
                .build();
            let password = adw::PasswordEntryRow::builder().title("Password").build();
            let row = adw::ActionRow::builder()
                .title("Not shared")
                .subtitle(note.unwrap_or_else(|| {
                    "This machine keeps its own catalog in library.sqlite3.".into()
                }))
                .subtitle_lines(3)
                .build();
            let connect = gtk::Button::with_label("Connect…");
            connect.set_valign(gtk::Align::Center);
            connect.add_css_class("suggested-action");
            row.add_suffix(&connect);
            for r in [
                host.clone().upcast::<gtk::Widget>(),
                port.clone().upcast(),
                database.clone().upcast(),
                user.clone().upcast(),
                password.clone().upcast(),
                row.clone().upcast(),
            ] {
                add(r);
            }
            let (s, g, r, w) = (shared.clone(), group.clone(), rows.clone(), row.clone());
            connect.connect_clicked(move |b| {
                let port = match port.text().trim().parse::<u16>() {
                    Ok(p) => p,
                    Err(_) => {
                        w.set_subtitle("The port is a number (Postgres's is 5432).");
                        return;
                    }
                };
                let cfg = Config {
                    host: host.text().trim().to_owned(),
                    port,
                    database: database.text().trim().to_owned(),
                    user: user.text().trim().to_owned(),
                    password: password.text().to_string(),
                };
                b.set_sensitive(false);
                w.set_subtitle(&format!("Connecting to {}…", cfg.describe()));
                let (s, g, r, w, b) = (s.clone(), g.clone(), r.clone(), w.clone(), b.clone());
                let cfg2 = cfg.clone();
                in_background(
                    move || -> Result<(Option<Summary>, Summary)> {
                        let remote = shared::remote(&cfg2)?;
                        let local = shared::local(&library_path(), nas())?;
                        Ok((remote, local))
                    },
                    move |result| {
                        b.set_sensitive(true);
                        match result {
                            Ok((remote, local)) => confirm_start(&s, &g, &r, cfg, remote, local),
                            Err(e) => w.set_subtitle(&format!("Couldn't connect: {e:#}")),
                        }
                    },
                );
            });
        }
    }
}

/// Asks before filling an empty shared catalog from this machine, or before
/// joining a filled one.
fn confirm_start(
    shared: &Shared,
    group: &adw::PreferencesGroup,
    rows: &Rows,
    cfg: Config,
    remote: Option<Summary>,
    local: Summary,
) {
    let (heading, body, label, copy_in) = match &remote {
        None => (
            "Fill the shared catalog from this machine?".to_owned(),
            format!(
                "{} is empty. This machine's catalog ({}) is copied into it, and from then on every machine sharing it sees the same clips, drafts, tags and videos.\n\nDo this on the machine whose catalog you want to keep.",
                cfg.describe(),
                local.text()
            ),
            "Copy it in",
            true,
        ),
        Some(summary) => {
            let from = summary
                .filled_by
                .as_ref()
                .map(|(by, at)| format!(" (filled from {by}, {at})"))
                .unwrap_or_default();
            (
                "Use the shared catalog?".to_owned(),
                format!(
                    "{} has {}{from}. This machine's own catalog ({}) is put aside as library-before-shared-<date>.sqlite3 and no longer used.",
                    cfg.describe(),
                    summary.text(),
                    local.text()
                ),
                "Use the shared catalog",
                false,
            )
        }
    };
    let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
    dialog.add_responses(&[("cancel", "Cancel"), ("go", label)]);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let (s, g, r) = (shared.clone(), group.clone(), rows.clone());
    dialog.connect_response(None, move |_, response| {
        if response != "go" {
            return;
        }
        fill_busy(&g, &r, "Setting up the shared catalog…");
        let (s, g, r, cfg) = (s.clone(), g.clone(), r.clone(), cfg.clone());
        in_background(
            move || {
                shared::start(
                    &cfg,
                    &library_path(),
                    nas(),
                    copy_in,
                    &shared::config_path(),
                )
            },
            move |result| {
                let note = match result {
                    Ok(Started::CopiedIn(rows)) => format!(
                        "This machine's catalog was copied in ({rows} rows). Other machines can join it now."
                    ),
                    Ok(Started::Joined(Some(aside))) => format!(
                        "Joined. This machine's own catalog was put aside as {}.",
                        aside.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    Ok(Started::Joined(None)) => "Joined.".to_owned(),
                    Err(e) => format!("Couldn't start sharing: {e:#}"),
                };
                fill(&s, &g, &r, Some(note));
                s.notify(Change::Catalog);
            },
        );
    });
    dialog.present(Some(&shared.window));
}

/// Asks before going back to a catalog of this machine's own.
fn confirm_stop(shared: &Shared, group: &adw::PreferencesGroup, rows: &Rows, cfg: Config) {
    let dialog = adw::AlertDialog::new(
        Some("Stop sharing the catalog?"),
        Some(&format!(
            "This machine goes back to a catalog of its own: its copy of {} (brought up to date first if the server can be reached). The shared catalog stays as it is for other machines.",
            cfg.describe()
        )),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("stop", "Stop sharing")]);
    dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let (s, g, r) = (shared.clone(), group.clone(), rows.clone());
    dialog.connect_response(None, move |_, response| {
        if response != "stop" {
            return;
        }
        fill_busy(&g, &r, "Stopping…");
        let (s, g, r, cfg) = (s.clone(), g.clone(), r.clone(), cfg.clone());
        in_background(
            move || shared::stop(&cfg, &library_path(), &shared::config_path()),
            move |result| {
                let note = match result {
                    Ok(true) => "Stopped sharing: this machine's catalog is its copy, up to date.".to_owned(),
                    Ok(false) => "Stopped sharing: this machine's catalog is its last copy (the server couldn't be reached to bring it up to date).".to_owned(),
                    Err(e) => format!("Couldn't stop sharing: {e:#}"),
                };
                fill(&s, &g, &r, Some(note));
                s.notify(Change::Catalog);
            },
        );
    });
    dialog.present(Some(&shared.window));
}

/// One row saying what's happening, while it happens.
fn fill_busy(group: &adw::PreferencesGroup, rows: &Rows, text: &str) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    let row = adw::ActionRow::builder().title(text).build();
    let spinner = gtk::Spinner::new();
    spinner.start();
    row.add_suffix(&spinner);
    group.add(&row);
    rows.borrow_mut().push(row.upcast());
}
