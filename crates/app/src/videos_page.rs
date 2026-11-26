//! The Videos page (decision 0005): the videos in the catalog
//! (`library::videos`), newest first, and the one picked: its title, status
//! (planned, in Resolve, published), YouTube link and date, the Resolve
//! project it was built as, and its clips in order, each with a Timer switch
//! when it's a timed run (its overlay goes over the timed part). Build in
//! Resolve hands it to the jobs drawer (`Shared::build_video`), which renders
//! the timers wanted, copies NAS clips down and makes the project.
//!
//! Clips are added from the Library (Organise → Add to video…).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use library::videos::{Item, Status, Video};
use library::{Clip, Library};

use crate::shared::{Change, Shared};

/// A clip's committed run, as a video item shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct RunLabel {
    pub id: String,
    pub course: String,
}

#[derive(Default)]
struct State {
    videos: Vec<Video>,
    selected: Option<i64>,
    /// Clip key → the clip as catalogued.
    clips: HashMap<String, Clip>,
    /// Lower-case clip name ([`crate::library_page::run_key`]) → its run.
    runs: HashMap<String, RunLabel>,
}

#[derive(Clone)]
struct Page {
    shared: Shared,
    state: Rc<RefCell<State>>,
    list: gtk::ListBox,
    stack: gtk::Stack,
    detail: gtk::Box,
    status: gtk::Label,
    /// Set while the list is refilled, so its selection isn't taken as a click.
    filling: Rc<std::cell::Cell<bool>>,
}

pub fn page(shared: &Shared) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Videos", "")));
    let new = gtk::Button::with_label("New video…");
    new.set_tooltip_text(Some(
        "Start a video; add its clips from the Library (Organise → Add to video…)",
    ));
    header.pack_end(&new);
    root.append(&header);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    body.add_css_class("content");
    body.set_vexpand(true);
    let side = gtk::Box::new(gtk::Orientation::Vertical, 8);
    side.set_size_request(220, -1);
    side.append(&label("Videos", "heading"));
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.update_property(&[gtk::accessible::Property::Label("Videos")]);
    side.append(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build(),
    );
    body.append(&side);
    body.append(&gtk::Separator::new(gtk::Orientation::Vertical));

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    let empty = adw::StatusPage::new();
    empty.set_icon_name(Some("video-x-generic-symbolic"));
    empty.set_title("No videos yet");
    empty.set_description(Some(
        "Tick clips in the Library and choose Organise → Add to video…, or start one here with New video….",
    ));
    stack.add_named(&empty, Some("empty"));
    let detail = gtk::Box::new(gtk::Orientation::Vertical, 18);
    stack.add_named(
        &gtk::ScrolledWindow::builder()
            .child(&detail)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build(),
        Some("detail"),
    );
    body.append(&stack);
    root.append(&body);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    footer.add_css_class("footer");
    let status = label("", "");
    status.set_wrap(true);
    status.set_hexpand(true);
    footer.append(&status);
    root.append(&footer);

    let page = Page {
        shared: shared.clone(),
        state: Rc::default(),
        list: list.clone(),
        stack,
        detail,
        status,
        filling: Rc::default(),
    };

    let p = page.clone();
    list.connect_row_selected(move |_, row| {
        if p.filling.get() {
            return;
        }
        let Some(id) = row.and_then(|r| r.widget_name().parse().ok()) else {
            return;
        };
        p.state.borrow_mut().selected = Some(id);
        show(&p);
    });
    let p = page.clone();
    new.connect_clicked(move |_| {
        let p2 = p.clone();
        crate::organise_ui::name_dialog(
            &p.shared,
            "New video",
            "Then tick its clips in the Library and choose Organise → Add to video….",
            "",
            None,
            move |title| match Library::open_default().and_then(|l| l.new_video(&title, "clips")) {
                Ok(id) => {
                    p2.state.borrow_mut().selected = Some(id);
                    p2.shared.videos_changed();
                }
                Err(e) => p2.status.set_text(&format!("Not made: {e:#}")),
            },
        );
    });
    let p = page.clone();
    shared.subscribe(move |change| match change {
        // After the widget that saved is done (it's rebuilt).
        Change::Videos | Change::Runs | Change::Settings => {
            let p = p.clone();
            gtk::glib::idle_add_local_once(move || load(&p));
        }
        Change::ShowVideo => {
            if let Some(id) = p.shared.take_video_to_show() {
                p.state.borrow_mut().selected = Some(id);
                load(&p);
            }
        }
        _ => {}
    });
    load(&page);
    root.upcast()
}

