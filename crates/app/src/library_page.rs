//! The Library page: every clip in your footage folders, when it was recorded,
//! whether it has a proxy, and which stored run (if any) it is.
//!
//! Scans and proxy encodes run on a worker thread (`worker`); the page sends it
//! [`Command`]s and polls its [`Reply`]s, like the Import page does.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use library::scan::{self, Progress};
use library::settings::AppSettings;
use library::{Cancel, Clip, Library, MediaInfo, ProxyState, proxy};

enum Command {
    Scan(Vec<PathBuf>),
    Proxies(Vec<(PathBuf, MediaInfo)>),
}

enum Reply {
    Clips(Vec<Clip>),
    Status(String, Option<f64>),
    /// Which clip's proxy is being made now (`None`: the queue is done).
    Making(Option<PathBuf>),
    Done(String),
    Error(String),
}

struct State {
    settings: AppSettings,
    clips: Vec<Clip>,
    /// Lower-case clip file name → "course · run id".
    runs: HashMap<String, String>,
    runs_error: Option<String>,
    selected: HashSet<PathBuf>,
    making: Option<PathBuf>,
    queued: HashSet<PathBuf>,
    busy: bool,
    only_missing: bool,
}

#[derive(Clone)]
struct Page {
    window: adw::ApplicationWindow,
    state: Rc<RefCell<State>>,
    store: gio::ListStore,
    tx: mpsc::Sender<Command>,
    cancel: Cancel,
    banner: gtk::Box,
    banner_text: gtk::Label,
    counts: gtk::Label,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    rescan: gtk::Button,
    proxies: gtk::Button,
    stop: gtk::Button,
}

/// Builds the page; the returned widget goes in the app's stack.
pub fn page(window: &adw::ApplicationWindow) -> gtk::Widget {
    let (settings, settings_error) = match AppSettings::load() {
        Ok(s) => (s, None),
        Err(e) => (
            AppSettings::default(),
            Some(format!("Settings could not be read: {e:#}")),
        ),
    };
    let (tx, commands) = mpsc::channel();
    let (replies, rx) = mpsc::channel();
    let cancel = Cancel::default();
    worker(commands, replies, cancel.clone());

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Library", "")));
    let folders = button("Folders…");
    folders.set_tooltip_text(Some("Footage folders, run database and Resolve template"));
    header.pack_end(&folders);
    root.append(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("content");
    content.set_vexpand(true);

    let banner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    banner.add_css_class("banner");
    let banner_text = label("", "");
    banner_text.set_wrap(true);
    banner_text.set_hexpand(true);
    let add_first = button("Add folder…");
    banner.append(&banner_text);
    banner.append(&add_first);
    content.append(&banner);

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let counts = label("", "heading");
    counts.set_hexpand(true);
    let filter = gtk::DropDown::from_strings(&["All clips", "Without a proxy"]);
    filter.update_property(&[gtk::accessible::Property::Label("Which clips to show")]);
    let rescan = button("Rescan");
    let proxies = button("Make proxies");
    proxies.add_css_class("suggested-action");
    toolbar.append(&counts);
    toolbar.append(&filter);
    toolbar.append(&rescan);
    toolbar.append(&proxies);
    content.append(&toolbar);

    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let footer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    footer.add_css_class("footer");
    let footer_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let status = label("", "");
    status.set_hexpand(true);
    status.set_wrap(true);
    let stop = button("Stop");
    stop.set_visible(false);
    footer_row.append(&status);
    footer_row.append(&stop);
    let progress = gtk::ProgressBar::new();
    progress.set_visible(false);
    footer.append(&footer_row);
    footer.append(&progress);

    let page = Page {
        window: window.clone(),
        state: Rc::new(RefCell::new(State {
            settings,
            clips: Vec::new(),
            runs: HashMap::new(),
            runs_error: None,
            selected: HashSet::new(),
            making: None,
            queued: HashSet::new(),
            busy: false,
            only_missing: false,
        })),
        store: store.clone(),
        tx,
        cancel,
        banner,
        banner_text,
        counts,
        status,
        progress,
        rescan: rescan.clone(),
        proxies: proxies.clone(),
        stop: stop.clone(),
    };

    let table = table(&page);
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&table)
        .min_content_height(160)
        .build();
    content.append(&scroller);
    root.append(&content);
    root.append(&footer);

    let p = page.clone();
    rescan.connect_clicked(move |_| start_scan(&p));
    let p = page.clone();
    proxies.connect_clicked(move |_| confirm_proxies(&p));
    let p = page.clone();
    stop.connect_clicked(move |_| {
        p.cancel.store(true, Ordering::Relaxed);
        p.status.set_text("Stopping…");
    });
    let p = page.clone();
    filter.connect_selected_notify(move |f| {
        p.state.borrow_mut().only_missing = f.selected() == 1;
        refill(&p);
    });
    let p = page.clone();
    folders.connect_clicked(move |_| settings_dialog(&p));
    let p = page.clone();
    add_first.connect_clicked(move |_| add_folder(&p, None));

    let p = page.clone();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        for reply in rx.try_iter() {
            handle(&p, reply);
        }
        glib::ControlFlow::Continue
    });

    load_runs(&page);
    if let Some(e) = settings_error {
        page.status.set_text(&e);
    }
    // The library from last time shows at once; a scan then brings it up to date.
    match Library::open_default().and_then(|l| l.clips()) {
        Ok(clips) => page.state.borrow_mut().clips = clips,
        Err(e) => page
            .status
            .set_text(&format!("Library could not be opened: {e:#}")),
    }
    refill(&page);
    if !page.state.borrow().settings.footage_folders.is_empty() {
        start_scan(&page);
    }
    root.upcast()
}

