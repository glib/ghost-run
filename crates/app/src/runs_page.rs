//! The Runs page: a leaderboard per course from the run database, with best
//! and optimal laps, and batch edits (move to another course, ignore, delete;
//! add and rename courses). Edits are saved at once through
//! [`timer_core::file::DatabaseFile`], which backs the file up first and won't
//! write over changes made elsewhere.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use timer_core::TimerDatabase;
use timer_core::board::{BoardRow, CourseBoard, RunStatus, course_board};
use timer_core::edit::{self, EditError};
use timer_core::file::{DatabaseFile, SaveError};
use timer_core::timing::{format_delta, format_duration};

use crate::shared::{Change, Shared};

struct State {
    file: Option<DatabaseFile>,
    db: Option<TimerDatabase>,
    course: Option<String>,
    selected: HashSet<String>,
}

#[derive(Clone)]
struct Page {
    shared: Shared,
    state: Rc<RefCell<State>>,
    banner: gtk::Box,
    banner_text: gtk::Label,
    body: gtk::Box,
    courses: gtk::DropDown,
    course_ids: Rc<RefCell<Vec<String>>>,
    /// Set while the course list is refilled, so its change signal is ignored.
    filling: Rc<std::cell::Cell<bool>>,
    summary: gtk::Label,
    table_holder: gtk::ScrolledWindow,
    store: gio::ListStore,
    actions: gtk::Box,
    selected_label: gtk::Label,
    status: gtk::Label,
}

pub fn page(shared: &Shared) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Runs", "")));
    let new_course = button("New course…");
    let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
    reload.set_tooltip_text(Some("Reload the run database"));
    header.pack_end(&new_course);
    header.pack_end(&reload);
    root.append(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("content");
    content.set_vexpand(true);

    let banner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    banner.add_css_class("banner");
    let banner_text = label("", "");
    banner_text.set_wrap(true);
    banner_text.set_hexpand(true);
    let choose = button("Choose database…");
    let create = button("New database…");
    banner.append(&banner_text);
    banner.append(&choose);
    banner.append(&create);
    content.append(&banner);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_vexpand(true);
    let course_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let courses = gtk::DropDown::from_strings(&[]);
    courses.update_property(&[gtk::accessible::Property::Label("Course")]);
    let rename = button("Rename…");
    course_row.append(&courses);
    course_row.append(&rename);
    body.append(&course_row);
    let summary = label("", "heading");
    summary.set_wrap(true);
    body.append(&summary);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.add_css_class("locations");
    let selected_label = label("", "");
    selected_label.set_hexpand(true);
    let move_to = button("Move to course…");
    let ignore = button("Ignore");
    ignore.set_tooltip_text(Some(
        "Leave these out of best and optimal laps (they stay in the database)",
    ));
    let include = button("Count again");
    let delete = button("Delete…");
    delete.add_css_class("destructive-action");
    for w in [
        selected_label.upcast_ref::<gtk::Widget>(),
        move_to.upcast_ref(),
        ignore.upcast_ref(),
        include.upcast_ref(),
        delete.upcast_ref(),
    ] {
        actions.append(w);
    }
    actions.set_visible(false);
    body.append(&actions);

    let table_holder = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(160)
        .build();
    body.append(&table_holder);
    content.append(&body);
    root.append(&content);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    footer.add_css_class("footer");
    let status = label("", "");
    status.set_wrap(true);
    status.set_hexpand(true);
    footer.append(&status);
    root.append(&footer);

    let page = Page {
        shared: shared.clone(),
        state: Rc::new(RefCell::new(State {
            file: None,
            db: None,
            course: None,
            selected: HashSet::new(),
        })),
        banner,
        banner_text,
        body,
        courses: courses.clone(),
        course_ids: Rc::default(),
        filling: Rc::default(),
        summary,
        table_holder,
        store: gio::ListStore::new::<glib::BoxedAnyObject>(),
        actions,
        selected_label,
        status,
    };

    let p = page.clone();
    choose.connect_clicked(move |_| crate::settings_dialog::choose_database(&p.shared, false));
    let p = page.clone();
    create.connect_clicked(move |_| crate::settings_dialog::choose_database(&p.shared, true));
    let p = page.clone();
    reload.connect_clicked(move |_| {
        load(&p);
        p.status.set_text("Reloaded.");
    });
    let p = page.clone();
    courses.connect_selected_notify(move |d| {
        if p.filling.get() {
            return;
        }
        let id = p.course_ids.borrow().get(d.selected() as usize).cloned();
        if id.is_some() && id != p.state.borrow().course {
            let mut s = p.state.borrow_mut();
            s.course = id;
            s.selected.clear();
            drop(s);
            show(&p);
        }
    });
    let p = page.clone();
    new_course.connect_clicked(move |_| new_course_dialog(&p));
    let p = page.clone();
    rename.connect_clicked(move |_| rename_dialog(&p));
    let p = page.clone();
    move_to.connect_clicked(move |_| move_dialog(&p));
    let p = page.clone();
    ignore.connect_clicked(move |_| {
        let ids = selected(&p);
        apply(&p, &format!("Ignored {}.", count(ids.len())), |db| {
            edit::set_ignored(db, &ids, true)
        });
    });
    let p = page.clone();
    include.connect_clicked(move |_| {
        let ids = selected(&p);
        apply(&p, &format!("{} counted again.", count(ids.len())), |db| {
            edit::set_ignored(db, &ids, false)
        });
    });
    let p = page.clone();
    delete.connect_clicked(move |_| delete_dialog(&p));
    let p = page.clone();
    shared.subscribe(move |change| {
        // Our own saves are already shown; anything else may have moved the file.
        if change == Change::Settings {
            load(&p);
        }
    });
    load(&page);
    root.upcast()
}