/// Reads the videos, clips and runs again, and shows them.
fn load(p: &Page) {
    let library = Library::open_default();
    let (videos, clips) = match library.and_then(|l| Ok((l.videos()?, l.clips()?))) {
        Ok(v) => v,
        Err(e) => {
            p.status
                .set_text(&format!("The catalog couldn't be read: {e:#}"));
            return;
        }
    };
    let mut s = p.state.borrow_mut();
    s.clips = clips
        .into_iter()
        .map(|c| (library::organise::key(&c), c))
        .collect();
    s.runs = runs(&p.shared);
    if !s
        .selected
        .is_some_and(|id| videos.iter().any(|v| v.id == id))
    {
        s.selected = videos.first().map(|v| v.id);
    }
    s.videos = videos;
    drop(s);
    fill_list(p);
    show(p);
}

/// Each committed run by its clip's name ([`crate::library_page::run_key`]).
pub fn runs(shared: &Shared) -> HashMap<String, RunLabel> {
    let Some(db) = shared
        .settings()
        .run_database
        .clone()
        .and_then(|path| timer_core::TimerDatabase::load(path).ok())
    else {
        return HashMap::new();
    };
    let names: HashMap<&str, &str> = db
        .courses
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str()))
        .collect();
    db.runs
        .iter()
        .filter(|r| r.committed && !r.ignored)
        .map(|r| {
            (
                crate::library_page::run_key(std::path::Path::new(&r.filename)),
                RunLabel {
                    id: r.id.clone(),
                    course: names.get(r.course_id.as_str()).unwrap_or(&"").to_string(),
                },
            )
        })
        .collect()
}

fn fill_list(p: &Page) {
    let s = p.state.borrow();
    p.filling.set(true);
    while let Some(row) = p.list.first_child() {
        p.list.remove(&row);
    }
    for v in &s.videos {
        let line = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let title = label(&v.title, "");
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&title);
        let sub = label(
            &format!(
                "{} · {} {}",
                v.status.label(),
                v.items.len(),
                if v.items.len() == 1 { "clip" } else { "clips" }
            ),
            "dim-label",
        );
        sub.add_css_class("caption");
        line.append(&sub);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&line));
        row.set_widget_name(&v.id.to_string());
        row.set_tooltip_text(Some(&v.title));
        p.list.append(&row);
        if s.selected == Some(v.id) {
            p.list.select_row(Some(&row));
        }
    }
    p.filling.set(false);
}

/// Saves `edit` applied to the selected video (not its items), then reloads.
fn save(p: &Page, edit: impl FnOnce(&mut Video)) {
    let s = p.state.borrow();
    let Some(mut video) = s.videos.iter().find(|v| Some(v.id) == s.selected).cloned() else {
        return;
    };
    drop(s);
    edit(&mut video);
    match Library::open_default().and_then(|l| l.save_video(&video)) {
        Ok(()) => p.shared.videos_changed(),
        Err(e) => p.status.set_text(&format!("Not saved: {e:#}")),
    }
}

/// Saves the selected video's items as `edit` leaves them, then reloads.
fn save_items(p: &Page, edit: impl FnOnce(&mut Vec<Item>)) {
    let s = p.state.borrow();
    let Some(video) = s.videos.iter().find(|v| Some(v.id) == s.selected).cloned() else {
        return;
    };
    drop(s);
    let mut items = video.items;
    edit(&mut items);
    match Library::open_default().and_then(|l| l.set_items(video.id, &items)) {
        Ok(()) => p.shared.videos_changed(),
        Err(e) => p.status.set_text(&format!("Not saved: {e:#}")),
    }
}

