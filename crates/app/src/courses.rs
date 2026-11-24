//! Course and run edits any page can start: new course, rename, move runs to
//! another course. Each loads the run database fresh, applies the edit to a
//! copy, saves it through [`DatabaseFile`] (which backs the file up and won't
//! write over changes made elsewhere) and tells every page ([`Change::Runs`]).

use adw::prelude::*;
use timer_core::TimerDatabase;
use timer_core::edit::{self, EditError};
use timer_core::file::{DatabaseFile, SaveError};

use crate::shared::{Change, Shared};

/// Applies `change` to the run database and saves it. `Ok` with the change's
/// result, or why nothing was saved.
pub fn edit<T>(
    shared: &Shared,
    change: impl FnOnce(&mut TimerDatabase) -> Result<T, EditError>,
) -> Result<T, String> {
    let path = shared
        .settings()
        .run_database
        .clone()
        .ok_or("Choose or create a run database first (the gear in the sidebar).")?;
    let (mut file, db) =
        DatabaseFile::load(&path).map_err(|e| format!("The run database couldn't be read: {e}"))?;
    let mut new = db.clone();
    let result = change(&mut new).map_err(|e| format!("Not changed: {e}"))?;
    match file.save(&new) {
        Ok(()) => {
            shared.notify(Change::Runs);
            Ok(result)
        }
        Err(SaveError::ChangedOnDisk(_)) => Err(
            "The database changed on disk while saving, so nothing was saved: try again."
                .to_owned(),
        ),
        Err(e) => Err(format!("Not saved: {e}")),
    }
}

/// The run database as it is now, if there is one.
pub fn load(shared: &Shared) -> Option<TimerDatabase> {
    let path = shared.settings().run_database.clone()?;
    TimerDatabase::load(&path).ok()
}

const SAVED: &str = "Saved; the previous version is in the backups folder beside it.";

/// Asks for a new course's name and sectors and adds it; `done` gets its id
/// and a message, or why it wasn't added.
pub fn new_course(shared: &Shared, done: impl Fn(Result<(String, String), String>) + 'static) {
    if shared.settings().run_database.is_none() {
        done(Err(
            "Choose or create a run database first (the gear in the sidebar).".into(),
        ));
        return;
    }
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let name = gtk::Entry::builder()
        .placeholder_text("Name, e.g. Lower Whistler A-Line")
        .activates_default(true)
        .build();
    name.update_property(&[gtk::accessible::Property::Label("Course name")]);
    let sectors_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    sectors_row.append(&gtk::Label::new(Some("Sectors")));
    let sectors = gtk::SpinButton::with_range(1.0, 20.0, 1.0);
    sectors.set_value(3.0);
    sectors.update_property(&[gtk::accessible::Property::Label("Sectors")]);
    sectors_row.append(&sectors);
    form.append(&name);
    form.append(&sectors_row);
    let (shared2, name2, sectors2) = (shared.clone(), name.clone(), sectors.clone());
    ask(
        shared,
        "New course",
        "Sectors are the timed parts between Start, S1, S2… and Finish: 3 sectors means markers Start, S1, S2, Finish.",
        "Add",
        Some(form.upcast_ref()),
        Some(name.upcast_ref()),
        move || {
            let text = name2.text().trim().to_owned();
            let n = sectors2.value() as i64;
            let result = edit(&shared2, |db| edit::add_course(db, &text, n))
                .map(|id| (id, format!("Added {text}. {SAVED}")));
            done(result);
        },
    );
}

/// Asks for a course's new name and renames it.
pub fn rename_course(
    shared: &Shared,
    course_id: &str,
    current: &str,
    done: impl Fn(Result<String, String>) + 'static,
) {
    let entry = gtk::Entry::builder()
        .text(current)
        .activates_default(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Course name")]);
    let (shared2, id, entry2) = (shared.clone(), course_id.to_owned(), entry.clone());
    ask(
        shared,
        "Rename course",
        &format!("Its id ({course_id}) stays the same, so runs and overlays keep pointing at it."),
        "Rename",
        Some(entry.upcast_ref()),
        Some(entry.upcast_ref()),
        move || {
            let name = entry2.text().trim().to_owned();
            done(
                edit(&shared2, |db| edit::rename_course(db, &id, &name))
                    .map(|()| format!("Renamed to {name}. {SAVED}")),
            );
        },
    );
}

/// Asks which course to move runs `ids` to (any but `from`) and moves them.
pub fn move_runs(
    shared: &Shared,
    ids: Vec<String>,
    from: Option<&str>,
    done: impl Fn(Result<String, String>) + 'static,
) {
    let Some(db) = load(shared) else {
        done(Err(
            "Choose or create a run database first (the gear in the sidebar).".into(),
        ));
        return;
    };
    let others: Vec<(String, String)> = db
        .courses
        .iter()
        .filter(|c| Some(c.id.as_str()) != from)
        .map(|c| {
            (
                c.id.clone(),
                format!(
                    "{} ({} {})",
                    c.name,
                    c.sector_count,
                    if c.sector_count == 1 {
                        "sector"
                    } else {
                        "sectors"
                    }
                ),
            )
        })
        .collect();
    if others.is_empty() {
        done(Err(
            "There's no other course to move them to: add one with New course….".into(),
        ));
        return;
    }
    let names: Vec<&str> = others.iter().map(|(_, n)| n.as_str()).collect();
    let choice = gtk::DropDown::from_strings(&names);
    choice.update_property(&[gtk::accessible::Property::Label("Course to move them to")]);
    let (shared2, choice2) = (shared.clone(), choice.clone());
    let n = ids.len();
    let what = format!("{n} {}", if n == 1 { "run" } else { "runs" });
    ask(
        shared,
        &format!("Move {what}?"),
        "Their markers stay as they are, so the course must have the same number of sectors.",
        "Move",
        Some(choice.upcast_ref()),
        None,
        move || {
            let (id, name) = others[choice2.selected() as usize].clone();
            let result = edit(&shared2, |db| edit::move_runs(db, &ids, &id))
                .map(|()| format!("Moved {what} to {name}. {SAVED}"));
            done(result);
        },
    );
}

fn ask(
    shared: &Shared,
    heading: &str,
    body: &str,
    verb: &str,
    extra: Option<&gtk::Widget>,
    focus: Option<&gtk::Widget>,
    then: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", verb)]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    if let Some(extra) = extra {
        dialog.set_extra_child(Some(extra));
    }
    dialog.connect_response(None, move |_, r| {
        if r == "ok" {
            then();
        }
    });
    dialog.present(Some(&shared.window));
    // Typing goes into the form, not to the buttons.
    if let Some(focus) = focus {
        dialog.set_focus(Some(focus));
    }
}
