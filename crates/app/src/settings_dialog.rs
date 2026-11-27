//! The app's settings: footage folders, the run database, the Resolve
//! template, and where imports go.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use library::settings::AppSettings;

use crate::shared::Shared;

pub fn open(shared: &Shared) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Settings");
    let page = adw::PreferencesPage::new();
    dialog.add(&page);

    let folders = adw::PreferencesGroup::new();
    folders.set_title("Footage folders");
    folders.set_description(Some(
        "Every video in these folders and their subfolders is listed in the Library. Proxy, cache and backup folders are skipped.",
    ));
    let add = gtk::Button::with_label("Add folder…");
    add.set_valign(gtk::Align::Center);
    folders.set_header_suffix(Some(&add));
    page.add(&folders);
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let fill = {
        let (shared, folders, rows) = (shared.clone(), folders.clone(), rows.clone());
        Rc::new(move || {
            for row in rows.borrow_mut().drain(..) {
                folders.remove(&row);
            }
            let list = shared.settings().footage_folders.clone();
            if list.is_empty() {
                let row = adw::ActionRow::builder().title("No folders yet").build();
                folders.add(&row);
                rows.borrow_mut().push(row);
            }
            for path in list {
                let row = adw::ActionRow::builder()
                    .title(
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .as_ref(),
                    )
                    .subtitle(path.display().to_string())
                    .build();
                let remove = gtk::Button::from_icon_name("list-remove-symbolic");
                remove.set_valign(gtk::Align::Center);
                remove.set_tooltip_text(Some("Stop listing this folder (nothing is deleted)"));
                remove.add_css_class("flat");
                row.add_suffix(&remove);
                let shared = shared.clone();
                remove.connect_clicked(move |_| {
                    let _ = shared.update(|s| s.footage_folders.retain(|f| f != &path));
                });
                folders.add(&row);
                rows.borrow_mut().push(row);
            }
        })
    };
    fill();
    let refill = fill.clone();
    let mut subscriptions = vec![shared.subscribe(move |_| refill())];
    let shared2 = shared.clone();
    add.connect_clicked(move |_| add_folder(&shared2));

    let files = adw::PreferencesGroup::new();
    files.set_title("Files");
    page.add(&files);
    files.add(&file_row(
        shared,
        &mut subscriptions,
        "Run database",
        "timer_db.yaml: your courses and stored runs.",
        |s| s.run_database.clone(),
        |s, v| s.run_database = v,
        ("YAML", &["*.yaml", "*.yml"]),
    ));
    files.add(&file_row(
        shared,
        &mut subscriptions,
        "Resolve project template",
        "A .drp exported from Resolve. Not used yet: new Resolve projects will start from it.",
        |s| s.resolve_template.clone(),
        |s, v| s.resolve_template = v,
        ("Resolve project", &["*.drp"]),
    ));
    files.add(&file_row(
        shared,
        &mut subscriptions,
        "Overlay folder",
        "Where rendered overlays go. Not set: an Overlays folder beside each clip.",
        |s| s.overlay_folder.clone(),
        |s, v| s.overlay_folder = v,
        ("Folder", &[]),
    ));
    files.add(&file_row(
        shared,
        &mut subscriptions,
        "Video kinds folder",
        "Your own kinds (*.toml, like the examples in Ghost Run's kinds folder), replacing the built-in ones with the same id. Not set: the six built-in kinds.",
        |s| s.kinds_folder.clone(),
        |s, v| s.kinds_folder = v,
        ("Folder", &[]),
    ));
    files.add(&file_row(
        shared,
        &mut subscriptions,
        "Brand kit",
        "A brand.toml: title, end and chart cards and captions take its colours, fonts and logos. Not set: Ghost Run's own look.",
        |s| s.brand_kit.clone(),
        |s, v| s.brand_kit = v,
        ("Brand kit", &["brand.toml", "*.toml"]),
    ));
    let import = adw::PreferencesGroup::new();
    import.set_title("Import");
    import.set_description(Some(
        "Where the Import page puts a card's clips: each original, checked byte for byte, and a copy to edit.",
    ));
    page.add(&import);
    import.add(&import_row(
        shared,
        &mut subscriptions,
        "Originals",
        "A folder on your NAS share (an SMB/CIFS mount). The only copy once the card is wiped.",
        true,
    ));
    import.add(&import_row(
        shared,
        &mut subscriptions,
        "Editing copies",
        "A local folder: copies Resolve edits smoothly, kept in the same folders as the originals.",
        false,
    ));
    let limit = adw::SpinRow::with_range(5.0, 5000.0, 5.0);
    limit.set_title("Suggest removing local copies past (GB)");
    limit.set_subtitle(
        "Local copies of NAS clips are for editing. Past this, the Library suggests removing the ones you're done with.",
    );
    limit.set_value(f64::from(
        shared
            .settings()
            .local_copies_gb
            .unwrap_or(library::settings::LOCAL_COPIES_GB),
    ));
    let shared2 = shared.clone();
    limit.connect_value_notify(move |row| {
        let gb = row.value().round() as u32;
        if shared2
            .settings()
            .local_copies_gb
            .unwrap_or(library::settings::LOCAL_COPIES_GB)
            != gb
        {
            let _ = shared2.update(|s| s.local_copies_gb = Some(gb));
        }
    });
    import.add(&limit);
    let resolve = adw::PreferencesGroup::new();
    resolve.set_title("DaVinci Resolve");
    resolve.set_description(Some(
        "Ghost Run reaches Resolve through its scripting API: Resolve Studio, running, with Preferences > System > General > External scripting using set to Local.",
    ));
    page.add(&resolve);
    let container = adw::EntryRow::builder()
        .title("Distrobox Resolve is in (empty: davincibox)")
        .text(
            shared
                .settings()
                .resolve_container
                .clone()
                .unwrap_or_default(),
        )
        .show_apply_button(true)
        .build();
    let shared2 = shared.clone();
    container.connect_apply(move |row| {
        let text = row.text().trim().to_owned();
        let _ = shared2.update(|s| {
            s.resolve_container = (!text.is_empty()).then_some(text);
        });
    });
    resolve.add(&container);
    page.add(&crate::strava_ui::group(shared));
    let shared2 = shared.clone();
    dialog.connect_closed(move |_| {
        for id in &subscriptions {
            shared2.unsubscribe(*id);
        }
    });
    dialog.present(Some(&shared.window));
}

