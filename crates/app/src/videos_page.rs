//! The Videos page (decision 0005): the videos in the catalog
//! (`library::videos`), newest first, and the one picked: its title, status
//! (planned, in Resolve, published), YouTube link and date, the Resolve
//! project it was built as, and its clips in order, each with a Timer switch
//! when it's a timed run (its overlay goes over the timed part). Build in
//! Resolve hands it to the jobs drawer (`Shared::build_video`), which renders
//! the timers wanted, copies NAS clips down and makes the project.
//!
//! Clips are added from the Library (Organise → Add to video…), or a
//! timeline made by hand is linked (Link open timeline: the bridge's
//! `read_timeline`, matched with `videos::match_timeline`). Read back records
//! what the final timeline uses; setting a video to Published reads it back
//! if Resolve has it open, then offers to remove the local copies of its
//! clips that no unfinished video needs (`videos::freeable`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use library::videos::{Item, OnTimeline, Status, Use, Video};
use library::{Clip, Library};

use crate::shared::{Change, Shared};

/// A clip's committed run, as a video item shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct RunLabel {
    pub id: String,
    pub course: String,
}

/// What a bridge thread sends back.
enum Reply {
    /// Text for the footer; then reload.
    Done(String),
    /// A video linked: show it.
    Linked(i64, String),
    Error(String),
    /// After publishing: the video's clips whose local copies can go, and
    /// what the read-back said first.
    Freeable(Vec<Clip>, String),
}

#[derive(Default)]
struct State {
    videos: Vec<Video>,
    usage: HashMap<i64, Vec<Use>>,
    /// Videos whose usage was read from their timeline.
    read_back: HashSet<i64>,
    /// A bridge call is running.
    busy: bool,
    /// Kind id → its name.
    kinds: HashMap<String, String>,
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
    link: gtk::Button,
    replies: mpsc::Sender<Reply>,
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
    let link = gtk::Button::with_label("Link open timeline");
    link.set_tooltip_text(Some(
        "Make a video of the timeline open in Resolve, with the clips it uses",
    ));
    header.pack_end(&link);
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
        "Tick clips in the Library and choose Organise → Add to video…, start one here with New video…, or link a timeline you've made by hand: open it in Resolve and press Link open timeline.",
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