fn load(p: &Page) {
    let path = p.shared.settings().run_database.clone();
    let mut s = p.state.borrow_mut();
    s.selected.clear();
    match path.map(DatabaseFile::load) {
        None => {
            s.file = None;
            s.db = None;
            p.banner_text.set_text(
                "Choose your run database (timer_db.yaml), or start a new one. It holds your courses and runs; the Resolve script uses the same file.",
            );
        }
        Some(Ok((file, db))) => {
            s.file = Some(file);
            s.db = Some(db);
        }
        Some(Err(e)) => {
            s.file = None;
            s.db = None;
            p.banner_text
                .set_text(&format!("The run database couldn't be read: {e}"));
        }
    }
    let ids: Vec<String> =
        s.db.as_ref()
            .map(|db| db.courses.iter().map(|c| c.id.clone()).collect())
            .unwrap_or_default();
    if !s.course.as_ref().is_some_and(|c| ids.contains(c)) {
        // Default to the course ridden most recently.
        s.course = s.db.as_ref().and_then(|db| {
            db.runs
                .iter()
                .filter(|r| ids.contains(&r.course_id))
                .max_by_key(|r| timer_core::matching::run_order_time(r))
                .map(|r| r.course_id.clone())
                .or_else(|| ids.first().cloned())
        });
    }
    drop(s);
    show(p);
}

/// Fills everything from the state.
fn show(p: &Page) {
    let s = p.state.borrow();
    p.banner.set_visible(s.db.is_none());
    p.body.set_visible(s.db.is_some());
    let Some(db) = &s.db else { return };

    let names: Vec<String> = db
        .courses
        .iter()
        .map(|c| {
            let runs = db.runs.iter().filter(|r| r.course_id == c.id).count();
            format!(
                "{} ({} {})",
                c.name,
                runs,
                if runs == 1 { "run" } else { "runs" }
            )
        })
        .collect();
    *p.course_ids.borrow_mut() = db.courses.iter().map(|c| c.id.clone()).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let position = s
        .course
        .as_ref()
        .and_then(|c| db.courses.iter().position(|x| &x.id == c));
    let model = gtk::StringList::new(&names);
    drop(s);
    p.filling.set(true);
    p.courses.set_model(Some(&model));
    if let Some(i) = position {
        p.courses.set_selected(i as u32);
    }
    p.filling.set(false);
    let s = p.state.borrow();
    let db = s.db.as_ref().expect("checked above");
    let Some(course) = s
        .course
        .as_ref()
        .and_then(|c| db.courses.iter().find(|x| &x.id == c))
    else {
        p.summary
            .set_text("No courses yet: add one with New course….");
        p.table_holder.set_child(None::<&gtk::Widget>);
        p.actions.set_visible(false);
        return;
    };
    let board = course_board(course, &db.runs);
    p.summary.set_text(&summary(&board));
    let rows: Vec<glib::BoxedAnyObject> = board
        .rows
        .iter()
        .cloned()
        .map(glib::BoxedAnyObject::new)
        .collect();
    p.store.remove_all();
    p.store.extend_from_slice(&rows);
    let sectors = board.course.sector_count.max(0) as usize;
    drop(s);
    p.table_holder.set_child(Some(&table(p, sectors)));
    refresh_actions(p);
}