/// Asks for a folder and adds it to the footage folders.
pub fn add_folder(shared: &Shared) {
    let dialog = gtk::FileDialog::builder()
        .title("Add a footage folder")
        .build();
    let shared = shared.clone();
    dialog.select_folder(
        Some(&shared.window.clone()),
        gio::Cancellable::NONE,
        move |r| {
            let Ok(folder) = r else { return };
            if let Some(path) = folder.path() {
                let _ = shared.update(|s| {
                    if !s.footage_folders.contains(&path) {
                        s.footage_folders.push(path);
                    }
                });
            }
        },
    );
}

/// Asks for the run database (an existing one, or a new file name).
pub fn choose_database(shared: &Shared, create: bool) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("YAML"));
    filter.add_pattern("*.yaml");
    filter.add_pattern("*.yml");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title(if create {
            "New run database"
        } else {
            "Choose your run database"
        })
        .filters(&filters)
        .initial_name("timer_db.yaml")
        .build();
    let shared2 = shared.clone();
    let done = move |r: Result<gio::File, gtk::glib::Error>| {
        if let Ok(file) = r
            && let Some(path) = file.path()
        {
            let _ = shared2.update(|s| s.run_database = Some(path));
        }
    };
    if create {
        dialog.save(Some(&shared.window), gio::Cancellable::NONE, done);
    } else {
        dialog.open(Some(&shared.window), gio::Cancellable::NONE, done);
    }
}

