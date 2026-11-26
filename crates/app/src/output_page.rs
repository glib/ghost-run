//! The jobs drawer (was the Output page; opened from the sidebar's Jobs row,
//! and by itself when runs are sent here): a queue of runs whose timer
//! overlays to render. Runs come
//! from the Library or Tracks (Render overlays) or "Add unrendered"; each
//! renders in turn on a worker thread, using every core the machine's memory
//! allows ([`timer_render::encode::plan`]). A finished overlay is recorded in
//! the library, so the Resolve bridge can find it.
//!
//! Runs sent to Resolve from the Library or Tracks come here too: overlays already
//! rendered for the run's current markers are used as they are, the rest are
//! rendered first, then they all go to Resolve together.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use library::settings::Comparison;
use library::{Library, Rendered, auto, overlays};
use timer_core::TimerDatabase;
use timer_core::overlay::{ComparisonMode, OverlayPayload, SelectedRun, overlay_payload};
use timer_render::job::{self, RenderError, Spec};

use crate::shared::{Change, ResolveTarget, Shared};

/// A render for the worker.
struct Job {
    run_id: String,
    payload: OverlayPayload,
    clip: PathBuf,
    out: PathBuf,
    fingerprint: String,
}

enum Reply {
    Progress(String, f64),
    Finished(String, Result<Rendered, String>),
    /// What Resolve said: its status, or what adding overlays did.
    Resolve(Result<String, String>),
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Queued,
    Rendering(f64),
    Done(String),
    Failed(String),
    Stopped,
}

#[derive(Debug, Clone)]
struct Item {
    run_id: String,
    course: String,
    clip: Option<PathBuf>,
    out: Option<PathBuf>,
    status: Status,
}

/// What to do in Resolve once a set of runs has its overlays.
#[derive(Debug, Clone, PartialEq)]
enum Send {
    Timeline,
    NewProject(String),
}

struct State {
    items: Vec<Item>,
    running: bool,
    /// Runs waiting on their renders before going to Resolve.
    pending: Option<(Send, Vec<String>)>,
}

#[derive(Clone)]
struct Page {
    shared: Shared,
    state: Rc<RefCell<State>>,
    jobs: mpsc::Sender<Job>,
    cancel: Arc<AtomicBool>,
    views: gtk::Stack,
    list: gtk::ListBox,
    /// Run id → its row's progress bar, to update in place.
    bars: Rc<RefCell<HashMap<String, gtk::ProgressBar>>>,
    folder: gtk::Label,
    start: gtk::Button,
    stop: gtk::Button,
    clear: gtk::Button,
    status: gtk::Label,
    resolve_status: gtk::Label,
    resolve_buttons: gtk::Box,
    add_to_resolve: gtk::Button,
    new_project: gtk::Button,
    replies: mpsc::Sender<Reply>,
    /// The sidebar's one-line summary of the jobs.
    activity: gtk::Label,
}

