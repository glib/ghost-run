//! New releases in the app (`library::update`, `docs/releasing.md`): a
//! check a few seconds after start and every 6 hours, a banner over the
//! pages when one is out (Update… shows what's new and installs it), a
//! Restart once it's in, and the Settings group (version, what to do about
//! releases, Check now). Settings → Updates: ask first (the default), install
//! automatically (a build of your own branch is still only offered them),
//! or don't check.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use library::settings::UpdatePolicy;
use library::update::{self, Release};
use semver::Version;

use crate::shared::{Change, Shared};

const FIRST_CHECK: Duration = Duration::from_secs(5);
const EVERY: u32 = 6 * 3600;

/// Where updates stand.
#[derive(Clone, Default)]
enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing(Version),
    /// Installed at this path; a restart runs it.
    Installed(Version, PathBuf),
    /// What failed, and the release's page when there was one.
    Failed(String, Option<String>),
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

fn state() -> State {
    STATE.with_borrow(|s| s.clone())
}

fn set(shared: &Shared, state: State) {
    STATE.set(state);
    shared.notify(Change::Update);
}

/// This build's version (`build.rs`).
pub fn version() -> Version {
    update::parse_version(env!("GHOST_RUN_VERSION"))
}

/// Where this copy is installed, if it can update itself there.
fn install_dir() -> Option<PathBuf> {
    update::install_dir(&std::env::current_exe().ok()?)
}

/// Checks soon after start, then every 6 hours, unless updates are off.
pub fn start(shared: &Shared) {
    let s = shared.clone();
    glib::timeout_add_local_once(FIRST_CHECK, move || check(&s, false));
    let s = shared.clone();
    glib::timeout_add_seconds_local(EVERY, move || {
        check(&s, false);
        glib::ControlFlow::Continue
    });
}

/// Asks GitHub for a newer release. `by_hand` (Check now) also checks with
/// updates off, and says why a check failed; otherwise failures (offline)
/// are only logged.
pub fn check(shared: &Shared, by_hand: bool) {
    let policy = shared.settings().updates;
    if (policy == UpdatePolicy::Off && !by_hand)
        || install_dir().is_none()
        || matches!(
            state(),
            State::Checking | State::Installing(_) | State::Installed(..)
        )
    {
        return;
    }
    set(shared, State::Checking);
    let shared = shared.clone();
    crate::strava_ui::in_background(
        || update::check(&update::base(), &version()),
        move |result| match result {
            Ok(Some(release)) => {
                let automatic = shared.settings().updates == UpdatePolicy::Automatic
                    && update::is_release_build(&version());
                if automatic && !by_hand {
                    install(&shared, release);
                } else {
                    set(&shared, State::Available(release));
                }
            }
            Ok(None) => set(&shared, State::UpToDate),
            Err(e) if by_hand => set(&shared, State::Failed(format!("{e:#}"), None)),
            Err(e) => {
                eprintln!("checking for updates: {e:#}");
                set(&shared, State::Idle);
            }
        },
    );
}

/// Downloads, checks and installs `release` in the background.
fn install(shared: &Shared, release: Release) {
    set(shared, State::Installing(release.version.clone()));
    let shared = shared.clone();
    let page = release.page.clone();
    crate::strava_ui::in_background(
        move || -> Result<(Version, PathBuf)> {
            let bin = install_dir().context("this copy can't update itself")?;
            let staging = library::settings::data_dir().join("updates");
            let staged = update::download(&release, &version(), &staging)?;
            update::try_run(&staged, &release.version)?;
            update::install(&staged, &bin)?;
            let _ = std::fs::remove_dir_all(&staging);
            Ok((release.version, bin.join("ghost-run")))
        },
        move |result| match result {
            Ok((v, exe)) => set(&shared, State::Installed(v, exe)),
            Err(e) => set(&shared, State::Failed(format!("{e:#}"), Some(page))),
        },
    );
}

/// The banner over the pages.
pub fn banner(shared: &Shared) -> adw::Banner {
    let banner = adw::Banner::new("");
    let show = {
        let banner = banner.clone();
        move || {
            let (title, button) = match state() {
                State::Available(r) => (format!("Ghost Run {} is out", r.version), Some("Update…")),
                State::Installing(v) => (format!("Updating to {v}…"), None),
                State::Installed(v, _) => (
                    format!("Ghost Run {v} is installed: restart to use it"),
                    Some("Restart…"),
                ),
                State::Failed(e, Some(_)) => {
                    (format!("Couldn't update: {e}"), Some("Release page"))
                }
                _ => {
                    banner.set_revealed(false);
                    return;
                }
            };
            banner.set_title(&glib::markup_escape_text(&title));
            banner.set_button_label(button);
            banner.set_revealed(true);
        }
    };
    show();
    shared.subscribe(move |change| {
        if change == Change::Update {
            show();
        }
    });
    let shared = shared.clone();
    banner.connect_button_clicked(move |_| match state() {
        State::Available(r) => confirm(&shared, r),
        State::Installed(_, exe) => restart(&shared, exe),
        State::Failed(_, Some(page)) => {
            if let Err(e) = crate::desktop::open_url(&page) {
                eprintln!("couldn't open {page}: {e}");
            }
            set(&shared, State::Idle);
        }
        _ => {}
    });
    banner
}

