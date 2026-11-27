//! Strava in the app (decision 0005, `library::strava`): connecting your own
//! Strava API app from Settings, and syncing, by hand or after a Library
//! scan (at most hourly). The keys and tokens live in `strava.json` in the
//! data folder, never in the settings file.

use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use library::Library;
use library::strava::{self, Client};

use crate::shared::{Change, Shared};

/// How long Strava's page waits for you to say yes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(300);
/// The least time between automatic syncs.
const AUTO_EVERY: Duration = Duration::from_secs(3600);

/// When the last sync started, and whether one is running.
static LAST: Mutex<(Option<Instant>, bool)> = Mutex::new((None, false));

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
    glib::timeout_add_local(Duration::from_millis(200), move || match rx.try_recv() {
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

/// Strava, or a stand-in for screenshots and trying it out
/// (`GHOST_RUN_STRAVA=http://127.0.0.1:PORT`).
fn base() -> String {
    std::env::var("GHOST_RUN_STRAVA").unwrap_or_else(|_| strava::STRAVA.to_owned())
}

fn client() -> Result<Option<Client>> {
    Client::open(&base(), &strava::tokens_path())
}

/// The athlete's name, if connected.
fn connected() -> Option<String> {
    client().ok().flatten().map(|c| c.athlete().to_owned())
}

fn sync_now() -> Result<String> {
    let mut client = client()?.context("Strava isn't connected")?;
    let library = Library::open_default()?;
    let clips = library.clips()?;
    let report = strava::sync(&library, &mut client, &clips, strava::DETAIL_BUDGET)?;
    Ok(report.text())
}

/// Syncs in the background unless one is running; `done` gets what it did.
fn sync(shared: &Shared, done: impl FnOnce(Result<String>) + 'static) {
    {
        let mut last = LAST.lock().unwrap();
        if last.1 {
            done(Ok("Strava: already syncing.".into()));
            return;
        }
        *last = (Some(Instant::now()), true);
    }
    let shared = shared.clone();
    in_background(sync_now, move |result| {
        LAST.lock().unwrap().1 = false;
        if result.is_ok() {
            shared.notify(Change::Strava);
        }
        done(result);
    });
}

/// After a Library scan: syncs if connected and the last sync was over an
/// hour ago. Quiet: a failure only goes to the log.
pub fn auto_sync(shared: &Shared) {
    let due = LAST
        .lock()
        .unwrap()
        .0
        .is_none_or(|t| t.elapsed() >= AUTO_EVERY);
    if due && strava::tokens_path().exists() {
        sync(shared, |result| {
            if let Err(e) = result {
                eprintln!("Strava sync: {e:#}");
            }
        });
    }
}

/// The Settings group.
pub fn group(shared: &Shared) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Strava");
    group.set_description(Some(
        "Rides from Strava name your clips, suggest their courses, and give captions their stats. Use your own Strava API app (strava.com/settings/api) with its Authorization Callback Domain set to localhost. Its keys and the access Strava gives stay in strava.json in Ghost Run's data folder; activities are kept in the catalog.",
    ));
    let rows: std::rc::Rc<std::cell::RefCell<Vec<gtk::Widget>>> = Default::default();
    fill(shared, &group, &rows, None);
    group
}

type Rows = std::rc::Rc<std::cell::RefCell<Vec<gtk::Widget>>>;

/// (Re)fills the group for connected or not, with `note` under it.
fn fill(shared: &Shared, group: &adw::PreferencesGroup, rows: &Rows, note: Option<String>) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    let add = |row: gtk::Widget| {
        group.add(&row);
        rows.borrow_mut().push(row);
    };
    match connected() {
        Some(athlete) => {
            let row =
                adw::ActionRow::builder()
                    .title(format!("Connected as {athlete}"))
                    .subtitle(note.unwrap_or_else(|| {
                        "Syncs after a Library scan, at most once an hour.".into()
                    }))
                    .build();
            let sync_button = gtk::Button::with_label("Sync now");
            sync_button.set_valign(gtk::Align::Center);
            let disconnect = gtk::Button::with_label("Disconnect");
            disconnect.set_valign(gtk::Align::Center);
            disconnect.add_css_class("destructive-action");
            row.add_suffix(&sync_button);
            row.add_suffix(&disconnect);
            let (s, g, r, w) = (shared.clone(), group.clone(), rows.clone(), row.clone());
            sync_button.connect_clicked(move |b| {
                b.set_sensitive(false);
                w.set_subtitle("Syncing…");
                let (s2, g, r) = (s.clone(), g.clone(), r.clone());
                sync(&s, move |result| {
                    let note = match result {
                        Ok(text) => text,
                        Err(e) => format!("Strava couldn't be synced: {e:#}"),
                    };
                    fill(&s2, &g, &r, Some(note));
                });
            });
            let (s, g, r) = (shared.clone(), group.clone(), rows.clone());
            disconnect.connect_clicked(move |_| {
                let forget = || -> Result<()> {
                    if let Some(c) = client()? {
                        c.disconnect()?;
                    }
                    Library::open_default()?.clear_strava()
                };
                let note = match forget() {
                    Ok(()) => "Disconnected: Strava's activities are gone from the catalog.".into(),
                    Err(e) => format!("Couldn't disconnect: {e:#}"),
                };
                s.notify(Change::Strava);
                fill(&s, &g, &r, Some(note));
            });
            add(row.upcast());
        }
        None => {
            let id = adw::EntryRow::builder().title("Client ID").build();
            let secret = adw::PasswordEntryRow::builder()
                .title("Client secret")
                .build();
            let row = adw::ActionRow::builder()
                .title("Not connected")
                .subtitle(note.unwrap_or_else(|| {
                    "Connect opens Strava in your browser to ask for your rides.".into()
                }))
                .build();
            let connect = gtk::Button::with_label("Connect");
            connect.set_valign(gtk::Align::Center);
            connect.add_css_class("suggested-action");
            row.add_suffix(&connect);
            let (s, g, r, w) = (shared.clone(), group.clone(), rows.clone(), row.clone());
            let (id2, secret2) = (id.clone(), secret.clone());
            connect.connect_clicked(move |b| {
                b.set_sensitive(false);
                w.set_subtitle("Waiting for Strava in your browser…");
                let (client_id, client_secret) =
                    (id2.text().to_string(), secret2.text().to_string());
                let (s, g, r) = (s.clone(), g.clone(), r.clone());
                in_background(
                    move || -> Result<()> {
                        let tokens = strava::connect(
                            &base(),
                            &client_id,
                            &client_secret,
                            CONNECT_TIMEOUT,
                            |url| {
                                if let Err(e) = crate::desktop::open_url(url) {
                                    eprintln!("couldn't open the browser: {e}; open {url}");
                                }
                            },
                        )?;
                        tokens.save(&strava::tokens_path())
                    },
                    move |result| match result {
                        Ok(()) => {
                            fill(&s, &g, &r, Some("Connected. Syncing…".into()));
                            let (s2, g2, r2) = (s.clone(), g.clone(), r.clone());
                            sync(&s, move |result| {
                                let note = match result {
                                    Ok(text) => text,
                                    Err(e) => format!("Strava couldn't be synced: {e:#}"),
                                };
                                fill(&s2, &g2, &r2, Some(note));
                            });
                        }
                        Err(e) => fill(&s, &g, &r, Some(format!("Not connected: {e:#}"))),
                    },
                );
            });
            add(id.upcast());
            add(secret.upcast());
            add(row.upcast());
        }
    }
}