/// The drawer's content. `activity` is kept to a short summary ("2 to
/// render", "3 done"); `close` hides the drawer.
pub fn page(shared: &Shared, activity: gtk::Label, close: impl Fn() + 'static) -> gtk::Widget {
    let (jobs, replies, reply_tx) = worker();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Jobs", "Overlays and Resolve")));
    header.set_show_start_title_buttons(false);
    let close_button = gtk::Button::from_icon_name("go-next-symbolic");
    close_button.set_tooltip_text(Some("Hide jobs"));
    close_button.connect_clicked(move |_| close());
    header.pack_start(&close_button);
    root.append(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("content");
    content.set_vexpand(true);

    let options = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    options.append(&label("Compare with", ""));
    let compare = gtk::DropDown::from_strings(&["Best lap so far", "Optimal lap so far"]);
    compare.update_property(&[gtk::accessible::Property::Label(
        "What each overlay compares the run with",
    )]);
    compare.set_tooltip_text(Some(
        "Best lap: the fastest run before this one. Optimal: the fastest of each sector before it, added up.",
    ));
    options.append(&compare);
    content.append(&options);
    let where_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let folder = label("", "dim-label");
    folder.set_hexpand(true);
    folder.set_wrap(true);
    where_row.append(&folder);
    let change_folder = button("Change…");
    change_folder.set_valign(gtk::Align::Center);
    change_folder.set_tooltip_text(Some("Choose where overlays go (Settings)"));
    where_row.append(&change_folder);
    content.append(&where_row);

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let add = button("Add unrendered");
    add.set_tooltip_text(Some(
        "Queue every counted run whose overlay hasn't been rendered, or is out of date",
    ));
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let clear = button("Clear done");
    clear.set_tooltip_text(Some("Take finished renders off the list"));
    let stop = button("Stop");
    stop.set_visible(false);
    let start = button("Render");
    start.add_css_class("suggested-action");
    toolbar.append(&add);
    toolbar.append(&spacer);
    toolbar.append(&clear);
    toolbar.append(&stop);
    toolbar.append(&start);
    content.append(&toolbar);

    let views = gtk::Stack::new();
    views.set_vexpand(true);
    let empty = adw::StatusPage::new();
    empty.set_icon_name(Some("video-x-generic-symbolic"));
    empty.set_title("Nothing to render");
    empty.set_description(Some(
        "Tick clips in the Library (or runs on Tracks) and press Render overlays, or add every run that has no overlay yet.",
    ));
    views.add_named(&empty, Some("empty"));
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_valign(gtk::Align::Start);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    views.add_named(&scroller, Some("list"));
    content.append(&views);

    let resolve_row = gtk::Box::new(gtk::Orientation::Vertical, 8);
    resolve_row.add_css_class("locations");
    let resolve_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    resolve_text.set_hexpand(true);
    resolve_text.append(&label("DaVinci Resolve", "heading"));
    let resolve_status = label("Not checked yet.", "dim-label");
    resolve_status.set_wrap(true);
    resolve_text.append(&resolve_status);
    resolve_row.append(&resolve_text);
    let resolve_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    resolve_buttons.set_halign(gtk::Align::End);
    let check = button("Check");
    check.set_tooltip_text(Some(
        "See whether Resolve is running and which project is open",
    ));
    let add_to_resolve = button("Add to open timeline");
    add_to_resolve.set_tooltip_text(Some(
        "Put each finished overlay on the track above its clip in Resolve's current timeline, adding clips that aren't there yet at its end",
    ));
    let new_project = button("New project…");
    new_project.set_tooltip_text(Some(
        "Start a Resolve project with the finished runs back to back: clips on V1, overlays on V2, proxies linked",
    ));
    resolve_buttons.append(&check);
    resolve_buttons.append(&new_project);
    resolve_buttons.append(&add_to_resolve);
    resolve_row.append(&resolve_buttons);
    content.append(&resolve_row);
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
            items: Vec::new(),
            running: false,
            pending: None,
        })),
        jobs: jobs.0,
        cancel: jobs.1,
        views,
        list,
        bars: Rc::default(),
        folder,
        start: start.clone(),
        stop: stop.clone(),
        clear: clear.clone(),
        status,
        resolve_status,
        resolve_buttons,
        add_to_resolve: add_to_resolve.clone(),
        new_project: new_project.clone(),
        replies: reply_tx,
        activity,
    };

    compare.set_selected(match shared.settings().comparison {
        Comparison::BestLap => 0,
        Comparison::Optimal => 1,
    });
    let p = page.clone();
    compare.connect_selected_notify(move |d| {
        let mode = if d.selected() == 1 {
            Comparison::Optimal
        } else {
            Comparison::BestLap
        };
        if p.shared.settings().comparison != mode {
            let _ = p.shared.update(|s| s.comparison = mode);
        }
    });
    let p = page.clone();
    change_folder.connect_clicked(move |_| crate::settings_dialog::open(&p.shared));
    let p = page.clone();
    add.connect_clicked(move |_| add_unrendered(&p));
    let p = page.clone();
    check.connect_clicked(move |_| ask_resolve(&p, None));
    let p = page.clone();
    new_project.connect_clicked(move |_| {
        let items = finished(&p);
        if items.is_empty() {
            return;
        }
        let p2 = p.clone();
        new_project_dialog(&p, items.len(), move |name| {
            make_project(&p2, name, items.clone())
        });
    });
    let p = page.clone();
    add_to_resolve.connect_clicked(move |_| {
        let items = finished(&p);
        if !items.is_empty() {
            ask_resolve(&p, Some(items));
        }
    });
    let p = page.clone();
    start.connect_clicked(move |_| {
        let mut s = p.state.borrow_mut();
        s.running = true;
        // Stopped renders start again from the beginning.
        for item in &mut s.items {
            if item.status == Status::Stopped {
                item.status = Status::Queued;
            }
        }
        drop(s);
        next(&p);
    });
    let p = page.clone();
    stop.connect_clicked(move |_| {
        p.cancel.store(true, Ordering::Relaxed);
        let mut s = p.state.borrow_mut();
        s.running = false;
        let dropped = s.pending.take().is_some();
        drop(s);
        p.status.set_text("Stopping…");
        if dropped {
            p.resolve_status.add_css_class("error");
            p.resolve_status
                .set_text("Stopped: nothing was sent to Resolve.");
        }
    });
    let p = page.clone();
    clear.connect_clicked(move |_| {
        p.state
            .borrow_mut()
            .items
            .retain(|i| matches!(i.status, Status::Queued | Status::Rendering(_)));
        show(&p);
    });
    let p = page.clone();
    shared.subscribe(move |change| {
        if change == Change::Render {
            let ids = p.shared.take_runs_to_render();
            queue(&p, &ids);
            p.state.borrow_mut().running = true;
            next(&p);
        }
        if change == Change::Resolve
            && let Some((target, ids)) = p.shared.take_runs_for_resolve()
            && !ids.is_empty()
        {
            match target {
                ResolveTarget::Timeline => begin(&p, Send::Timeline, ids),
                ResolveTarget::NewProject => {
                    let (p2, n) = (p.clone(), ids.len());
                    new_project_dialog(&p, n, move |name| {
                        begin(&p2, Send::NewProject(name), ids.clone())
                    });
                }
            }
        }
        show_folder(&p);
    });

    let p = page.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        for reply in replies.try_iter() {
            handle(&p, reply);
        }
        glib::ControlFlow::Continue
    });

    show_folder(&page);
    show(&page);
    root.upcast()
}