fn worker(commands: mpsc::Receiver<Command>, replies: mpsc::Sender<Reply>, cancel: Cancel) {
    std::thread::spawn(move || {
        let library = match Library::open_default() {
            Ok(l) => l,
            Err(e) => {
                let _ = replies.send(Reply::Error(format!("Library could not be opened: {e:#}")));
                return;
            }
        };
        let send_clips = |library: &Library| {
            if let Ok(clips) = library.clips() {
                let _ = replies.send(Reply::Clips(clips));
            }
        };
        for command in commands {
            cancel.store(false, Ordering::Relaxed);
            match command {
                Command::Scan(folders) => {
                    let result = scan::scan(&folders, &library, &cancel, |p| {
                        let text = match p {
                            Progress::Looking(_) => "Looking for footage…".to_owned(),
                            Progress::Probing { done, total, path } => format!(
                                "Reading {} ({} of {total})",
                                path.file_name().unwrap_or_default().to_string_lossy(),
                                done + 1
                            ),
                        };
                        let _ = replies.send(Reply::Status(text, None));
                    });
                    send_clips(&library);
                    let _ = replies.send(match result {
                        Ok(r) if cancel.load(Ordering::Relaxed) => {
                            Reply::Done(format!("Scan stopped after {} clips.", r.probed))
                        }
                        Ok(r) => Reply::Done(scan_summary(&r)),
                        Err(e) => Reply::Error(format!("Scan failed: {e:#}")),
                    });
                }
                Command::Proxies(queue) => {
                    if let Err(e) = proxy::check_ffmpeg() {
                        let _ = replies.send(Reply::Error(format!("{e:#}")));
                        continue;
                    }
                    let (mut made, mut failed) = (0, 0);
                    let total = queue.len();
                    for (i, (path, info)) in queue.into_iter().enumerate() {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let _ = replies.send(Reply::Making(Some(path.clone())));
                        let name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let result = proxy::make(&path, &info, &cancel, |f| {
                            let _ = replies.send(Reply::Status(
                                format!("Making proxy {} of {total}: {name}", i + 1),
                                Some(f),
                            ));
                        });
                        let state = match result {
                            Ok(p) => {
                                made += 1;
                                ProxyState::Ready(p)
                            }
                            Err(_) if cancel.load(Ordering::Relaxed) => break,
                            Err(e) => {
                                failed += 1;
                                ProxyState::Failed(format!("{e:#}"))
                            }
                        };
                        let _ = library.set_proxy(&path, state);
                        send_clips(&library);
                    }
                    let _ = replies.send(Reply::Making(None));
                    let mut text = format!("Made {made} {}.", plural(made, "proxy", "proxies"));
                    if failed > 0 {
                        text.push_str(&format!(" {failed} failed: see the Proxy column."));
                    }
                    if cancel.load(Ordering::Relaxed) {
                        text.push_str(" Stopped.");
                    }
                    let _ = replies.send(Reply::Done(text));
                }
            }
        }
    });
}