/// What's new, and Update / Not now.
fn confirm(shared: &Shared, release: Release) {
    let mut notes = update::plain_notes(&release.notes);
    if notes.chars().count() > 1500 {
        notes = notes.chars().take(1500).collect::<String>() + "…";
    }
    if notes.is_empty() {
        notes = "No notes for this release.".into();
    }
    let dialog = adw::AlertDialog::new(
        Some(&format!("Update to Ghost Run {}?", release.version)),
        Some(&format!(
            "You have {}. What's new:\n\n{notes}\n\nIt's downloaded, checked, and installed beside this one; restart Ghost Run afterwards to use it.",
            version()
        )),
    );
    dialog.add_responses(&[("later", "Not now"), ("update", "Update")]);
    dialog.set_response_appearance("update", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("update"));
    dialog.set_close_response("later");
    let shared2 = shared.clone();
    dialog.connect_response(None, move |_, response| {
        if response == "update" {
            install(&shared2, release.clone());
        }
    });
    dialog.present(Some(&shared.window));
}

/// Closes the window (which still asks about an import that's running) and
/// starts `exe` once this process has gone.
fn restart(shared: &Shared, exe: PathBuf) {
    let dialog = adw::AlertDialog::new(
        Some("Restart Ghost Run?"),
        Some("Renders and copies that are running stop. An import that's running asks first."),
    );
    dialog.add_responses(&[("later", "Not now"), ("restart", "Restart")]);
    dialog.set_response_appearance("restart", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("later");
    let shared2 = shared.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "restart" {
            return;
        }
        use std::os::unix::process::CommandExt;
        let spawned = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"while kill -0 "$1" 2>/dev/null; do sleep 0.3; done; exec "$0""#)
            .arg(&exe)
            .arg(std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .process_group(0)
            .spawn();
        match spawned {
            Ok(_) => shared2.window.close(),
            Err(e) => set(
                &shared2,
                State::Failed(format!("couldn't restart: {e}"), None),
            ),
        }
    });
    dialog.present(Some(&shared.window));
}

/// The Settings group.
pub fn group(shared: &Shared, subscriptions: &mut Vec<usize>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Updates");
    group.set_description(Some(&format!(
        "This is Ghost Run {}. New releases come from GitHub ({}).",
        version(),
        update::REPO
    )));
    let policy = adw::ComboRow::new();
    policy.set_title("New releases");
    policy.set_model(Some(&gtk::StringList::new(&[
        "Ask before installing",
        "Install automatically",
        "Don't check",
    ])));
    policy.set_selected(match shared.settings().updates {
        UpdatePolicy::Ask => 0,
        UpdatePolicy::Automatic => 1,
        UpdatePolicy::Off => 2,
    });
    let shared2 = shared.clone();
    policy.connect_selected_notify(move |row| {
        let choice = match row.selected() {
            1 => UpdatePolicy::Automatic,
            2 => UpdatePolicy::Off,
            _ => UpdatePolicy::Ask,
        };
        if shared2.settings().updates != choice {
            let _ = shared2.update(|s| s.updates = choice);
        }
    });
    group.add(&policy);

    let row = adw::ActionRow::builder().title("Check for updates").build();
    let button = gtk::Button::with_label("Check now");
    button.set_valign(gtk::Align::Center);
    row.add_suffix(&button);
    let show = {
        let (row, button) = (row.clone(), button.clone());
        move || {
            let built = install_dir().is_none();
            let text = if built {
                "This copy runs from a build folder; install it with make install to get updates."
                    .to_owned()
            } else {
                match state() {
                    State::Idle => "Checks a few seconds after start and every 6 hours.".into(),
                    State::Checking => "Checking…".into(),
                    State::UpToDate => "Up to date.".into(),
                    State::Available(r) => {
                        format!(
                            "Ghost Run {} is out: Update… is at the top of the window.",
                            r.version
                        )
                    }
                    State::Installing(v) => format!("Updating to {v}…"),
                    State::Installed(v, _) => format!("{v} is installed: restart to use it."),
                    State::Failed(e, _) => format!("Couldn't update: {e}"),
                }
            };
            row.set_subtitle(&glib::markup_escape_text(&text));
            button.set_sensitive(
                !built
                    && !matches!(
                        state(),
                        State::Checking | State::Installing(_) | State::Installed(..)
                    ),
            );
        }
    };
    show();
    subscriptions.push(shared.subscribe(move |change| {
        if change == Change::Update {
            show();
        }
    }));
    let shared2 = shared.clone();
    button.connect_clicked(move |_| check(&shared2, true));
    group.add(&row);
    group
}