type Worker = (
    (mpsc::Sender<Job>, Arc<AtomicBool>),
    mpsc::Receiver<Reply>,
    mpsc::Sender<Reply>,
);

/// Renders each job sent, one at a time.
fn worker() -> Worker {
    let (tx, jobs) = mpsc::channel::<Job>();
    let (replies, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let others = replies.clone();
    std::thread::spawn(move || {
        for job in jobs {
            let id = job.run_id.clone();
            let result = render(&job, &stop, &replies);
            if replies.send(Reply::Finished(id, result)).is_err() {
                break;
            }
        }
    });
    ((tx, cancel), rx, others)
}

fn render(
    job: &Job,
    cancel: &AtomicBool,
    replies: &mpsc::Sender<Reply>,
) -> Result<Rendered, String> {
    let info = timer_render::encode::probe(&job.clip)
        .map_err(|e| format!("{} can't be read: {e}", job.clip.display()))?;
    if let Some(dir) = job.out.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    let spec = Spec::new(&job.payload, &job.out, Some(&info));
    let id = job.run_id.clone();
    let report = job::render(
        &spec,
        &|done, total| {
            let _ = replies.send(Reply::Progress(id.clone(), done as f64 / total as f64));
        },
        cancel,
    )
    .map_err(|e| match e {
        RenderError::Cancelled => "stopped".to_owned(),
        e => e.to_string(),
    })?;
    let rendered = Rendered {
        run_id: job.run_id.clone(),
        clip: job.clip.clone(),
        overlay: job.out.clone(),
        first_frame: report.first,
        timecode: report.timecode.clone(),
        fingerprint: job.fingerprint.clone(),
        rendered_ms: chrono::Utc::now().timestamp_millis(),
    };
    Library::open_default()
        .and_then(|l| l.set_rendered(&rendered))
        .map_err(|e| format!("rendered, but not recorded in the library: {e:#}"))?;
    Ok(rendered)
}

fn handle(p: &Page, reply: Reply) {
    match reply {
        Reply::Progress(id, fraction) => {
            if let Some(item) = p
                .state
                .borrow_mut()
                .items
                .iter_mut()
                .find(|i| i.run_id == id)
            {
                item.status = Status::Rendering(fraction);
            }
            if let Some(bar) = p.bars.borrow().get(&id) {
                bar.set_fraction(fraction);
            }
        }
        Reply::Finished(id, result) => {
            let mut s = p.state.borrow_mut();
            if let Some(item) = s.items.iter_mut().find(|i| i.run_id == id) {
                item.status = match result {
                    Ok(r) => Status::Done(format!(
                        "Starts at clip frame {} (timecode {})",
                        r.first_frame, r.timecode
                    )),
                    Err(e) if e == "stopped" => Status::Stopped,
                    Err(e) => Status::Failed(e),
                };
            }
            drop(s);
            next(p);
            settle(p);
        }
        Reply::Resolve(result) => {
            p.resolve_buttons.set_sensitive(true);
            match result {
                Ok(text) => {
                    p.resolve_status.remove_css_class("error");
                    p.resolve_status.set_text(&text);
                }
                Err(text) => {
                    p.resolve_status.add_css_class("error");
                    p.resolve_status.set_text(&text);
                }
            }
            show(p);
        }
    }
}

/// The finished overlays in the list, as the bridge wants them.
fn finished(p: &Page) -> Vec<(String, PathBuf, PathBuf)> {
    p.state
        .borrow()
        .items
        .iter()
        .filter(|i| matches!(i.status, Status::Done(_)))
        .filter_map(|i| Some((i.run_id.clone(), i.clip.clone()?, i.out.clone()?)))
        .collect()
}

/// Asks Resolve for its status, or (with `items`) to add those overlays, on
/// a thread of its own: Resolve can take a while to answer.
fn ask_resolve(p: &Page, items: Option<Vec<(String, PathBuf, PathBuf)>>) {
    let container = p.shared.settings().resolve_container.clone();
    let tx = p.replies.clone();
    p.resolve_buttons.set_sensitive(false);
    p.resolve_status.remove_css_class("error");
    p.resolve_status.set_text(match items {
        None => "Asking Resolve…",
        Some(_) => "Adding the overlays in Resolve (copying any clip only on the NAS here first)…",
    });
    std::thread::spawn(move || {
        let bridge = crate::resolve::Bridge::new(container.as_deref());
        let result = match items {
            None => bridge
                .request(&serde_json::json!({"command": "status"}))
                .map(|r| crate::resolve::describe_status(&r)),
            Some(items) => add_overlays(&bridge, &items),
        };
        let _ = tx.send(Reply::Resolve(result.map_err(|e| format!("{e:#}"))));
    });
}

fn add_overlays(
    bridge: &crate::resolve::Bridge,
    items: &[(String, PathBuf, PathBuf)],
) -> anyhow::Result<String> {
    let library = Library::open_default()?;
    let clips = library.clips()?;
    let mut requests = Vec::new();
    for (run_id, clip, overlay) in items {
        let first_frame = library
            .rendered(run_id)?
            .map(|r| r.first_frame)
            .ok_or_else(|| anyhow::anyhow!("{run_id}'s overlay isn't recorded"))?;
        let frames = library::probe::probe(overlay)
            .ok()
            .and_then(|i| i.frames)
            .ok_or_else(|| anyhow::anyhow!("{} can't be read", overlay.display()))?;
        // Sent in case the clip isn't on the timeline yet and the bridge adds it.
        let (clip, proxy) = ensure_local(&library, &clips, clip)?;
        requests.push(serde_json::json!({
            "clip": clip, "proxy": proxy, "overlay": overlay,
            "first_frame": first_frame, "frames": frames,
        }));
    }
    let reply = bridge.request(&serde_json::json!({
        "command": "add_overlays", "items": requests,
    }))?;
    Ok(describe_added(&reply))
}

/// Asks for the new project's name for `count` runs, then calls `create`.
fn new_project_dialog(p: &Page, count: usize, create: impl Fn(String) + 'static) {
    let dialog = adw::AlertDialog::new(
        Some("New Resolve project"),
        Some(&format!(
            "{count} {} back to back, in the order ridden: each clip from a second before Start to the end of its overlay on V1, the overlay above it on V2, proxies linked. It starts from Resolve's default project settings, at the first clip's frame rate and size.",
            if count == 1 { "run" } else { "runs" }
        )),
    );
    let entry = gtk::Entry::new();
    entry.set_text(&format!(
        "Ghost Run {}",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    entry.set_activates_default(true);
    entry.update_property(&[gtk::accessible::Property::Label("Project name")]);
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "create" {
            create(entry.text().trim().to_owned());
        }
    });
    dialog.present(Some(&p.shared.window));
}

/// Makes a Resolve project from these overlays, in the order ridden.
fn make_project(p: &Page, name: String, mut items: Vec<(String, PathBuf, PathBuf)>) {
    let container = p.shared.settings().resolve_container.clone();
    let order = ride_order(p);
    items.sort_by_key(|(id, _, _)| order.get(id).cloned().unwrap_or_default());
    let tx = p.replies.clone();
    p.resolve_buttons.set_sensitive(false);
    p.resolve_status.remove_css_class("error");
    p.resolve_status
        .set_text("Making the project in Resolve (copying any clip only on the NAS here first)…");
    std::thread::spawn(move || {
        let bridge = crate::resolve::Bridge::new(container.as_deref());
        let result = create_project(&bridge, &name, &items).map_err(|e| format!("{e:#}"));
        let _ = tx.send(Reply::Resolve(result));
    });
}

/// Runs sent to Resolve: lists them, renders the ones without an up-to-date
/// overlay, and sends them all once those are done ([`settle`]).
fn begin(p: &Page, send: Send, ids: Vec<String>) {
    let Some(path) = p.shared.settings().run_database.clone() else {
        p.status
            .set_text("Choose your run database first (the gear in the sidebar).");
        return;
    };
    let db = match TimerDatabase::load(&path) {
        Ok(db) => db,
        Err(e) => {
            p.status
                .set_text(&format!("The run database couldn't be read: {e}"));
            return;
        }
    };
    let renders = Library::open_default()
        .and_then(|l| l.renders())
        .unwrap_or_default();
    let (ready, stale) = split_rendered(&ids, &db, &renders);
    queue(p, &ids);
    let mut s = p.state.borrow_mut();
    for r in &ready {
        if let Some(item) = s.items.iter_mut().find(|i| i.run_id == r.run_id)
            && item.status == Status::Queued
        {
            item.clip = Some(r.clip.clone());
            item.out = Some(r.overlay.clone());
            item.status = Status::Done(format!(
                "Already rendered; starts at clip frame {} (timecode {})",
                r.first_frame, r.timecode
            ));
        }
    }
    s.pending = Some((send, ids));
    if !stale.is_empty() {
        s.running = true;
    }
    drop(s);
    p.resolve_status.remove_css_class("error");
    p.resolve_status.set_text(&match stale.len() {
        0 => "Sending the runs to Resolve…".to_owned(),
        n => format!(
            "Rendering {n} {} first, then sending the runs to Resolve…",
            if n == 1 { "overlay" } else { "overlays" }
        ),
    });
    next(p);
    settle(p);
}

/// Once none of the pending runs is waiting or rendering, sends the ones
/// with an overlay to Resolve.
fn settle(p: &Page) {
    let mut s = p.state.borrow_mut();
    let Some((_, ids)) = &s.pending else {
        return;
    };
    let waiting = s.items.iter().any(|i| {
        ids.contains(&i.run_id) && matches!(i.status, Status::Queued | Status::Rendering(_))
    });
    if waiting {
        return;
    }
    let (send, ids) = s.pending.take().expect("checked");
    let items: Vec<_> = s
        .items
        .iter()
        .filter(|i| ids.contains(&i.run_id) && matches!(i.status, Status::Done(_)))
        .filter_map(|i| Some((i.run_id.clone(), i.clip.clone()?, i.out.clone()?)))
        .collect();
    drop(s);
    let missing = ids.len() - items.len();
    if items.is_empty() {
        p.resolve_status.add_css_class("error");
        p.resolve_status.set_text(
            "None of the runs has an overlay, so nothing was sent to Resolve: see the list for why.",
        );
        return;
    }
    if missing > 0 {
        p.status.set_text(&format!(
            "{missing} of the {} runs couldn't be rendered and were left out (see the list).",
            ids.len()
        ));
    }
    match send {
        Send::Timeline => ask_resolve(p, Some(items)),
        Send::NewProject(name) => make_project(p, name, items),
    }
}

/// Whether `done` is the overlay for `run` as it is now: the file is there
/// and the run's markers haven't changed since.
fn is_current(done: &Rendered, run: &timer_core::models::RunRecord) -> bool {
    done.overlay.exists() && done.fingerprint == run.fingerprint.clone().unwrap_or_default()
}

/// `ids` split into the overlays already rendered and up to date, and the
/// runs that need rendering (in the order given).
fn split_rendered(
    ids: &[String],
    db: &TimerDatabase,
    renders: &HashMap<String, Rendered>,
) -> (Vec<Rendered>, Vec<String>) {
    let mut ready = Vec::new();
    let mut stale = Vec::new();
    for id in ids {
        let run = db.runs.iter().find(|r| &r.id == id);
        match (run, renders.get(id)) {
            (Some(run), Some(done)) if is_current(done, run) => ready.push(done.clone()),
            _ => stale.push(id.clone()),
        }
    }
    (ready, stale)
}

/// Run id → when it was ridden, to put runs in order.
fn ride_order(p: &Page) -> HashMap<String, (String, String)> {
    p.shared
        .settings()
        .run_database
        .clone()
        .and_then(|path| TimerDatabase::load(&path).ok())
        .map(|db| {
            db.runs
                .iter()
                .map(|r| {
                    (
                        r.id.clone(),
                        (timer_core::matching::run_order_time(r), r.id.clone()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn create_project(
    bridge: &crate::resolve::Bridge,
    name: &str,
    items: &[(String, PathBuf, PathBuf)],
) -> anyhow::Result<String> {
    let library = Library::open_default()?;
    let clips = library.clips()?;
    let mut runs = Vec::new();
    let mut format = None;
    for (run_id, clip, overlay) in items {
        let first_frame = library
            .rendered(run_id)?
            .map(|r| r.first_frame)
            .ok_or_else(|| anyhow::anyhow!("{run_id}'s overlay isn't recorded"))?;
        let frames = library::probe::probe(overlay)
            .ok()
            .and_then(|i| i.frames)
            .ok_or_else(|| anyhow::anyhow!("{} can't be read", overlay.display()))?;
        let known = clips.iter().find(|c| &c.path == clip);
        if format.is_none() {
            let info = match known.and_then(|c| c.info.clone()) {
                Some(i) => i,
                None => library::probe::probe(clip)?,
            };
            format = Some(info);
        }
        let (clip, proxy) = ensure_local(&library, &clips, clip)?;
        runs.push(serde_json::json!({
            "clip": clip, "proxy": proxy, "overlay": overlay,
            "first_frame": first_frame, "frames": frames,
        }));
    }
    let info = format.ok_or_else(|| anyhow::anyhow!("no runs"))?;
    // Upright size, as Resolve shows a rotated clip.
    let (width, height) = if info.rotation.rem_euclid(180) == 90 {
        (info.height, info.width)
    } else {
        (info.width, info.height)
    };
    let reply = bridge.request(&serde_json::json!({
        "command": "new_project",
        "name": name,
        "fps": timer_render::job::trim(info.fps()),
        "width": width,
        "height": height,
        "runs": runs,
    }))?;
    Ok(describe_project(&reply))
}

/// The file Resolve should use for a clip, and its proxy: for an original on
/// the NAS, its local copy, made now (with its proxy) if it has none yet.
fn ensure_local(
    library: &Library,
    clips: &[library::Clip],
    clip: &std::path::Path,
) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    let roots = crate::library_page::roots();
    let Some(known) = clips.iter().find(|c| c.path == clip) else {
        return Ok((clip.to_owned(), None));
    };
    if !roots.on_nas(&known.path) {
        return Ok((known.path.clone(), ready_proxy(clips, clip)));
    }
    if let Some(local) = known.local.as_ref().filter(|p| p.is_file()) {
        return Ok((local.clone(), ready_proxy(clips, clip)));
    }
    let cancel = library::Cancel::default();
    let local = library::hydrate::hydrate(known, &roots, &cancel, |_| {})?;
    let proxy = known
        .info
        .as_ref()
        .and_then(|info| library::proxy::make(&local, info, &cancel, |_| {}).ok());
    let mut updated = known.clone();
    library::scan::place(&mut updated, &roots);
    library.upsert(&updated)?;
    Ok((local, proxy))
}

/// The clip's proxy, if the library has one ready.
fn ready_proxy(clips: &[library::Clip], clip: &std::path::Path) -> Option<PathBuf> {
    clips
        .iter()
        .find(|c| c.path == clip)
        .and_then(|c| match &c.proxy {
            library::ProxyState::Ready(p) => Some(p.clone()),
            _ => None,
        })
}

fn describe_project(reply: &serde_json::Value) -> String {
    let placed = reply["placed"].as_u64().unwrap_or(0);
    let mut text = format!(
        "Made project {} with {placed} {} on timeline {}.",
        reply["project"].as_str().unwrap_or("?"),
        if placed == 1 { "run" } else { "runs" },
        reply["timeline"].as_str().unwrap_or("?"),
    );
    let notes: Vec<&str> = reply["notes"]
        .as_array()
        .map(|n| n.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if !notes.is_empty() {
        text.push_str(&format!(" {}.", notes.join("; ")));
    }
    text
}

/// What adding overlays did, in a sentence or two.
fn describe_added(reply: &serde_json::Value) -> String {
    let results = reply["results"].as_array().cloned().unwrap_or_default();
    let count = |key: &str| {
        results
            .iter()
            .map(|r| r[key].as_u64().unwrap_or(0))
            .sum::<u64>()
    };
    let (placed, already) = (count("placed"), count("already"));
    let clips_added = results
        .iter()
        .filter(|r| r["clip_added"].as_bool().unwrap_or(false))
        .count();
    let mut text = format!(
        "Added {placed} {} to {} in {}",
        if placed == 1 { "overlay" } else { "overlays" },
        reply["timeline"].as_str().unwrap_or("the timeline"),
        reply["project"].as_str().unwrap_or("Resolve"),
    );
    if already > 0 {
        text.push_str(&format!(" ({already} already there)"));
    }
    text.push('.');
    if clips_added > 0 {
        text.push_str(&format!(
            " {clips_added} {} on the timeline, so {} added at its end.",
            if clips_added == 1 {
                "clip wasn't"
            } else {
                "clips weren't"
            },
            if clips_added == 1 {
                "it was"
            } else {
                "they were"
            },
        ));
    }
    let named = |key: &str| -> Vec<String> {
        results
            .iter()
            .filter_map(|r| {
                let note = r[key].as_str()?;
                let name = std::path::Path::new(r["clip"].as_str().unwrap_or(""))
                    .file_name()?
                    .to_string_lossy()
                    .into_owned();
                Some(format!("{name}: {note}"))
            })
            .collect()
    };
    let proxies = named("proxy_note");
    if !proxies.is_empty() {
        text.push_str(&format!(" {}.", proxies.join("; ")));
    }
    let notes = named("note");
    if !notes.is_empty() {
        text.push_str(&format!(" Not added: {}.", notes.join("; ")));
    }
    text
}

/// Starts the next queued render, if running and nothing is rendering.
fn next(p: &Page) {
    let mut s = p.state.borrow_mut();
    let busy = s
        .items
        .iter()
        .any(|i| matches!(i.status, Status::Rendering(_)));
    if busy {
        drop(s);
        show(p);
        return;
    }
    if !s.running {
        drop(s);
        p.status.set_text(&summary(&p.state.borrow().items));
        show(p);
        return;
    }
    let Some(i) = s.items.iter().position(|i| i.status == Status::Queued) else {
        s.running = false;
        drop(s);
        p.status.set_text(&summary(&p.state.borrow().items));
        show(p);
        return;
    };
    let run_id = s.items[i].run_id.clone();
    drop(s);
    match prepare(p, &run_id) {
        Ok(job) => {
            let mut s = p.state.borrow_mut();
            s.items[i].clip = Some(job.clip.clone());
            s.items[i].out = Some(job.out.clone());
            s.items[i].status = Status::Rendering(0.0);
            drop(s);
            p.cancel.store(false, Ordering::Relaxed);
            p.status.set_text(&format!("Rendering {run_id}…"));
            let _ = p.jobs.send(job);
            show(p);
        }
        Err(e) => {
            p.state.borrow_mut().items[i].status = Status::Failed(e);
            next(p);
        }
    }
}

/// The render for `run_id`, from the database and library as they are now.
fn prepare(p: &Page, run_id: &str) -> Result<Job, String> {
    let settings = p.shared.settings().clone();
    let path = settings
        .run_database
        .ok_or("no run database is chosen (Settings)")?;
    let db = TimerDatabase::load(&path).map_err(|e| format!("the run database: {e}"))?;
    let run = db
        .runs
        .iter()
        .find(|r| r.id == run_id)
        .ok_or("the run is no longer in the database")?;
    let mode = match settings.comparison {
        Comparison::BestLap => ComparisonMode::BestLap,
        Comparison::Optimal => ComparisonMode::Optimal,
    };
    let payload = overlay_payload(&db, &SelectedRun::from_run(run), mode)
        .map_err(|e| format!("can't make its overlay: {e}"))?;
    let clips = Library::open_default()
        .and_then(|l| l.clips())
        .map_err(|e| format!("{e:#}"))?;
    let clip = auto::run_clip(run, &clips)
        .ok_or("its clip wasn't found: add its folder in Settings, or check it's still there")?
        .path;
    // Overlays stay on this machine: for an original on the NAS, beside where
    // its local copy goes.
    let local = crate::library_page::roots().local_copy(&clip);
    let out = overlays::overlay_path(
        local.as_deref().unwrap_or(&clip),
        settings.overlay_folder.as_deref(),
    );
    Ok(Job {
        run_id: run_id.to_owned(),
        payload,
        clip,
        out,
        fingerprint: run.fingerprint.clone().unwrap_or_default(),
    })
}

/// Adds runs to the queue (once each), with their course names.
fn queue(p: &Page, ids: &[String]) {
    let db = p
        .shared
        .settings()
        .run_database
        .clone()
        .and_then(|path| TimerDatabase::load(&path).ok());
    let mut s = p.state.borrow_mut();
    for id in ids {
        if let Some(item) = s.items.iter_mut().find(|i| &i.run_id == id) {
            if !matches!(item.status, Status::Rendering(_)) {
                item.status = Status::Queued;
            }
            continue;
        }
        let course = db
            .as_ref()
            .and_then(|db| {
                let run = db.runs.iter().find(|r| &r.id == id)?;
                db.course_by_id(&run.course_id).ok().map(|c| c.name.clone())
            })
            .unwrap_or_default();
        s.items.push(Item {
            run_id: id.clone(),
            course,
            clip: None,
            out: None,
            status: Status::Queued,
        });
    }
}

/// Queues every counted run without an up-to-date overlay.
fn add_unrendered(p: &Page) {
    let Some(path) = p.shared.settings().run_database.clone() else {
        p.status
            .set_text("Choose your run database first (the gear in the sidebar).");
        return;
    };
    let db = match TimerDatabase::load(&path) {
        Ok(db) => db,
        Err(e) => {
            p.status
                .set_text(&format!("The run database couldn't be read: {e}"));
            return;
        }
    };
    let renders = Library::open_default()
        .and_then(|l| l.renders())
        .unwrap_or_default();
    let ids: Vec<String> = db
        .runs
        .iter()
        .filter(|r| r.committed && !r.ignored)
        .filter(|r| !renders.get(&r.id).is_some_and(|done| is_current(done, r)))
        .map(|r| r.id.clone())
        .collect();
    let n = ids.len();
    queue(p, &ids);
    p.status.set_text(&match n {
        0 => "Every counted run's overlay is rendered and up to date.".to_owned(),
        1 => "Added 1 run. Press Render to start.".to_owned(),
        n => format!("Added {n} runs. Press Render to start."),
    });
    show(p);
}

fn summary(items: &[Item]) -> String {
    let count = |f: fn(&Status) -> bool| items.iter().filter(|i| f(&i.status)).count();
    let done = count(|s| matches!(s, Status::Done(_)));
    let failed = count(|s| matches!(s, Status::Failed(_)));
    let queued = count(|s| matches!(s, Status::Queued | Status::Stopped));
    let mut parts = Vec::new();
    if done > 0 {
        parts.push(format!("{done} rendered"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if queued > 0 {
        parts.push(format!("{queued} waiting"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}.", parts.join(", "))
    }
}

fn show_folder(p: &Page) {
    p.folder
        .set_text(&match &p.shared.settings().overlay_folder {
            Some(f) => format!("Overlays go to {}", f.display()),
            None => "Overlays go in an Overlays folder beside each clip".to_owned(),
        });
}

/// Rebuilds the list from the state.
/// The sidebar's summary: what's left to render, else failures, else done.
fn activity_text(items: &[Item]) -> String {
    let count = |f: fn(&Status) -> bool| items.iter().filter(|i| f(&i.status)).count();
    let left = count(|s| matches!(s, Status::Queued | Status::Rendering(_)));
    let failed = count(|s| matches!(s, Status::Failed(_)));
    let done = count(|s| matches!(s, Status::Done(_)));
    match (left, failed, done) {
        (0, 0, 0) => String::new(),
        (0, 0, d) => format!("{d} done"),
        (0, f, _) => format!("{f} failed"),
        (l, _, _) => format!("{l} to render"),
    }
}

fn show(p: &Page) {
    let s = p.state.borrow();
    p.activity.set_text(&activity_text(&s.items));
    p.views
        .set_visible_child_name(if s.items.is_empty() { "empty" } else { "list" });
    while let Some(row) = p.list.first_child() {
        p.list.remove(&row);
    }
    let mut bars = p.bars.borrow_mut();
    bars.clear();
    for item in &s.items {
        let row = adw::ActionRow::new();
        row.set_title(&if item.course.is_empty() {
            item.run_id.clone()
        } else {
            format!("{} · {}", item.course, item.run_id)
        });
        let clip = item
            .clip
            .as_ref()
            .and_then(|c| c.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        let (text, class) = match &item.status {
            Status::Queued => ("Waiting".to_owned(), ""),
            Status::Rendering(_) => ("Rendering…".to_owned(), ""),
            Status::Done(d) => (d.clone(), "success"),
            Status::Failed(e) => (format!("Failed: {e}"), "error"),
            Status::Stopped => ("Stopped".to_owned(), "warning"),
        };
        row.set_subtitle(&match clip {
            Some(c) => format!("{c} · {text}"),
            None => text,
        });
        row.set_subtitle_lines(3);
        if !class.is_empty() {
            row.add_css_class(class);
        }
        if let Status::Rendering(f) = item.status {
            let bar = gtk::ProgressBar::new();
            bar.set_fraction(f);
            bar.set_valign(gtk::Align::Center);
            bar.set_width_request(160);
            row.add_suffix(&bar);
            bars.insert(item.run_id.clone(), bar);
        }
        if let (Status::Done(_), Some(out)) = (&item.status, &item.out) {
            let open = gtk::Button::from_icon_name("folder-open-symbolic");
            open.set_tooltip_text(Some("Show the overlay in its folder"));
            open.add_css_class("flat");
            open.set_valign(gtk::Align::Center);
            let (out, window) = (out.clone(), p.shared.window.clone());
            open.connect_clicked(move |_| {
                gtk::FileLauncher::new(Some(&gio::File::for_path(&out))).open_containing_folder(
                    Some(&window),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            });
            row.add_suffix(&open);
        }
        if !matches!(item.status, Status::Rendering(_)) {
            let remove = gtk::Button::from_icon_name("window-close-symbolic");
            remove.set_tooltip_text(Some("Take it off the list"));
            remove.add_css_class("flat");
            remove.set_valign(gtk::Align::Center);
            let (p2, id) = (p.clone(), item.run_id.clone());
            remove.connect_clicked(move |_| {
                p2.state.borrow_mut().items.retain(|i| i.run_id != id);
                show(&p2);
            });
            row.add_suffix(&remove);
        }
        p.list.append(&row);
    }
    let rendering = s
        .items
        .iter()
        .any(|i| matches!(i.status, Status::Rendering(_)));
    let waiting = s
        .items
        .iter()
        .any(|i| matches!(i.status, Status::Queued | Status::Stopped));
    let any_done = s.items.iter().any(|i| matches!(i.status, Status::Done(_)));
    p.add_to_resolve.set_sensitive(any_done);
    p.new_project.set_sensitive(any_done);
    p.start.set_visible(!rendering);
    p.start.set_sensitive(waiting);
    p.stop.set_visible(rendering);
    p.clear.set_sensitive(
        s.items
            .iter()
            .any(|i| !matches!(i.status, Status::Queued | Status::Rendering(_))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_overlays_that_are_up_to_date_and_renders_the_rest() {
        use timer_core::models::RunRecord;
        let dir = tempfile::tempdir().unwrap();
        let run = |id: &str, fp: &str| {
            let mut r = RunRecord::new(id, "c", "2026-11-24", "a.mp4", 30.0, Default::default());
            r.fingerprint = Some(fp.into());
            r
        };
        let db = TimerDatabase {
            courses: vec![],
            runs: vec![
                run("fresh", "f1"),
                run("moved", "m2"),
                run("gone", "g1"),
                run("new", "n1"),
            ],
        };
        let done = |id: &str, fp: &str, exists: bool| {
            let overlay = dir.path().join(format!("{id}.overlay.mov"));
            if exists {
                std::fs::write(&overlay, b"x").unwrap();
            }
            Rendered {
                run_id: id.into(),
                clip: dir.path().join("a.mp4"),
                overlay,
                first_frame: 0,
                timecode: "00:00:00:00".into(),
                fingerprint: fp.into(),
                rendered_ms: 0,
            }
        };
        let renders: HashMap<String, Rendered> = [
            done("fresh", "f1", true),
            // Its markers changed since it was rendered.
            done("moved", "m1", true),
            // The file was deleted.
            done("gone", "g1", false),
        ]
        .into_iter()
        .map(|r| (r.run_id.clone(), r))
        .collect();
        let ids: Vec<String> = ["new", "fresh", "moved", "gone", "not-in-db"]
            .map(String::from)
            .into();
        let (ready, stale) = split_rendered(&ids, &db, &renders);
        assert_eq!(
            ready.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            ["fresh"]
        );
        assert_eq!(stale, ["new", "moved", "gone", "not-in-db"]);
    }

    #[test]
    fn says_what_the_new_project_has() {
        let reply = json!({"ok": true, "project": "Ghost Run 2026-11-24 2", "timeline": "Ghost Run 2026-11-24 2",
            "placed": 2, "notes": ["c.MP4: the clip is missing"]});
        assert_eq!(
            describe_project(&reply),
            "Made project Ghost Run 2026-11-24 2 with 2 runs on timeline Ghost Run 2026-11-24 2. c.MP4: the clip is missing."
        );
    }

    #[test]
    fn says_what_adding_to_resolve_did() {
        let reply = json!({"ok": true, "project": "Race day", "timeline": "Edit", "results": [
            {"clip": "/v/a.mp4", "placed": 1, "already": 0, "note": null},
            {"clip": "/v/b.mp4", "placed": 0, "already": 1, "note": null},
            {"clip": "/v/c.mp4", "placed": 0, "already": 0,
             "note": "its clip is missing"},
            {"clip": "/v/d.mp4", "placed": 1, "already": 0, "clip_added": true,
             "proxy_note": "Resolve didn't take its proxy", "note": null},
        ]});
        assert_eq!(
            describe_added(&reply),
            "Added 2 overlays to Edit in Race day (1 already there). 1 clip wasn't on the timeline, so it was added at its end. d.mp4: Resolve didn't take its proxy. Not added: c.mp4: its clip is missing."
        );
    }
}