fn summary(board: &CourseBoard) -> String {
    let counted = board
        .rows
        .iter()
        .filter(|r| r.status == RunStatus::Counted)
        .count();
    let mut parts = vec![format!(
        "{} {} · {} {}",
        board.rows.len(),
        if board.rows.len() == 1 { "run" } else { "runs" },
        board.course.sector_count,
        if board.course.sector_count == 1 {
            "sector"
        } else {
            "sectors"
        }
    )];
    if let Some((id, lap)) = &board.best_lap {
        parts.push(format!("Best lap {} ({id})", time(*lap)));
    }
    if let Some(optimal) = board.optimal_seconds {
        parts.push(format!("Optimal {}", time(optimal)));
    }
    if counted < board.rows.len() {
        parts.push(format!("{} not counted", board.rows.len() - counted));
    }
    parts.join("  ·  ")
}

fn time(seconds: f64) -> String {
    format_duration(seconds).unwrap_or_else(|_| "?".into())
}

#[derive(Clone, Copy)]
enum Col {
    Rank,
    Lap,
    Delta,
    Sector(usize),
    When,
    Run,
    Clip,
    Status,
}

fn table(p: &Page, sectors: usize) -> gtk::ColumnView {
    let view = gtk::ColumnView::new(Some(gtk::NoSelection::new(Some(p.store.clone()))));
    view.add_css_class("footage-table");
    view.set_show_row_separators(true);
    let mut columns = vec![
        ("#", Col::Rank, 70),
        ("Lap", Col::Lap, 95),
        ("Δ best", Col::Delta, 85),
    ];
    let names: Vec<String> = (1..=sectors).map(|i| format!("S{i}")).collect();
    for (i, name) in names.iter().enumerate() {
        columns.push((name.as_str(), Col::Sector(i), 85));
    }
    columns.extend([
        ("Ridden", Col::When, 150),
        ("Run", Col::Run, 200),
        ("Clip", Col::Clip, 200),
        ("", Col::Status, 260),
    ]);
    for (title, col, width) in columns {
        let factory = gtk::SignalListItemFactory::new();
        let page = p.clone();
        factory.connect_setup(move |_, item| {
            let li = item.downcast_ref::<gtk::ListItem>().unwrap();
            let text = label("", "");
            if matches!(col, Col::Run | Col::Clip | Col::Status) {
                text.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
            if let Col::Rank = col {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let check = gtk::CheckButton::new();
                row.append(&check);
                row.append(&text);
                li.set_child(Some(&row));
                let weak = li.downgrade();
                let page = page.clone();
                check.connect_toggled(move |check| {
                    let Some(li) = weak.upgrade() else { return };
                    let Some(object) = li.item().and_downcast::<glib::BoxedAnyObject>() else {
                        return;
                    };
                    let id = object.borrow::<BoardRow>().run_id.clone();
                    let mut s = page.state.borrow_mut();
                    let changed = if check.is_active() {
                        s.selected.insert(id)
                    } else {
                        s.selected.remove(&id)
                    };
                    drop(s);
                    if changed {
                        refresh_actions(&page);
                    }
                });
            } else {
                li.set_child(Some(&text));
            }
        });
        let page = p.clone();
        factory.connect_bind(move |_, item| {
            let li = item.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(object) = li.item().and_downcast::<glib::BoxedAnyObject>() else {
                return;
            };
            let row = object.borrow::<BoardRow>();
            let text = if let Col::Rank = col {
                let boxed = li.child().and_downcast::<gtk::Box>().unwrap();
                let check = boxed
                    .first_child()
                    .and_downcast::<gtk::CheckButton>()
                    .unwrap();
                check.set_active(page.state.borrow().selected.contains(&row.run_id));
                check.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Select run {}",
                    row.run_id
                ))]);
                check.next_sibling().and_downcast::<gtk::Label>().unwrap()
            } else {
                li.child().and_downcast::<gtk::Label>().unwrap()
            };
            for class in ["success", "warning", "muted", "fastest"] {
                text.remove_css_class(class);
            }
            text.set_tooltip_text(None);
            let value = match col {
                Col::Rank => row.rank.map(|r| r.to_string()).unwrap_or_default(),
                Col::Lap => row.lap_seconds.map(time).unwrap_or_default(),
                Col::Delta => match row.delta_best {
                    Some(d) if row.rank == Some(1) => {
                        text.add_css_class("success");
                        if d == 0.0 {
                            "best".into()
                        } else {
                            format_delta(d)
                        }
                    }
                    Some(d) => format_delta(d),
                    None => String::new(),
                },
                Col::Sector(i) => {
                    if row.fastest.get(i) == Some(&true) {
                        text.add_css_class("fastest");
                        text.set_tooltip_text(Some("Fastest time for this sector"));
                    }
                    row.sectors[i].map(time).unwrap_or_default()
                }
                Col::When => row.when.clone(),
                Col::Run => row.run_id.clone(),
                Col::Clip => row.filename.clone(),
                Col::Status => match &row.status {
                    RunStatus::Counted => String::new(),
                    RunStatus::Ignored => {
                        text.add_css_class("muted");
                        "Ignored".into()
                    }
                    RunStatus::NotCommitted => {
                        text.add_css_class("muted");
                        "Not committed".into()
                    }
                    RunStatus::Invalid(why) => {
                        text.add_css_class("warning");
                        text.set_tooltip_text(Some(why));
                        format!("Markers need fixing: {why}")
                    }
                },
            };
            if matches!(row.status, RunStatus::Ignored | RunStatus::NotCommitted)
                && matches!(col, Col::Lap | Col::Sector(_))
            {
                text.add_css_class("muted");
            }
            text.set_text(&value);
        });
        let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
        column.set_fixed_width(width);
        column.set_resizable(true);
        column.set_expand(matches!(col, Col::Status));
        view.append_column(&column);
    }
    view
}