    let (replies, rx) = mpsc::channel();
    let page = Page {
        shared: shared.clone(),
        state: Rc::default(),
        list: list.clone(),
        stack,
        detail,
        status,
        link: link.clone(),
        replies,
        filling: Rc::default(),
    };
    let p = page.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(150), move || {
        for reply in rx.try_iter() {
            handle(&p, reply);
        }
        gtk::glib::ControlFlow::Continue
    });
    let p = page.clone();
    link.connect_clicked(move |_| link_open_timeline(&p));

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
    new.connect_clicked(move |_| new_video_dialog(&p));
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
    let read = library.and_then(|l| Ok((l.videos()?, l.clips()?, l.usage()?, l.read_back()?)));
    let (videos, clips, usage, read_back) = match read {
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
    s.kinds = library::kinds::all(p.shared.settings().kinds_folder.as_deref())
        .0
        .into_iter()
        .map(|k| (k.id, k.name))
        .collect();
    s.usage = usage;
    s.read_back = read_back;
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
            &{
                let n = v.items.iter().filter(|i| i.card.is_none()).count();
                let mut text = format!(
                    "{} · {n} {}",
                    v.status.label(),
                    if n == 1 { "clip" } else { "clips" }
                );
                if let Some(kind) = s.kinds.get(&v.kind) {
                    text = format!("{kind} · {text}");
                }
                text
            },
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
    let was = video.status;
    status.connect_selected_notify(move |row| {
        let wanted = Status::ALL[row.selected() as usize];
        save(&pp, |v| {
            if wanted == Status::Published && v.published.is_none() {
                v.published = Some(chrono::Local::now().format("%Y-%m-%d").to_string());
            }
            v.status = wanted;
        });
        if wanted == Status::Published && was != Status::Published {
            published(&pp);
        }
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
    let used = s.usage.get(&video.id).map_or(0, |u| u.len());
    resolve.set_subtitle(&glib_escape(&match (&video.project, &video.timeline) {
        (Some(project), Some(timeline)) => format!(
            "Project {project}, timeline {timeline} · {used} {} {}",
            if used == 1 { "clip" } else { "clips" },
            if s.read_back.contains(&video.id) {
                "on the timeline when last read"
            } else {
                "as built"
            }
        ),
        _ => "Not built yet".to_owned(),
    }));
    if video.project.is_some() {
        let read = gtk::Button::with_label("Read back");
        read.set_valign(gtk::Align::Center);
        read.set_sensitive(!s.busy);
        read.set_tooltip_text(Some(
            "Record which clips the timeline uses now (open its project in Resolve first)",
        ));
        let (pp, id) = (p.clone(), video.id);
        read.connect_clicked(move |_| read_back(&pp, id, false));
        resolve.add_suffix(&read);
    }
    about.add(&resolve);
    p.detail.append(&about);

    let group = adw::PreferencesGroup::new();
    group.set_title(if video.items.iter().any(|i| i.card.is_some()) {
        "Clips and cards"
    } else {
        "Clips"
    });
    group.set_description(Some(if video.items.is_empty() {
        "None yet: tick clips in the Library and choose Organise → Add to video…."
    } else {
        "In this order. Timer puts the run's overlay over its timed part. Trim in Resolve."
    }));
    let count = video.items.len();
    for (index, item) in video.items.iter().enumerate() {
        let clip = s.clips.get(&item.clip);
        let row = adw::ActionRow::new();
        if let Some(card) = &item.card {
            row.set_title(card.kind.label());
            let mut parts = vec![card.text.clone()];
            if !card.subtitle.is_empty() {
                parts.push(card.subtitle.clone());
            }
            parts.push(format!("{} s", card.seconds));
            row.set_subtitle(&glib_escape(&parts.join(" · ")));
            let edit = gtk::Button::from_icon_name("document-edit-symbolic");
            edit.add_css_class("flat");
            edit.set_valign(gtk::Align::Center);
            edit.set_tooltip_text(Some("Change its words and length"));
            let (pp, card) = (p.clone(), card.clone());
            edit.connect_clicked(move |_| edit_card(&pp, index, card.clone()));
            row.add_suffix(&edit);
        }
        let name = item
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let run = clip.and_then(|c| s.runs.get(&crate::library_page::run_key(&c.path)));
        if item.card.is_none() {
            row.set_title(&glib_escape(&name));
            let mut subtitle = item_text(item, clip, run);
            if let Some(c) = &item.caption {
                subtitle.push_str(&format!(" · “{c}”"));
            }
            row.set_subtitle(&glib_escape(&subtitle));
            let caption = gtk::Button::from_icon_name("insert-text-symbolic");
            caption.add_css_class("flat");
            caption.set_valign(gtk::Align::Center);
            caption.set_tooltip_text(Some(
                "A caption over the start of this clip, like \"Attempt 3 · Landed\"",
            ));
            caption.update_property(&[gtk::accessible::Property::Label(&format!(
                "Caption on {name}"
            ))]);
            let (pp, current) = (p.clone(), item.caption.clone().unwrap_or_default());
            caption.connect_clicked(move |_| edit_caption(&pp, index, &current));
            row.add_suffix(&caption);
        }
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

/// Runs `job` on a thread with the Resolve bridge; its reply comes back to
/// [`handle`].
fn with_bridge(
    p: &Page,
    doing: &str,
    job: impl FnOnce(&crate::resolve::Bridge) -> Reply + Send + 'static,
) {
    if std::mem::replace(&mut p.state.borrow_mut().busy, true) {
        return;
    }
    p.link.set_sensitive(false);
    p.status.set_text(doing);
    show(p);
    let container = p.shared.settings().resolve_container.clone();
    let tx = p.replies.clone();
    std::thread::spawn(move || {
        let bridge = crate::resolve::Bridge::new(container.as_deref());
        let _ = tx.send(job(&bridge));
    });
}

fn handle(p: &Page, reply: Reply) {
    p.state.borrow_mut().busy = false;
    p.link.set_sensitive(true);
    match reply {
        Reply::Done(text) => p.status.set_text(&text),
        Reply::Linked(id, text) => {
            p.status.set_text(&text);
            p.state.borrow_mut().selected = Some(id);
        }
        Reply::Error(text) => p.status.set_text(&text),
        Reply::Freeable(clips, note) => {
            p.status.set_text(&note);
            if !clips.is_empty() {
                offer_to_free(p, clips, note);
            }
        }
    }
    p.shared.videos_changed();
}

/// What the bridge reads from a timeline: project, timeline, the clips on it.
fn read_timeline(
    bridge: &crate::resolve::Bridge,
    project: Option<&str>,
    timeline: Option<&str>,
) -> Result<(String, String, Vec<OnTimeline>)> {
    let reply = bridge.request(&serde_json::json!({
        "command": "read_timeline",
        "project": project,
        "timeline": timeline,
    }))?;
    let found = reply["items"]
        .as_array()
        .context("Resolve's reply had no items")?
        .iter()
        .filter_map(|i| {
            Some(OnTimeline {
                path: i["path"].as_str()?.into(),
                first_frame: i["first_frame"].as_i64()?,
                frames: i["frames"].as_i64()?,
            })
        })
        .collect();
    Ok((
        reply["project"].as_str().unwrap_or_default().to_owned(),
        reply["timeline"].as_str().unwrap_or_default().to_owned(),
        found,
    ))
}

/// Reads `video`'s timeline and records what it uses; returns a sentence.
fn record_timeline(bridge: &crate::resolve::Bridge, video: i64) -> Result<String> {
    let library = Library::open_default()?;
    let v = library
        .video(video)?
        .context("that video isn't in the catalog any more")?;
    let (_, _, found) = read_timeline(bridge, v.project.as_deref(), v.timeline.as_deref())?;
    let (items, missing) = library::videos::match_timeline(&library.clips()?, &found);
    let uses: Vec<Use> = items
        .iter()
        .map(|i| Use {
            video,
            clip: i.clip.clone(),
            first_frame: i.first_frame,
            frames: i.frames,
        })
        .collect();
    library.set_usage(video, "timeline", &uses)?;
    Ok(format!(
        "Read {}: {} {} from the Library{}.",
        v.timeline.as_deref().unwrap_or("its timeline"),
        uses.len(),
        if uses.len() == 1 { "clip" } else { "clips" },
        missing_text(&missing)
    ))
}

fn missing_text(missing: &[String]) -> String {
    match missing {
        [] => String::new(),
        [one] => format!(", and {one}, which isn't in it"),
        [first, rest @ ..] => format!(
            ", and {} files that aren't in it ({first}…)",
            rest.len() + 1
        ),
    }
}

/// Read back: what `video`'s timeline uses now. With `then_free`, after it
/// is published: then the local copies it alone needed.
fn read_back(p: &Page, video: i64, then_free: bool) {
    let roots = crate::library_page::roots();
    with_bridge(p, "Reading the timeline in Resolve…", move |bridge| {
        let read = record_timeline(bridge, video);
        if !then_free {
            return match read {
                Ok(text) => Reply::Done(text),
                Err(e) => Reply::Error(format!("Not read: {e:#}")),
            };
        }
        let note = match read {
            Ok(text) => text,
            Err(e) => format!("Its timeline wasn't read ({e:#}), so this goes by what was built."),
        };
        match freeable_clips(video, &roots) {
            Ok(clips) => Reply::Freeable(clips, note),
            Err(e) => Reply::Error(format!("{note} {e:#}")),
        }
    });
}

/// Once a video is published: read its timeline back if it has one, then
/// offer to free its clips' local copies.
fn published(p: &Page) {
    let s = p.state.borrow();
    let Some(video) = s.videos.iter().find(|v| Some(v.id) == s.selected).cloned() else {
        return;
    };
    drop(s);
    if video.project.is_some() {
        read_back(p, video.id, true);
        return;
    }
    match freeable_clips(video.id, &crate::library_page::roots()) {
        Ok(clips) if !clips.is_empty() => offer_to_free(p, clips, String::new()),
        Ok(_) => {}
        Err(e) => p.status.set_text(&format!("{e:#}")),
    }
}

/// `video`'s clips that no unfinished video needs and that have a local copy
/// of a NAS original here.
fn freeable_clips(video: i64, roots: &library::footage::Roots) -> Result<Vec<Clip>> {
    let library = Library::open_default()?;
    let keys = library::videos::freeable(video, &library.videos()?, &library.usage()?);
    Ok(library
        .clips()?
        .into_iter()
        .filter(|c| keys.contains(&library::organise::key(c)))
        .filter(|c| roots.on_nas(&c.path) && c.local.as_ref().is_some_and(|l| l.is_file()))
        .collect())
}

/// Asks whether to remove these clips' local copies.
fn offer_to_free(p: &Page, clips: Vec<Clip>, note: String) {
    let roots = crate::library_page::roots();
    let bytes = library::hydrate::local_bytes(&clips, &roots);
    let n = clips.len();
    let dialog = adw::AlertDialog::new(
        Some("Remove the local copies?"),
        Some(&format!(
            "{}{} here ({}) that no unfinished video needs. The originals stay on the NAS; Copy here in the Library brings them back.",
            if note.is_empty() {
                String::new()
            } else {
                format!("{note}\n\n")
            },
            if n == 1 {
                "One of this video's clips has a local copy".to_owned()
            } else {
                format!("{n} of this video's clips have local copies")
            },
            library::hydrate::size_text(bytes)
        )),
    );
    dialog.add_responses(&[("keep", "Keep them"), ("remove", "Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("keep");
    let p2 = p.clone();
    dialog.connect_response(None, move |_, r| {
        if r != "remove" {
            return;
        }
        let (clips, roots) = (clips.clone(), roots.clone());
        p2.state.borrow_mut().busy = true;
        p2.status.set_text("Removing local copies…");
        let tx = p2.replies.clone();
        std::thread::spawn(move || {
            let reply = match Library::open_default() {
                Ok(l) => {
                    Reply::Done(library::hydrate::remove_local_copies(&l, &clips, &roots).text())
                }
                Err(e) => Reply::Error(format!("Not removed: {e:#}")),
            };
            let _ = tx.send(reply);
        });
    });
    dialog.present(Some(&p.shared.window));
}

/// Makes a video of the timeline open in Resolve (or brings an existing
/// video of it up to date), with the clips it uses.
fn link_open_timeline(p: &Page) {
    with_bridge(p, "Reading the open timeline in Resolve…", |bridge| {
        let linked = (|| -> Result<(i64, String)> {
            let (project, timeline, found) = read_timeline(bridge, None, None)?;
            let library = Library::open_default()?;
            let (items, missing) = library::videos::match_timeline(&library.clips()?, &found);
            let existing = library.videos()?.into_iter().find(|v| {
                v.project.as_deref() == Some(project.as_str())
                    && v.timeline.as_deref() == Some(timeline.as_str())
            });
            let mut video = match existing {
                Some(v) => v,
                None => {
                    let id = library.new_video(&timeline, "clips")?;
                    library.video(id)?.context("the new video went missing")?
                }
            };
            video.project = Some(project.clone());
            video.timeline = Some(timeline.clone());
            if video.status == Status::Planned {
                video.status = Status::InResolve;
            }
            library.save_video(&video)?;
            if video.items.is_empty() {
                // Each clip once, in the order first used.
                let mut seen = HashSet::new();
                let firsts: Vec<Item> = items
                    .iter()
                    .filter(|i| seen.insert(i.clip.clone()))
                    .cloned()
                    .collect();
                library.set_items(video.id, &firsts)?;
            }
            let uses: Vec<Use> = items
                .iter()
                .map(|i| Use {
                    video: video.id,
                    clip: i.clip.clone(),
                    first_frame: i.first_frame,
                    frames: i.frames,
                })
                .collect();
            library.set_usage(video.id, "timeline", &uses)?;
            Ok((
                video.id,
                format!(
                    "Linked {timeline} in {project}: {} {} from the Library{}.",
                    uses.len(),
                    if uses.len() == 1 { "clip" } else { "clips" },
                    missing_text(&missing)
                ),
            ))
        })();
        match linked {
            Ok((id, text)) => Reply::Linked(id, text),
            Err(e) => Reply::Error(format!("Not linked: {e:#}")),
        }
    });
}

/// Asks for a card's words and length, and saves them.
fn edit_card(p: &Page, index: usize, card: library::videos::Card) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let text = gtk::Entry::builder()
        .text(&card.text)
        .activates_default(true)
        .build();
    text.update_property(&[gtk::accessible::Property::Label("Text")]);
    form.append(&text);
    let subtitle = gtk::Entry::builder()
        .text(&card.subtitle)
        .placeholder_text("Smaller line under it (optional)")
        .activates_default(true)
        .build();
    subtitle.update_property(&[gtk::accessible::Property::Label("Smaller line")]);
    if card.kind != library::videos::CardKind::Chart {
        form.append(&subtitle);
    }
    let seconds = gtk::SpinButton::with_range(1.0, 30.0, 0.5);
    seconds.set_value(card.seconds);
    seconds.update_property(&[gtk::accessible::Property::Label("Seconds")]);
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    line.append(&label("Seconds on screen", ""));
    line.append(&seconds);
    form.append(&line);
    let pp = p.clone();
    let focus = text.clone();
    crate::courses::ask(
        &p.shared,
        card.kind.label(),
        "What it says, and for how long.",
        "Save",
        Some(form.upcast_ref()),
        Some(focus.upcast_ref()),
        move || {
            let mut changed = card.clone();
            changed.text = text.text().trim().to_owned();
            changed.subtitle = subtitle.text().trim().to_owned();
            changed.seconds = seconds.value();
            save_items(&pp, |items| {
                if let Some(item) = items.get_mut(index) {
                    item.card = Some(changed);
                }
            });
        },
    );
}

/// Ids and what the new-video dialog calls them.
type Choices = Vec<(String, String)>;

/// Asks for a clip's caption (empty for none), and saves it.
fn edit_caption(p: &Page, index: usize, current: &str) {
    let entry = gtk::Entry::builder()
        .text(current)
        .placeholder_text("e.g. Attempt 3 · Landed (empty for none)")
        .activates_default(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Caption")]);
    let (pp, entry2) = (p.clone(), entry.clone());
    crate::courses::ask(
        &p.shared,
        "Caption",
        &format!(
            "Shown low over the first {} seconds of the clip.",
            crate::cards::CAPTION_SECONDS
        ),
        "Save",
        Some(entry.upcast_ref()),
        Some(entry.upcast_ref()),
        move || {
            let text = entry2.text().trim().to_owned();
            save_items(&pp, |items| {
                if let Some(item) = items.get_mut(index) {
                    item.caption = (!text.is_empty()).then_some(text);
                }
            });
        },
    );
}

/// New video: clips picked in the Library, or a kind made from a run, a
/// course, a tag, a collection, a ride or a feature (`library::kinds`).
fn new_video_dialog(p: &Page) {
    let settings = p.shared.settings().clone();
    let (kinds, errors) = library::kinds::all(settings.kinds_folder.as_deref());
    let db = settings
        .run_database
        .as_ref()
        .and_then(|path| timer_core::TimerDatabase::load(path).ok())
        .unwrap_or_default();
    let laps: HashMap<String, f64> = db
        .courses
        .iter()
        .flat_map(|c| timer_core::board::course_board(c, &db.runs).rows)
        .filter_map(|r| Some((r.run_id.clone(), r.lap_seconds?)))
        .collect();
    let course_name = |id: &str| {
        db.courses
            .iter()
            .find(|c| c.id == id)
            .map_or(id.to_owned(), |c| c.name.clone())
    };
    let mut runs: Vec<&timer_core::models::RunRecord> = db
        .runs
        .iter()
        .filter(|r| r.committed && !r.ignored)
        .collect();
    runs.sort_by(|a, b| (&b.date, &b.capture_time).cmp(&(&a.date, &a.capture_time)));
    let run_choices: Vec<(String, String)> = runs
        .iter()
        .map(|r| {
            let lap = laps
                .get(&r.id)
                .map(|l| format!(" · {}", library::kinds::lap_text(*l)))
                .unwrap_or_default();
            (
                r.id.clone(),
                format!("{} · {}{lap}", course_name(&r.course_id), r.date),
            )
        })
        .collect();
    let course_choices: Vec<(String, String)> = db
        .courses
        .iter()
        .filter_map(|c| {
            let n = runs.iter().filter(|r| r.course_id == c.id).count();
            (n > 0).then(|| {
                (
                    c.id.clone(),
                    format!("{} ({n} {})", c.name, if n == 1 { "run" } else { "runs" }),
                )
            })
        })
        .collect();
    let (clips, catalog) = Library::open_default()
        .and_then(|l| Ok((l.clips()?, l.catalog()?)))
        .unwrap_or_default();
    let counted = |n: usize| format!("{n} {}", if n == 1 { "clip" } else { "clips" });
    let tag_choices: Vec<(String, String)> = catalog
        .all_tags()
        .into_iter()
        .map(|(t, n)| (t.clone(), format!("{t} ({})", counted(n))))
        .collect();
    let collection_choices: Vec<(String, String)> = catalog
        .collections
        .iter()
        .map(|c| {
            (
                c.id.to_string(),
                format!("{} ({})", c.name, counted(c.clips.len())),
            )
        })
        .collect();
    let ride_choices: Vec<(String, String)> = catalog
        .rides(&clips)
        .into_iter()
        .map(|r| {
            (
                r.id.clone(),
                format!("{} ({})", r.title(), counted(r.clips.len())),
            )
        })
        .collect();
    let feature_choices: Vec<(String, String)> = catalog
        .features
        .iter()
        .map(|f| {
            let n = f.attempts.len();
            (
                f.id.to_string(),
                format!(
                    "{} ({n} {})",
                    f.name,
                    if n == 1 { "attempt" } else { "attempts" }
                ),
            )
        })
        .collect();
    use library::kinds::Input;
    // Each input's choices, the dropdown for them, and what to say when
    // there are none.
    let inputs: Vec<(Input, Choices, &str, &str)> = vec![
        (
            Input::Run,
            run_choices,
            "Run",
            "No stored runs yet: time one first.",
        ),
        (
            Input::Course,
            course_choices,
            "Course",
            "No course has stored runs yet.",
        ),
        (
            Input::Tag,
            tag_choices,
            "Tag",
            "No clips are tagged yet (Library → Organise → Tags…).",
        ),
        (
            Input::Collection,
            collection_choices,
            "Collection",
            "No collections yet (Library → Organise → Add to collection…).",
        ),
        (
            Input::Ride,
            ride_choices,
            "Ride",
            "No rides in the Library yet.",
        ),
        (
            Input::Feature,
            feature_choices,
            "Feature",
            "No features yet (Library → Organise → Feature attempt…).",
        ),
    ];

    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let mut names = vec!["Clips you pick in the Library"];
    names.extend(kinds.iter().map(|k| k.name.as_str()));
    let kind = gtk::DropDown::from_strings(&names);
    kind.update_property(&[gtk::accessible::Property::Label("Kind")]);
    form.append(&kind);
    let about = label("", "dim-label");
    about.set_wrap(true);
    about.set_max_width_chars(40);
    form.append(&about);
    let pickers: Rc<Vec<(Input, Choices, gtk::DropDown, String)>> = Rc::new(
        inputs
            .into_iter()
            .map(|(input, choices, name, none)| {
                let pick = gtk::DropDown::from_strings(
                    &choices.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(),
                );
                pick.update_property(&[gtk::accessible::Property::Label(name)]);
                form.append(&pick);
                (input, choices, pick, none.to_owned())
            })
            .collect(),
    );
    let title = gtk::Entry::builder().activates_default(true).build();
    title.update_property(&[gtk::accessible::Property::Label("Title")]);
    form.append(&title);
    if !errors.is_empty() {
        let e = label(&format!("Not read: {}", errors.join("; ")), "error");
        e.set_wrap(true);
        form.append(&e);
    }
    let kinds = Rc::new(kinds);
    let show_kind = {
        let (kinds, about, pickers, title) =
            (kinds.clone(), about.clone(), pickers.clone(), title.clone());
        move |picked: u32| {
            let k = (picked as usize).checked_sub(1).and_then(|i| kinds.get(i));
            let input = k.map(|k| k.input);
            about.set_text(&match k {
                None => "Name it, then tick its clips in the Library and choose Organise → Add to video….".to_owned(),
                Some(k) => match pickers.iter().find(|(i, ..)| Some(*i) == input) {
                    Some((_, choices, _, none)) if choices.is_empty() => {
                        format!("{} {none}", k.description)
                    }
                    _ => k.description.clone(),
                },
            });
            for (i, _, pick, _) in pickers.iter() {
                pick.set_visible(Some(*i) == input);
            }
            title.set_placeholder_text(Some(if k.is_some() {
                "Title (leave empty for the kind's)"
            } else {
                "Title, e.g. Ridge laps, November"
            }));
        }
    };
    show_kind(0);
    kind.connect_selected_notify(move |d| show_kind(d.selected()));
    let pp = p.clone();
    let (kind2, title2) = (kind.clone(), title.clone());
    crate::courses::ask(
        &p.shared,
        "New video",
        "",
        "Make",
        Some(form.upcast_ref()),
        Some(title.upcast_ref()),
        move || {
            let typed = title2.text().trim().to_owned();
            let picked = (kind2.selected() as usize)
                .checked_sub(1)
                .and_then(|i| kinds.get(i));
            let made = (|| -> Result<i64> {
                let library = Library::open_default()?;
                let Some(k) = picked else {
                    return library.new_video(&typed, "clips");
                };
                let input = pickers
                    .iter()
                    .find(|(i, ..)| *i == k.input)
                    .and_then(|(_, choices, pick, _)| choices.get(pick.selected() as usize))
                    .map(|(id, _)| id.clone())
                    .context("there's nothing to make it from yet")?;
                let mut strava = library.strava_index()?;
                library::strava::learn(&mut strava, &db, &library.clips()?);
                let plan = library::kinds::plan(
                    k,
                    &input,
                    library::kinds::Sources {
                        db: &db,
                        clips: &library.clips()?,
                        catalog: &library.catalog()?,
                        strava: Some(&strava),
                    },
                )?;
                let id = library.new_video(
                    if typed.is_empty() {
                        &plan.title
                    } else {
                        &typed
                    },
                    &k.id,
                )?;
                library.set_items(id, &plan.items)?;
                Ok(id)
            })();
            match made {
                Ok(id) => {
                    pp.state.borrow_mut().selected = Some(id);
                    pp.status.set_text("");
                    pp.shared.videos_changed();
                }
                Err(e) => pp.status.set_text(&format!("Not made: {e:#}")),
            }
        },
    );
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