fn scan_summary(r: &scan::ScanReport) -> String {
    let mut text = format!("{} {} found", r.found, plural(r.found, "clip", "clips"));
    if r.probed > 0 {
        text.push_str(&format!(", {} new or changed", r.probed));
    }
    if r.removed > 0 {
        text.push_str(&format!(", {} gone", r.removed));
    }
    if r.unreadable > 0 {
        text.push_str(&format!(", {} unreadable", r.unreadable));
    }
    text + "."
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

fn handle(p: &Page, reply: Reply) {
    match reply {
        Reply::Clips(clips) => {
            let mut s = p.state.borrow_mut();
            let present: HashSet<&PathBuf> = clips.iter().map(|c| &c.path).collect();
            s.selected.retain(|path| present.contains(path));
            s.clips = clips;
            drop(s);
            refill(p);
        }
        Reply::Status(text, fraction) => {
            p.status.set_text(&text);
            p.progress.set_visible(true);
            match fraction {
                Some(f) => p.progress.set_fraction(f),
                None => p.progress.pulse(),
            }
        }
        Reply::Making(path) => {
            let mut s = p.state.borrow_mut();
            if let Some(path) = &path {
                s.queued.remove(path);
            } else {
                s.queued.clear();
            }
            s.making = path;
            drop(s);
            refill(p);
        }
        Reply::Done(text) | Reply::Error(text) => {
            p.state.borrow_mut().busy = false;
            p.status.set_text(&text);
            p.progress.set_visible(false);
            refresh_controls(p);
        }
    }
}

fn start_scan(p: &Page) {
    let folders = p.state.borrow().settings.footage_folders.clone();
    if folders.is_empty() || p.state.borrow().busy {
        refresh_controls(p);
        return;
    }
    p.state.borrow_mut().busy = true;
    refresh_controls(p);
    let _ = p.tx.send(Command::Scan(folders));
}

/// The clips "Make proxies" would work on: the ticked ones, or else every
/// readable clip without a good proxy.
fn proxy_queue(s: &State) -> Vec<(PathBuf, MediaInfo)> {
    s.clips
        .iter()
        .filter(|c| s.selected.is_empty() || s.selected.contains(&c.path))
        .filter(|c| !matches!(c.proxy, ProxyState::Ready(_)))
        .filter_map(|c| Some((c.path.clone(), c.info.clone()?)))
        .collect()
}

fn confirm_proxies(p: &Page) {
    let queue = proxy_queue(&p.state.borrow());
    if queue.is_empty() {
        p.status.set_text("Every clip here already has a proxy.");
        return;
    }
    let seconds: f64 = queue.iter().filter_map(|(_, i)| i.duration_seconds).sum();
    // ProRes 422 Proxy runs about 45 Mb/s at 1920x1080, 29.97 fps.
    let bytes: f64 = queue
        .iter()
        .map(|(_, i)| {
            let (w, h) = proxy::proxy_size(i);
            let rate = 45e6 / 8.0 * f64::from(w * h) / (1920.0 * 1080.0) * i.fps() / 29.97;
            rate * i.duration_seconds.unwrap_or(0.0)
        })
        .sum();
    let body = format!(
        "{} {}, {} of footage. They take about {:.1} GB, in a Proxy folder beside each clip.\n\n\
         Resolve finds them there as proxy media, and the frame viewer and auto-markers use them.",
        queue.len(),
        plural(queue.len(), "clip", "clips"),
        duration(seconds),
        bytes / 1e9
    );
    let dialog = adw::AlertDialog::new(Some("Make proxies?"), Some(&body));
    dialog.add_responses(&[("cancel", "Cancel"), ("make", "Make proxies")]);
    dialog.set_response_appearance("make", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("make"));
    dialog.set_close_response("cancel");
    let p2 = p.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "make" || p2.state.borrow().busy {
            return;
        }
        let mut s = p2.state.borrow_mut();
        s.busy = true;
        s.queued = queue.iter().map(|(path, _)| path.clone()).collect();
        drop(s);
        refill(&p2);
        let _ = p2.tx.send(Command::Proxies(queue.clone()));
    });
    dialog.present(Some(&p.window));
}