fn refresh_actions(p: &Page) {
    let n = p.state.borrow().selected.len();
    p.actions.set_visible(n > 0);
    p.selected_label.set_text(&format!("{} selected", count(n)));
}

fn count(n: usize) -> String {
    format!("{n} {}", if n == 1 { "run" } else { "runs" })
}

fn selected(p: &Page) -> Vec<String> {
    let mut ids: Vec<String> = p.state.borrow().selected.iter().cloned().collect();
    ids.sort();
    ids
}

/// Applies an edit to a copy of the database and saves it; on any failure the
/// page and the file stay as they were.
fn apply(p: &Page, done: &str, change: impl FnOnce(&mut TimerDatabase) -> Result<(), EditError>) {
    let mut s = p.state.borrow_mut();
    let State { file, db, .. } = &mut *s;
    let (Some(file), Some(db)) = (file.as_mut(), db.as_ref()) else {
        return;
    };
    let mut new = db.clone();
    if let Err(e) = change(&mut new) {
        p.status.set_text(&format!("Not changed: {e}"));
        return;
    }
    match file.save(&new) {
        Ok(()) => {
            s.db = Some(new);
            s.selected.clear();
            drop(s);
            p.status.set_text(&format!(
                "{done} Saved; the previous version is in the backups folder beside it."
            ));
            show(p);
            p.shared.notify(Change::Runs);
        }
        Err(SaveError::ChangedOnDisk(_)) => {
            drop(s);
            load(p);
            p.status.set_text(
                "The database changed on disk since it was loaded (the Resolve script?), so nothing was saved. It's reloaded now: make the change again.",
            );
        }
        Err(e) => {
            p.status.set_text(&format!("Not saved: {e}"));
        }
    }
}

fn ask(
    p: &Page,
    heading: &str,
    body: &str,
    verb: &str,
    destructive: bool,
    extra: Option<&gtk::Widget>,
    then: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", verb)]);
    dialog.set_response_appearance(
        "ok",
        if destructive {
            adw::ResponseAppearance::Destructive
        } else {
            adw::ResponseAppearance::Suggested
        },
    );
    dialog.set_default_response(Some(if destructive { "cancel" } else { "ok" }));
    dialog.set_close_response("cancel");
    if let Some(extra) = extra {
        dialog.set_extra_child(Some(extra));
    }
    dialog.connect_response(None, move |_, r| {
        if r == "ok" {
            then();
        }
    });
    dialog.present(Some(&p.shared.window));
}

