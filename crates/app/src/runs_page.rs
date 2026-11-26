//! The Tracks page (`runs` in the sidebar's names): the courses ridden most
//! lately first (`timer_core::tracks`), and for the one picked its personal
//! best, target and trend (`tracks_view.rs`) over its leaderboard from the run
//! database, with best and optimal laps and sector times,
//! and batch edits (move to another course, ignore, delete) plus rendering and
//! Resolve for the ticked runs. Courses are added and renamed in the Library's
//! Courses panel. Edits are saved at once through
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
use timer_core::tracks::{CourseSummary, history, ridden_most, sector_gaps};

use crate::shared::{Change, Shared};
use crate::tracks_view::{TrackView, parse_target};

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
    course_list: gtk::ListBox,
    track: TrackView,
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
    header.set_title_widget(Some(&adw::WindowTitle::new("Tracks", "")));
    let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
    reload.set_tooltip_text(Some("Reload the run database"));
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

    // The courses on the left, ridden most lately first; the picked one on
    // the right.
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    body.set_vexpand(true);
    let side = gtk::Box::new(gtk::Orientation::Vertical, 8);
    side.add_css_class("course-panel");
    side.append(&label("Ridden most lately", "heading"));
    let course_list = gtk::ListBox::new();
    course_list.add_css_class("navigation-sidebar");
    course_list.update_property(&[gtk::accessible::Property::Label("Courses")]);
    let course_scroll = gtk::ScrolledWindow::builder()
        .child(&course_list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_width(250)
        .build();
    side.append(&course_scroll);
    body.append(&side);
    body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    let main = gtk::Box::new(gtk::Orientation::Vertical, 12);
    main.set_hexpand(true);
    let summary = label("", "heading");
    summary.set_wrap(true);
    main.append(&summary);
    let track = TrackView::new();
    main.append(&track.root);

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
    let render = button("Render overlays");
    render.set_tooltip_text(Some("Render these runs' timer overlays (Output page)"));
    let to_resolve = crate::resolve_menu::menu();
    let delete = button("Delete…");
    delete.add_css_class("destructive-action");
    for w in [
        selected_label.upcast_ref::<gtk::Widget>(),
        move_to.upcast_ref(),
        ignore.upcast_ref(),
        include.upcast_ref(),
        render.upcast_ref(),
        to_resolve.button.upcast_ref(),
        delete.upcast_ref(),
    ] {
        actions.append(w);
    }
    actions.set_visible(false);
    main.append(&actions);

    let table_holder = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(160)
        .build();
    main.append(&table_holder);
    body.append(&main);
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
        course_list: course_list.clone(),
        track: track.clone(),
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
    course_list.connect_row_selected(move |_, row| {
        if p.filling.get() {
            return;
        }
        let id = row.map(|r| r.widget_name().to_string());
        if id.is_some() && id != p.state.borrow().course {
            let mut s = p.state.borrow_mut();
            s.course = id;
            s.selected.clear();
            drop(s);
            show(&p);
        }
    });
    let p = page.clone();
    track
        .target_button
        .connect_clicked(move |_| target_dialog(&p));
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
    render.connect_clicked(move |_| {
        let ids = selected(&p);
        p.shared.render_runs(ids);
    });
    let p = page.clone();
    to_resolve.on_pick(move |target| p.shared.send_to_resolve(target, selected(&p)));
    let p = page.clone();
    shared.subscribe(move |change| {
        // Our own saves are already shown; other pages' (and new settings) are not.
        let stale = !p
            .state
            .borrow()
            .file
            .as_ref()
            .is_some_and(DatabaseFile::unchanged);
        if change == Change::Settings || (change == Change::Runs && stale) {
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
        // Default to the course ridden most lately.
        s.course = s.db.as_ref().and_then(|db| {
            ridden_most(db, chrono::Utc::now())
                .first()
                .map(|c| c.course.id.clone())
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

    let now = chrono::Utc::now();
    let order = ridden_most(db, now);
    let current = s.course.clone();
    drop(s);
    p.filling.set(true);
    while let Some(row) = p.course_list.first_child() {
        p.course_list.remove(&row);
    }
    for c in &order {
        let row = course_row(c);
        p.course_list.append(&row);
        if current.as_ref() == Some(&c.course.id) {
            p.course_list.select_row(Some(&row));
        }
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
            .set_text("No courses yet: add one with New… in the Library's Courses panel.");
        p.table_holder.set_child(None::<&gtk::Widget>);
        p.actions.set_visible(false);
        return;
    };
    let board = course_board(course, &db.runs);
    p.summary.set_text(&summary(&board));
    let target = library::Library::open_default()
        .and_then(|l| l.target(&course.id))
        .unwrap_or_default();
    p.track.show(
        order.iter().find(|c| c.course.id == course.id),
        history(&board, &db.runs),
        &sector_gaps(&board, &db.runs),
        target,
        now,
    );
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
    let from = p.state.borrow().course.clone();
    let p2 = p.clone();
    crate::courses::move_runs(&p.shared, ids, from.as_deref(), move |result| {
        p2.status.set_text(&result.unwrap_or_else(|e| e));
    });
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

/// A course in the list: its name, then its PB and how often it's been ridden lately.
fn course_row(c: &CourseSummary) -> gtk::ListBoxRow {
    let lines = gtk::Box::new(gtk::Orientation::Vertical, 2);
    lines.set_margin_top(6);
    lines.set_margin_bottom(6);
    let name = label(&c.course.name, "");
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    lines.append(&name);
    let mut detail = Vec::new();
    if let Some(b) = &c.best {
        detail.push(format!("PB {}", time(b.lap_seconds)));
    }
    detail.push(match c.recent_runs {
        0 if c.runs == 0 => "no runs yet".to_owned(),
        0 => "not ridden lately".to_owned(),
        1 => "1 run lately".to_owned(),
        n => format!("{n} runs lately"),
    });
    let detail = detail.join(" · ");
    lines.append(&label(&detail, "dim-label"));
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&lines));
    row.set_widget_name(&c.course.id);
    row.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {detail}",
        c.course.name
    ))]);
    row
}

/// Sets or clears the course's target lap time (kept in the library, not the
/// run database).
fn target_dialog(p: &Page) {
    let Some(course) = p.state.borrow().course.clone() else {
        return;
    };
    let library = match library::Library::open_default() {
        Ok(l) => l,
        Err(e) => {
            p.status
                .set_text(&format!("Library could not be opened: {e:#}"));
            return;
        }
    };
    let current = library.target(&course).unwrap_or_default();
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("2:31.4"));
    if let Some(t) = current {
        entry.set_text(&time(t));
    }
    entry.set_activates_default(true);
    let dialog = adw::AlertDialog::new(
        Some("Target lap time"),
        Some("The time you're chasing on this course. Leave it empty for none."),
    );
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    let page = p.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "save" {
            return;
        }
        match parse_target(&entry.text())
            .map_err(anyhow::Error::msg)
            .and_then(|t| {
                library.set_target(&course, t)?;
                Ok(t)
            }) {
            Ok(Some(t)) => page.status.set_text(&format!("Target set: {}.", time(t))),
            Ok(None) => page.status.set_text("Target cleared."),
            Err(e) => page.status.set_text(&format!("{e:#}")),
        }
        show(&page);
    });
    let window = p.course_list.root().and_downcast::<gtk::Window>();
    dialog.present(window.as_ref());
}