fn refresh_controls(p: &Page) {
    let s = p.state.borrow();
    let no_folders = s.settings.footage_folders.is_empty();
    p.banner.set_visible(no_folders || s.runs_error.is_some());
    p.banner_text.set_text(if no_folders {
        "Add the folders your footage is in. Ghost Run lists every clip in them and keeps the list up to date."
    } else {
        s.runs_error.as_deref().unwrap_or("")
    });
    p.rescan.set_sensitive(!s.busy && !no_folders);
    p.proxies.set_sensitive(!s.busy && !s.clips.is_empty());
    p.proxies.set_label(if s.selected.is_empty() {
        "Make missing proxies"
    } else {
        "Make proxies for ticked"
    });
    p.stop.set_visible(s.busy);
    let with_proxy = s
        .clips
        .iter()
        .filter(|c| matches!(c.proxy, ProxyState::Ready(_)))
        .count();
    p.counts.set_text(&format!(
        "{} {} · {} with proxies",
        s.clips.len(),
        plural(s.clips.len(), "clip", "clips"),
        with_proxy
    ));
}

/// Rebuilds the table from the state.
fn refill(p: &Page) {
    let s = p.state.borrow();
    let rows: Vec<glib::BoxedAnyObject> = s
        .clips
        .iter()
        .filter(|c| !s.only_missing || !matches!(c.proxy, ProxyState::Ready(_)))
        .map(|c| glib::BoxedAnyObject::new(c.clone()))
        .collect();
    drop(s);
    p.store.remove_all();
    p.store.extend_from_slice(&rows);
    refresh_controls(p);
}

const COLUMNS: &[(&str, i32)] = &[
    ("Clip", 240),
    ("Recorded", 175),
    ("Length", 70),
    ("Video", 190),
    ("Proxy", 90),
    ("Run", 200),
    ("Folder", 200),
];

fn table(p: &Page) -> gtk::ColumnView {
    let selection = gtk::NoSelection::new(Some(p.store.clone()));
    let view = gtk::ColumnView::new(Some(selection));
    view.add_css_class("footage-table");
    view.set_show_row_separators(true);
    for (index, (title, width)) in COLUMNS.iter().enumerate() {
        let factory = gtk::SignalListItemFactory::new();
        let page = p.clone();
        factory.connect_setup(move |_, item| {
            let li = item.downcast_ref::<gtk::ListItem>().unwrap();
            let text = label("", if index == 4 { "status" } else { "" });
            // Names and folders can be shortened; the rest is short and must show whole.
            if matches!(index, 0 | 5 | 6) {
                text.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            }
            if index == 0 {
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
                    let path = object.borrow::<Clip>().path.clone();
                    let mut s = page.state.borrow_mut();
                    if check.is_active() {
                        s.selected.insert(path);
                    } else {
                        s.selected.remove(&path);
                    }
                    drop(s);
                    refresh_controls(&page);
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
            let clip = object.borrow::<Clip>();
            let s = page.state.borrow();
            let (text, tip) = cell(&s, &clip, index);
            let label = if index == 0 {
                let row = li.child().and_downcast::<gtk::Box>().unwrap();
                let check = row
                    .first_child()
                    .and_downcast::<gtk::CheckButton>()
                    .unwrap();
                check.set_active(s.selected.contains(&clip.path));
                check.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Select {}",
                    clip.name()
                ))]);
                check.next_sibling().and_downcast::<gtk::Label>().unwrap()
            } else {
                li.child().and_downcast::<gtk::Label>().unwrap()
            };
            label.set_text(&text);
            label.set_tooltip_text(tip.as_deref());
            if index == 4 {
                for class in ["success", "warning", "error"] {
                    label.remove_css_class(class);
                }
                match clip.proxy {
                    ProxyState::Ready(_) => label.add_css_class("success"),
                    ProxyState::Failed(_) => label.add_css_class("error"),
                    ProxyState::Missing => {}
                }
            }
        });
        let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
        column.set_fixed_width(*width);
        column.set_resizable(true);
        column.set_expand(index == 0);
        view.append_column(&column);
    }
    view
}