/// A row holding one file path, with Choose and Clear buttons.
fn file_row(
    shared: &Shared,
    subscriptions: &mut Vec<usize>,
    title: &str,
    help: &str,
    get: fn(&AppSettings) -> Option<PathBuf>,
    set: fn(&mut AppSettings, Option<PathBuf>),
    (filter_name, patterns): (&str, &[&str]),
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let show = {
        let (row, shared, help) = (row.clone(), shared.clone(), help.to_owned());
        move || {
            row.set_subtitle(&match get(&shared.settings()) {
                Some(path) => format!("{}\n{help}", path.display()),
                None => format!("Not set\n{help}"),
            });
        }
    };
    show();
    subscriptions.push(shared.subscribe(move |_| show()));
    let choose = gtk::Button::with_label("Choose…");
    choose.set_valign(gtk::Align::Center);
    let clear = gtk::Button::from_icon_name("edit-clear-symbolic");
    clear.set_valign(gtk::Align::Center);
    clear.set_tooltip_text(Some("Clear"));
    clear.add_css_class("flat");
    row.add_suffix(&choose);
    row.add_suffix(&clear);
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(filter_name));
    for pattern in patterns {
        filter.add_pattern(pattern);
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let folder = patterns.is_empty();
    let (shared2, title) = (shared.clone(), title.to_owned());
    choose.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder().title(&title).build();
        if !folder {
            dialog.set_filters(Some(&filters));
        }
        let shared3 = shared2.clone();
        let chosen = move |r: Result<gio::File, glib::Error>| {
            if let Ok(file) = r
                && let Some(path) = file.path()
            {
                let _ = shared3.update(|s| set(s, Some(path)));
            }
        };
        // No patterns: it's a folder.
        if folder {
            dialog.select_folder(Some(&shared2.window), gio::Cancellable::NONE, chosen);
        } else {
            dialog.open(Some(&shared2.window), gio::Cancellable::NONE, chosen);
        }
    });
    let shared2 = shared.clone();
    clear.connect_clicked(move |_| {
        let _ = shared2.update(|s| set(s, None));
    });
    row
}

/// A row holding one of the Import page's destinations (`ingest` settings):
/// originals (`nas`: must be on a network share) or editing copies.
fn import_row(
    shared: &Shared,
    subscriptions: &mut Vec<usize>,
    title: &str,
    help: &str,
    nas: bool,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let show = {
        let (row, help) = (row.clone(), help.to_owned());
        move || {
            let settings = ingest::settings::Settings::load().unwrap_or_default();
            let place = if nas {
                settings.archive
            } else {
                settings.editing
            };
            row.set_subtitle(&match place {
                Some(d) => format!("{}\n{help}", d.path.display()),
                None => format!("Not set\n{help}"),
            });
        }
    };
    show();
    subscriptions.push(shared.subscribe(move |_| show()));
    let choose = gtk::Button::with_label("Choose…");
    choose.set_valign(gtk::Align::Center);
    row.add_suffix(&choose);
    let (shared2, title, row2) = (shared.clone(), title.to_owned(), row.clone());
    choose.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder().title(&title).build();
        let (shared3, row3) = (shared2.clone(), row2.clone());
        dialog.select_folder(Some(&shared2.window), gio::Cancellable::NONE, move |r| {
            let Ok(file) = r else { return };
            let Some(path) = file.path() else { return };
            let saved = ingest::settings::Destination::select(path, nas).and_then(|d| {
                let mut settings = ingest::settings::Settings::load()?;
                if nas {
                    settings.archive = Some(d);
                } else {
                    settings.editing = Some(d);
                }
                settings.save()
            });
            match saved {
                Ok(()) => shared3.notify(crate::shared::Change::Settings),
                Err(e) => row3.set_subtitle(&format!("Not changed: {e:#}")),
            }
        });
    });
    row
}