/// The picked video's details.
fn show(p: &Page) {
    while let Some(w) = p.detail.first_child() {
        p.detail.remove(&w);
    }
    let s = p.state.borrow();
    let Some(video) = s.videos.iter().find(|v| Some(v.id) == s.selected).cloned() else {
        p.stack.set_visible_child_name("empty");
        return;
    };
    p.stack.set_visible_child_name("detail");

    let about = adw::PreferencesGroup::new();
    about.set_title("Video");
    let title = adw::EntryRow::new();
    title.set_title("Title");
    title.set_text(&video.title);
    title.set_show_apply_button(true);
    let pp = p.clone();
    title.connect_apply(move |row| {
        let text = row.text().to_string();
        save(&pp, |v| v.title = text);
    });
    about.add(&title);
    let status = adw::ComboRow::new();
    status.set_title("Status");
    status.set_model(Some(&gtk::StringList::new(&Status::ALL.map(|s| s.label()))));
    status.set_selected(
        Status::ALL
            .iter()
            .position(|s| *s == video.status)
            .unwrap_or(0) as u32,
    );
    let pp = p.clone();
    status.connect_selected_notify(move |row| {
        let wanted = Status::ALL[row.selected() as usize];
        save(&pp, |v| {
            if wanted == Status::Published && v.published.is_none() {
                v.published = Some(chrono::Local::now().format("%Y-%m-%d").to_string());
            }
            v.status = wanted;
        });
    });
    about.add(&status);
    let link = adw::EntryRow::new();
    link.set_title("YouTube link");
    link.set_text(video.link.as_deref().unwrap_or_default());
    link.set_show_apply_button(true);
    let pp = p.clone();
    link.connect_apply(move |row| {
        let text = row.text().trim().to_owned();
        save(&pp, |v| v.link = (!text.is_empty()).then_some(text));
    });
    about.add(&link);
    let date = adw::EntryRow::new();
    date.set_title("Published on (YYYY-MM-DD)");
    date.set_text(video.published.as_deref().unwrap_or_default());
    date.set_show_apply_button(true);
    let pp = p.clone();
    date.connect_apply(move |row| {
        let text = row.text().trim().to_owned();
        if !text.is_empty() && chrono::NaiveDate::parse_from_str(&text, "%Y-%m-%d").is_err() {
            pp.status
                .set_text("Write the date as YYYY-MM-DD, e.g. 2026-11-27.");
            return;
        }
        save(&pp, |v| v.published = (!text.is_empty()).then_some(text));
    });
    about.add(&date);
    let resolve = adw::ActionRow::new();
    resolve.set_title("In Resolve");
    resolve.set_subtitle(&match (&video.project, &video.timeline) {
        (Some(project), Some(timeline)) => format!("Project {project}, timeline {timeline}"),
        _ => "Not built yet".to_owned(),
    });
    about.add(&resolve);
    p.detail.append(&about);

    let group = adw::PreferencesGroup::new();
    group.set_title("Clips");
    group.set_description(Some(if video.items.is_empty() {
        "None yet: tick clips in the Library and choose Organise → Add to video…."
    } else {
        "In this order, whole. Timer puts the run's overlay over its timed part. Trim in Resolve."
    }));
    let count = video.items.len();
    for (index, item) in video.items.iter().enumerate() {
        let clip = s.clips.get(&item.clip);
        let row = adw::ActionRow::new();
        let name = item
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        row.set_title(&glib_escape(&name));
        let run = clip.and_then(|c| s.runs.get(&crate::library_page::run_key(&c.path)));
        row.set_subtitle(&glib_escape(&item_text(item, clip, run)));
        if let Some(run) = run {
            let timer = gtk::Switch::new();
            timer.set_valign(gtk::Align::Center);
            timer.set_active(item.timer.is_some());
            timer.set_tooltip_text(Some("Put this run's timer overlay over it"));
            timer.update_property(&[gtk::accessible::Property::Label(&format!(
                "Timer on {name}"
            ))]);
            let (pp, run_id) = (p.clone(), run.id.clone());
            timer.connect_active_notify(move |t| {
                let on = t.is_active().then(|| run_id.clone());
                save_items(&pp, |items| items[index].timer = on);
            });
            let timer_label = label("Timer", "dim-label");
            row.add_suffix(&timer_label);
            row.add_suffix(&timer);
        }
        for (icon, tip, enabled, delta) in [
            ("go-up-symbolic", "Move up", index > 0, -1isize),
            ("go-down-symbolic", "Move down", index + 1 < count, 1),
        ] {
            let b = gtk::Button::from_icon_name(icon);
            b.add_css_class("flat");
            b.set_valign(gtk::Align::Center);
            b.set_tooltip_text(Some(tip));
            b.set_sensitive(enabled);
            let pp = p.clone();
            b.connect_clicked(move |_| {
                save_items(&pp, |items| {
                    items.swap(index, (index as isize + delta) as usize);
                })
            });
            row.add_suffix(&b);
        }
        let remove = gtk::Button::from_icon_name("list-remove-symbolic");
        remove.add_css_class("flat");
        remove.set_valign(gtk::Align::Center);
        remove.set_tooltip_text(Some(
            "Take it out of the video (the clip stays in the Library)",
        ));
        let pp = p.clone();
        remove.connect_clicked(move |_| {
            save_items(&pp, |items| {
                items.remove(index);
            })
        });
        row.add_suffix(&remove);
        group.add(&row);
    }
    p.detail.append(&group);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let delete = gtk::Button::with_label("Delete video…");
    delete.add_css_class("destructive-action");
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let build = gtk::Button::with_label(if video.project.is_some() {
        "Build again in Resolve"
    } else {
        "Build in Resolve"
    });
    build.add_css_class("suggested-action");
    build.set_sensitive(!video.items.is_empty());
    build.set_tooltip_text(Some(
        "Make a Resolve project with these clips back to back (timers rendered, NAS clips copied here first); see Jobs",
    ));
    actions.append(&delete);
    actions.append(&spacer);
    actions.append(&build);
    p.detail.append(&actions);
    drop(s);

    let (pp, id) = (p.clone(), video.id);
    build.connect_clicked(move |_| pp.shared.build_video(id));
    let (pp, title) = (p.clone(), video.title.clone());
    delete.connect_clicked(move |_| {
        let dialog = adw::AlertDialog::new(
            Some("Delete this video?"),
            Some(&format!(
                "{title} leaves the catalog, with what it used. Its clips stay in the Library, and the Resolve project isn't touched."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        let p2 = pp.clone();
        dialog.connect_response(None, move |_, r| {
            if r == "delete" {
                match Library::open_default().and_then(|l| l.delete_video(id)) {
                    Ok(()) => p2.shared.videos_changed(),
                    Err(e) => p2.status.set_text(&format!("Not deleted: {e:#}")),
                }
            }
        });
        dialog.present(Some(&pp.shared.window));
    });
}

/// An item's subtitle: when it was recorded, its length, and its run.
fn item_text(item: &Item, clip: Option<&Clip>, run: Option<&RunLabel>) -> String {
    let mut parts = Vec::new();
    match clip {
        Some(c) => {
            if let Some(r) = &c.recorded {
                parts.push(r.get(..16).unwrap_or(r).to_owned());
            }
        }
        None => parts.push("not in the Library now".to_owned()),
    }
    let fps = clip
        .and_then(|c| c.info.as_ref())
        .map_or(30.0, |i| i.fps().max(1.0));
    let seconds = (item.frames as f64 / fps).round() as i64;
    parts.push(format!("{}:{:02}", seconds / 60, seconds % 60));
    if let Some(r) = run {
        parts.push(if r.course.is_empty() {
            "timed".to_owned()
        } else {
            r.course.clone()
        });
    }
    parts.join(" · ")
}

/// Row titles and subtitles are markup.
fn glib_escape(text: &str) -> String {
    gtk::glib::markup_escape_text(text).to_string()
}

fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    if !class.is_empty() {
        l.add_css_class(class);
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_say_when_how_long_and_which_run() {
        let item = Item {
            frames: 3660,
            ..Item::default()
        };
        let clip = Clip {
            recorded: Some("2026-11-22 09:30:12".into()),
            info: Some(library::MediaInfo {
                codec: "h264".into(),
                width: 1920,
                height: 1080,
                fps_num: 60,
                fps_den: 1,
                frames: None,
                duration_seconds: None,
                timecode: None,
                rotation: 0,
                creation_time: None,
                has_audio: true,
            }),
            ..Clip::default()
        };
        let run = RunLabel {
            id: "r1".into(),
            course: "Ridge".into(),
        };
        assert_eq!(
            item_text(&item, Some(&clip), Some(&run)),
            "2026-11-22 09:30 · 1:01 · Ridge"
        );
        assert_eq!(
            item_text(&item, None, None),
            "not in the Library now · 2:02"
        );
    }
}