/// A cell's text and tooltip.
fn cell(s: &State, clip: &Clip, column: usize) -> (String, Option<String>) {
    let info = clip.info.as_ref();
    match column {
        0 => (clip.name(), Some(clip.path.display().to_string())),
        1 => {
            let source = match clip.recorded_source.as_str() {
                "filename" => "From the file name (the camera's clock)",
                "metadata" => "From the clip's metadata (the camera's clock)",
                _ => "From the file's modification time: the camera didn't say",
            };
            let mark = if clip.recorded_source == "modified" {
                " *"
            } else {
                ""
            };
            (
                clip.recorded.clone().unwrap_or_default() + mark,
                Some(source.into()),
            )
        }
        2 => (
            info.and_then(|i| i.duration_seconds)
                .map(duration)
                .unwrap_or_default(),
            None,
        ),
        3 => match info {
            Some(i) => (
                format!("{}×{} · {:.2} fps", i.width, i.height, i.fps()),
                Some(format!(
                    "{} · {} frames{}{}",
                    i.codec,
                    i.frames.map_or("?".into(), |f| f.to_string()),
                    i.timecode
                        .as_ref()
                        .map_or(String::new(), |t| format!(" · timecode {t}")),
                    if i.rotation != 0 {
                        format!(" · rotated {}°", i.rotation)
                    } else {
                        String::new()
                    }
                )),
            ),
            None => ("Can't read".into(), clip.probe_error.clone()),
        },
        4 => {
            if s.making.as_ref() == Some(&clip.path) {
                return ("Making…".into(), None);
            }
            if s.queued.contains(&clip.path) {
                return ("Queued".into(), None);
            }
            match &clip.proxy {
                ProxyState::Ready(p) => ("Ready".into(), Some(p.display().to_string())),
                ProxyState::Failed(e) => ("Failed".into(), Some(e.clone())),
                ProxyState::Missing if info.is_none() => (String::new(), None),
                ProxyState::Missing => ("None".into(), None),
            }
        }
        5 => match s.runs.get(&run_key(&clip.path)) {
            Some(run) => (run.clone(), None),
            None => (String::new(), None),
        },
        _ => {
            let folder = clip.path.parent().unwrap_or(Path::new(""));
            (
                folder.display().to_string(),
                Some(folder.display().to_string()),
            )
        }
    }
}

/// The name a stored run would know the clip by: an editing copy
/// (`clip.MP4.mov`) is the original (`clip.MP4`).
fn run_key(path: &Path) -> String {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    match name.strip_suffix(".mov") {
        Some(original) if original.contains('.') => original.to_owned(),
        _ => name,
    }
}

fn load_runs(p: &Page) {
    let path = p.state.borrow().settings.run_database.clone();
    let mut s = p.state.borrow_mut();
    s.runs.clear();
    s.runs_error = None;
    let Some(path) = path else { return };
    match timer_core::TimerDatabase::load(&path) {
        Ok(db) => {
            for run in &db.runs {
                let course = db
                    .course_by_id(&run.course_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|_| run.course_id.clone());
                let key = run_key(Path::new(&run.filename));
                s.runs.insert(key, format!("{course} · {}", run.id));
            }
        }
        Err(e) => s.runs_error = Some(format!("The run database couldn't be read: {e}")),
    }
}