fn move_dialog(p: &Page) {
    let ids = selected(p);
    let s = p.state.borrow();
    let Some(db) = &s.db else { return };
    let current = s.course.clone();
    let others: Vec<(String, String)> = db
        .courses
        .iter()
        .filter(|c| Some(&c.id) != current.as_ref())
        .map(|c| {
            (
                c.id.clone(),
                format!("{} ({} sectors)", c.name, c.sector_count),
            )
        })
        .collect();
    drop(s);
    if others.is_empty() {
        p.status
            .set_text("There's no other course to move them to: add one with New course….");
        return;
    }
    let names: Vec<&str> = others.iter().map(|(_, n)| n.as_str()).collect();
    let choice = gtk::DropDown::from_strings(&names);
    let p2 = p.clone();
    let choice2 = choice.clone();
    ask(
        p,
        &format!("Move {}?", count(ids.len())),
        "Their markers stay as they are, so the course must have the same number of sectors.",
        "Move",
        false,
        Some(choice.upcast_ref()),
        move || {
            let (id, name) = others[choice2.selected() as usize].clone();
            let ids = ids.clone();
            apply(
                &p2,
                &format!("Moved {} to {name}.", count(ids.len())),
                |db| edit::move_runs(db, &ids, &id),
            );
        },
    );
}

fn delete_dialog(p: &Page) {
    let ids = selected(p);
    let p2 = p.clone();
    let list = ids.join(", ");
    ask(
        p,
        &format!("Delete {}?", count(ids.len())),
        &format!(
            "{list}\n\nThey're removed from the database (a backup of it is kept). The clips aren't touched. To keep a run but leave it out of best laps, use Ignore."
        ),
        "Delete",
        true,
        None,
        move || {
            let ids = ids.clone();
            apply(&p2, &format!("Deleted {}.", count(ids.len())), |db| {
                edit::delete_runs(db, &ids)
            });
        },
    );
}

fn new_course_dialog(p: &Page) {
    if p.state.borrow().db.is_none() {
        p.status.set_text("Choose or create a run database first.");
        return;
    }
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let name = gtk::Entry::builder()
        .placeholder_text("Name, e.g. Lower Whistler A-Line")
        .build();
    let sectors_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    sectors_row.append(&label("Sectors", ""));
    let sectors = gtk::SpinButton::with_range(1.0, 20.0, 1.0);
    sectors.set_value(3.0);
    sectors_row.append(&sectors);
    form.append(&name);
    form.append(&sectors_row);
    let p2 = p.clone();
    let (name2, sectors2) = (name.clone(), sectors.clone());
    ask(
        p,
        "New course",
        "Sectors are the timed parts between Start, S1, S2… and Finish: 3 sectors means markers Start, S1, S2, Finish.",
        "Add",
        false,
        Some(form.upcast_ref()),
        move || {
            let text = name2.text().to_string();
            let n = sectors2.value() as i64;
            let added: Rc<RefCell<Option<String>>> = Rc::default();
            let added2 = added.clone();
            apply(&p2, &format!("Added {text}."), move |db| {
                *added2.borrow_mut() = Some(edit::add_course(db, &text, n)?);
                Ok(())
            });
            if let Some(id) = added.borrow().clone() {
                p2.state.borrow_mut().course = Some(id);
                show(&p2);
            }
        },
    );
}

fn rename_dialog(p: &Page) {
    let s = p.state.borrow();
    let Some(course) = s
        .course
        .as_ref()
        .and_then(|c| s.db.as_ref()?.courses.iter().find(|x| &x.id == c))
        .cloned()
    else {
        return;
    };
    drop(s);
    let entry = gtk::Entry::builder().text(&course.name).build();
    let p2 = p.clone();
    let entry2 = entry.clone();
    ask(
        p,
        "Rename course",
        &format!(
            "Its id ({}) stays the same, so runs and overlays keep pointing at it.",
            course.id
        ),
        "Rename",
        false,
        Some(entry.upcast_ref()),
        move || {
            let name = entry2.text().to_string();
            let id = course.id.clone();
            apply(&p2, &format!("Renamed to {name}."), |db| {
                edit::rename_course(db, &id, &name)
            });
        },
    );
}

fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.);
    if !class.is_empty() {
        l.add_css_class(class);
    }
    l
}

fn button(title: &str) -> gtk::Button {
    let b = gtk::Button::with_label(title);
    b.add_css_class("ingest-action");
    b
}