fn duration(seconds: f64) -> String {
    let s = seconds.round() as i64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn save_settings(p: &Page) {
    if let Err(e) = p.state.borrow().settings.save() {
        p.status
            .set_text(&format!("Settings could not be saved: {e:#}"));
    }
}

fn add_folder(p: &Page, then: Option<Refill>) {
    let dialog = gtk::FileDialog::builder()
        .title("Add a footage folder")
        .build();
    let p = p.clone();
    dialog.select_folder(Some(&p.window.clone()), gio::Cancellable::NONE, move |r| {
        let Ok(folder) = r else { return };
        let Some(path) = folder.path() else {
            p.status.set_text(
                "Choose a folder on this computer (not a network location in the file browser).",
            );
            return;
        };
        {
            let mut s = p.state.borrow_mut();
            if !s.settings.footage_folders.contains(&path) {
                s.settings.footage_folders.push(path);
            }
        }
        save_settings(&p);
        if let Some(then) = &then {
            then();
        }
        start_scan(&p);
    });
}

/// Rebuilds the folder rows in the settings dialog.
type Refill = Rc<dyn Fn()>;

fn settings_dialog(p: &Page) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Library settings");
    let page = adw::PreferencesPage::new();
    dialog.add(&page);

    let folders = adw::PreferencesGroup::new();
    folders.set_title("Footage folders");
    folders.set_description(Some(
        "Every video in these folders and their subfolders is listed. Proxy, cache and backup folders are skipped.",
    ));
    let add = button("Add folder…");
    add.set_valign(gtk::Align::Center);
    folders.set_header_suffix(Some(&add));
    page.add(&folders);
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let fill: Rc<RefCell<Option<Refill>>> = Rc::default();
    {
        let (p, folders, rows, fill_ref) = (p.clone(), folders.clone(), rows.clone(), fill.clone());
        let fill_fn: Refill = Rc::new(move || {
            for row in rows.borrow_mut().drain(..) {
                folders.remove(&row);
            }
            let list = p.state.borrow().settings.footage_folders.clone();
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
                let (p, fill_ref) = (p.clone(), fill_ref.clone());
                remove.connect_clicked(move |_| {
                    p.state
                        .borrow_mut()
                        .settings
                        .footage_folders
                        .retain(|f| f != &path);
                    save_settings(&p);
                    if let Some(f) = fill_ref.borrow().as_ref() {
                        f();
                    }
                    start_scan(&p);
                });
                folders.add(&row);
                rows.borrow_mut().push(row);
            }
        });
        *fill.borrow_mut() = Some(fill_fn.clone());
        fill_fn();
    }
    let (p2, fill2) = (p.clone(), fill.clone());
    add.connect_clicked(move |_| add_folder(&p2, fill2.borrow().clone()));

    let files = adw::PreferencesGroup::new();
    files.set_title("Files");
    page.add(&files);
    let database = file_row(
        p,
        "Run database",
        "timer_db.yaml: your courses and stored runs. The Run column comes from it.",
        |s| s.run_database.clone(),
        |s, v| s.run_database = v,
        "YAML",
        &["*.yaml", "*.yml"],
    );
    files.add(&database);
    let template = file_row(
        p,
        "Resolve project template",
        "A .drp exported from Resolve. Not used yet: new Resolve projects will start from it.",
        |s| s.resolve_template.clone(),
        |s, v| s.resolve_template = v,
        "Resolve project",
        &["*.drp"],
    );
    files.add(&template);
    let p2 = p.clone();
    dialog.connect_closed(move |_| {
        load_runs(&p2);
        refill(&p2);
    });
    dialog.present(Some(&p.window));
}

/// A settings row holding one file path, with Choose and Clear buttons.
fn file_row(
    p: &Page,
    title: &str,
    help: &str,
    get: fn(&AppSettings) -> Option<PathBuf>,
    set: fn(&mut AppSettings, Option<PathBuf>),
    filter_name: &str,
    patterns: &[&str],
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let show = {
        let (row, p, help) = (row.clone(), p.clone(), help.to_owned());
        move || {
            let current = get(&p.state.borrow().settings);
            row.set_subtitle(&match current {
                Some(path) => format!("{}\n{help}", path.display()),
                None => format!("Not set\n{help}"),
            });
        }
    };
    show();
    let choose = button("Choose…");
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
    let (p2, show2, title) = (p.clone(), show.clone(), title.to_owned());
    choose.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder()
            .title(&title)
            .filters(&filters)
            .build();
        let (p3, show3) = (p2.clone(), show2.clone());
        dialog.open(Some(&p2.window), gio::Cancellable::NONE, move |r| {
            if let Ok(file) = r
                && let Some(path) = file.path()
            {
                set(&mut p3.state.borrow_mut().settings, Some(path));
                save_settings(&p3);
                show3();
            }
        });
    });
    let p2 = p.clone();
    clear.connect_clicked(move |_| {
        set(&mut p2.state.borrow_mut().settings, None);
        save_settings(&p2);
        show();
    });
    row
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

    #[test]
    fn editing_copies_match_runs_by_the_original_name() {
        assert_eq!(run_key(Path::new("/v/GX010150.MP4.mov")), "gx010150.mp4");
        assert_eq!(run_key(Path::new("/v/GX010150.MP4")), "gx010150.mp4");
        assert_eq!(run_key(Path::new("/v/run.mov")), "run.mov");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(65.4), "1:05");
        assert_eq!(duration(3725.0), "1:02:05");
    }
}
