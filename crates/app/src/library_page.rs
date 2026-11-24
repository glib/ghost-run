//! The Library page: every clip in your footage folders, when it was recorded,
//! whether it has a proxy, and which stored run (if any) it is: its course,
//! lap and place. A Courses panel filters by course (or clips not timed yet)
//! and adds and renames courses. Timing a clip opens the Time page over the
//! Library, with a back arrow.
//!
//! Scans and proxy encodes run on a worker thread (`worker`); the page sends it
//! [`Command`]s and polls its [`Reply`]s, like the Import page does.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use library::auto::{self, Reference};
use library::scan::{self, Progress};
use library::{Cancel, Clip, Draft, Library, MediaInfo, ProxyState, proxy};
use timer_core::board::course_board;
use timer_core::timing::format_duration;

use crate::shared::{Change, Shared};

enum Command {
    Scan(Vec<PathBuf>),
    Proxies(Vec<(PathBuf, MediaInfo)>),
    /// Place markers in each clip (path, footage to read) from the best-matching course.
    AutoPlace(Vec<(PathBuf, PathBuf)>, Vec<Reference>),
}

enum Reply {
    Clips(Vec<Clip>),
    Status(String, Option<f64>),
    /// A clip's proxy is being made now.
    Making(PathBuf),
    /// A clip's proxy is finished (made or not).
    Made(PathBuf),
    Done(String),
    Error(String),
}

/// A stored run, as the Library shows it beside its clip.
#[derive(Debug, Clone, PartialEq)]
struct RunInfo {
    id: String,
    course_id: String,
    course: String,
    lap: Option<f64>,
    /// Its place on the course's board (counted runs only).
    rank: Option<usize>,
    ignored: bool,
}

/// Which clips the table shows.
#[derive(Debug, Clone, PartialEq)]
enum View {
    All,
    /// No stored run yet (drafts included).
    Untimed,
    Course(String),
}

struct State {
    /// The folders the last scan covered.
    scanned: Vec<PathBuf>,
    clips: Vec<Clip>,
    /// Lower-case clip file name → its run.
    runs: HashMap<String, RunInfo>,
    /// The database's courses: id, name.
    courses: Vec<(String, String)>,
    view: View,
    runs_error: Option<String>,
    /// Clips with markers placed but not committed.
    drafted: HashSet<PathBuf>,
    selected: HashSet<PathBuf>,
    making: HashSet<PathBuf>,
    queued: HashSet<PathBuf>,
    busy: bool,
    only_missing: bool,
}

#[derive(Clone)]
struct Page {
    shared: Shared,
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
    time: gtk::Button,
    auto_place: gtk::Button,
    move_course: gtk::Button,
    stop: gtk::Button,
    course_list: gtk::ListBox,
    rename: gtk::Button,
    /// Set while the course list is rebuilt, so its selection isn't taken as a click.
    filling: Rc<std::cell::Cell<bool>>,
}

/// Builds the page (with the Time page that opens over it); the returned
/// widget goes in the app's stack.
pub fn page(shared: &Shared) -> gtk::Widget {
    let nav = adw::NavigationView::new();
    let library = library_view(shared);
    nav.add(
        &adw::NavigationPage::builder()
            .child(&library)
            .title("Library")
            .tag("library")
            .build(),
    );
    let time = adw::NavigationPage::builder()
        .child(&crate::time_page::page(shared))
        .title("Time a run")
        .tag("time")
        .build();
    let nav2 = nav.clone();
    shared.subscribe(move |change| {
        if change == Change::TimeClip && nav2.visible_page().as_ref() != Some(&time) {
            nav2.push(&time);
        }
    });
    nav.upcast()
}

fn library_view(shared: &Shared) -> gtk::Widget {
    let window = &shared.window;
    let (tx, commands) = mpsc::channel();
    let (replies, rx) = mpsc::channel();
    let cancel = Cancel::default();
    worker(commands, replies, cancel.clone());

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Library", "")));
    let folders = button("Folders…");
    folders.set_tooltip_text(Some("Footage folders, run database and Resolve template"));
    let open_file = button("Open file…");
    open_file.set_tooltip_text(Some("Time a clip that isn't in your footage folders"));
    let rescan = button("Rescan");
    rescan.set_tooltip_text(Some("Look for new, changed and removed clips"));
    header.pack_end(&folders);
    header.pack_end(&open_file);
    header.pack_end(&rescan);
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
    counts.set_wrap(true);
    let filter = gtk::DropDown::from_strings(&["All clips", "Without a proxy"]);
    filter.update_property(&[gtk::accessible::Property::Label("Which clips to show")]);
    let proxies = button("Make proxies");
    proxies.add_css_class("suggested-action");
    let time = button("Time it");
    time.set_tooltip_text(Some(
        "Place the markers on the ticked clip (or double-click a clip)",
    ));
    toolbar.append(&counts);
    toolbar.append(&filter);
    let auto_place = button("Auto-place");
    auto_place.set_tooltip_text(Some(
        "Find each ticked clip's course and place its markers from that course's reference run, as drafts to check and commit",
    ));
    toolbar.append(&auto_place);
    let move_course = button("Course…");
    move_course.set_tooltip_text(Some("Move the ticked clips' runs to another course"));
    toolbar.append(&move_course);
    toolbar.append(&time);
    toolbar.append(&proxies);
    content.append(&toolbar);

    // The Courses panel: all clips, those not timed yet, and each course.
    let course_list = gtk::ListBox::new();
    course_list.add_css_class("navigation-sidebar");
    course_list.update_property(&[gtk::accessible::Property::Label("Show clips by course")]);
    let course_scroller = gtk::ScrolledWindow::builder()
        .child(&course_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let add_course = button("New…");
    add_course.set_tooltip_text(Some("Add a course"));
    let rename = button("Rename…");
    rename.set_tooltip_text(Some("Rename the selected course"));
    let course_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    course_buttons.set_homogeneous(true);
    course_buttons.append(&add_course);
    course_buttons.append(&rename);
    let course_panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
    course_panel.set_size_request(180, -1);
    course_panel.append(&label("Courses", "heading"));
    course_panel.append(&course_scroller);
    course_panel.append(&course_buttons);

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
        shared: shared.clone(),
        window: window.clone(),
        state: Rc::new(RefCell::new(State {
            scanned: Vec::new(),
            clips: Vec::new(),
            runs: HashMap::new(),
            courses: Vec::new(),
            view: View::All,
            runs_error: None,
            drafted: HashSet::new(),
            selected: HashSet::new(),
            making: HashSet::new(),
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
        time: time.clone(),
        auto_place: auto_place.clone(),
        move_course: move_course.clone(),
        stop: stop.clone(),
        course_list: course_list.clone(),
        rename: rename.clone(),
        filling: Rc::default(),
    };

    let table = table(&page);
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&table)
        .min_content_height(160)
        .build();
    content.append(&scroller);
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    body.set_vexpand(true);
    body.add_css_class("content");
    content.remove_css_class("content");
    content.set_hexpand(true);
    body.append(&course_panel);
    body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    body.append(&content);
    root.append(&body);
    root.append(&footer);

    let p = page.clone();
    rescan.connect_clicked(move |_| start_scan(&p));
    let p = page.clone();
    proxies.connect_clicked(move |_| confirm_proxies(&p));
    let p = page.clone();
    auto_place.connect_clicked(move |_| start_auto(&p));
    let p = page.clone();
    time.connect_clicked(move |_| {
        let one = p.state.borrow().selected.iter().next().cloned();
        if let Some(path) = one {
            p.shared.time_clip(path);
        }
    });
    let p = page.clone();
    open_file.connect_clicked(move |_| choose_file(&p));
    let p = page.clone();
    move_course.connect_clicked(move |_| move_ticked(&p));
    let p = page.clone();
    course_list.connect_row_selected(move |_, row| {
        if p.filling.get() {
            return;
        }
        let Some(row) = row else { return };
        let name = row.widget_name();
        let view = match name.as_str() {
            "untimed" => View::Untimed,
            n => match n.strip_prefix("course:") {
                Some(id) => View::Course(id.to_owned()),
                None => View::All,
            },
        };
        let mut s = p.state.borrow_mut();
        if s.view != view {
            s.view = view;
            s.selected.clear();
            drop(s);
            refill(&p);
        }
    });
    let p = page.clone();
    add_course.connect_clicked(move |_| {
        let p2 = p.clone();
        crate::courses::new_course(&p.shared, move |result| match result {
            Ok((id, text)) => {
                p2.state.borrow_mut().view = View::Course(id);
                fill_courses(&p2);
                refill(&p2);
                p2.status.set_text(&text);
            }
            Err(e) => p2.status.set_text(&e),
        });
    });
    let p = page.clone();
    rename.connect_clicked(move |_| {
        let s = p.state.borrow();
        let View::Course(id) = &s.view else { return };
        let Some((id, name)) = s.courses.iter().find(|(c, _)| c == id).cloned() else {
            return;
        };
        drop(s);
        let p2 = p.clone();
        crate::courses::rename_course(&p.shared, &id, &name, move |result| {
            p2.status.set_text(&result.unwrap_or_else(|e| e));
        });
    });
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
    folders.connect_clicked(move |_| crate::settings_dialog::open(&p.shared));
    let p = page.clone();
    add_first.connect_clicked(move |_| crate::settings_dialog::add_folder(&p.shared));
    let p = page.clone();
    shared.subscribe(move |change| {
        if matches!(change, Change::TimeClip | Change::Render | Change::Resolve) {
            return;
        }
        load_runs(&p);
        fill_courses(&p);
        refill(&p);
        let folders = p.shared.settings().footage_folders.clone();
        if change == Change::Settings && folders != p.state.borrow().scanned {
            start_scan(&p);
        }
    });

    let p = page.clone();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        for reply in rx.try_iter() {
            handle(&p, reply);
        }
        glib::ControlFlow::Continue
    });

    load_runs(&page);
    fill_courses(&page);
    if let Some(e) = &shared.load_error {
        page.status.set_text(e);
    }
    // The library from last time shows at once; a scan then brings it up to date.
    match Library::open_default().and_then(|l| l.clips()) {
        Ok(clips) => page.state.borrow_mut().clips = clips,
        Err(e) => page
            .status
            .set_text(&format!("Library could not be opened: {e:#}")),
    }
    refill(&page);
    if !shared.settings().footage_folders.is_empty() {
        start_scan(&page);
    }
    root.upcast()
}

/// Makes the queued proxies, several at once ([`proxy::plan`]), recording
/// each in the library as it finishes. Returns how many were made and failed.
fn make_proxies(
    library: &Library,
    queue: Vec<(PathBuf, MediaInfo)>,
    cancel: &Cancel,
    replies: &mpsc::Sender<Reply>,
    send_clips: &dyn Fn(&Library),
) -> (usize, usize) {
    use std::sync::Mutex;
    let total = queue.len();
    let (w, h) = queue
        .iter()
        .map(|(_, i)| (i.width, i.height))
        .max_by_key(|(w, h)| u64::from(*w) * u64::from(*h))
        .unwrap_or((3840, 2160));
    let plan = proxy::plan(w, h, total);
    let queue = Mutex::new(queue.into_iter().enumerate().collect::<VecDeque<_>>());
    // Each clip's progress, 0–1, for the overall bar.
    let progress = Mutex::new(vec![0.0f64; total]);
    let finished = Mutex::new(0usize);
    let (results, done) = mpsc::channel::<(PathBuf, ProxyState)>();
    let report = |finished: usize| {
        let fraction = progress.lock().unwrap().iter().sum::<f64>() / total.max(1) as f64;
        let at_once = if plan.jobs > 1 {
            format!(", {} at a time", plan.jobs)
        } else {
            String::new()
        };
        let _ = replies.send(Reply::Status(
            format!("Making proxies: {finished} of {total} done{at_once}"),
            Some(fraction),
        ));
    };
    let (mut made, mut failed) = (0, 0);
    std::thread::scope(|scope| {
        for _ in 0..plan.jobs {
            let results = results.clone();
            let (queue, progress, finished, report) = (&queue, &progress, &finished, &report);
            scope.spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some((i, (path, info))) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let _ = replies.send(Reply::Making(path.clone()));
                    let result = proxy::make_with(&path, &info, plan.threads, cancel, |f| {
                        progress.lock().unwrap()[i] = f;
                        report(*finished.lock().unwrap());
                    });
                    let state = match result {
                        Ok(p) => ProxyState::Ready(p),
                        Err(_) if cancel.load(Ordering::Relaxed) => {
                            let _ = replies.send(Reply::Made(path));
                            break;
                        }
                        Err(e) => ProxyState::Failed(format!("{e:#}")),
                    };
                    progress.lock().unwrap()[i] = 1.0;
                    *finished.lock().unwrap() += 1;
                    report(*finished.lock().unwrap());
                    let _ = results.send((path, state));
                }
            });
        }
        drop(results);
        // The library is this thread's: record results here as they come.
        for (path, state) in done {
            match state {
                ProxyState::Ready(_) => made += 1,
                _ => failed += 1,
            }
            let _ = library.set_proxy(&path, state);
            let _ = replies.send(Reply::Made(path));
            send_clips(library);
        }
    });
    (made, failed)
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
                    let (made, failed) =
                        make_proxies(&library, queue, &cancel, &replies, &send_clips);
                    let mut text = format!("Made {made} {}.", plural(made, "proxy", "proxies"));
                    if failed > 0 {
                        text.push_str(&format!(" {failed} failed: see the Proxy column."));
                    }
                    if cancel.load(Ordering::Relaxed) {
                        text.push_str(" Stopped.");
                    }
                    let _ = replies.send(Reply::Done(text));
                }
                Command::AutoPlace(jobs, references) => {
                    let _ = replies.send(Reply::Done(auto_place(
                        &library,
                        &jobs,
                        &references,
                        &cancel,
                        &replies,
                    )));
                }
            }
        }
    });
}

/// Places markers in each clip from the course that matches it best, as an
/// `auto` draft; returns what happened.
fn auto_place(
    library: &Library,
    jobs: &[(PathBuf, PathBuf)],
    references: &[Reference],
    cancel: &Cancel,
    replies: &mpsc::Sender<Reply>,
) -> String {
    let cache = auto::cache_dir();
    let (mut placed, mut to_check, mut unmatched, mut failed) = (0, 0, Vec::new(), Vec::new());
    for (i, (clip, footage)) in jobs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let name = clip
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let _ = replies.send(Reply::Status(
            format!("Matching {name} ({} of {})…", i + 1, jobs.len()),
            Some(i as f64 / jobs.len() as f64),
        ));
        match auto::match_courses(references, footage, cache.as_deref()) {
            Ok(ranked) if ranked[0].match_cost < auto::POOR_MATCH => {
                let best = &ranked[0];
                let draft = Draft {
                    course_id: best.course_id.clone(),
                    frames: best.markers.clone(),
                    confidence: best.confidence.clone(),
                    source: "auto".into(),
                };
                match library.set_draft(clip, &draft) {
                    Ok(()) => {
                        placed += 1;
                        if !best.to_check().is_empty() {
                            to_check += 1;
                        }
                    }
                    Err(e) => failed.push(format!("{name}: {e:#}")),
                }
            }
            Ok(_) => unmatched.push(name),
            Err(e) => failed.push(format!("{name}: {e:#}")),
        }
    }
    let mut text = format!(
        "Placed markers in {placed} {}",
        plural(placed, "clip", "clips")
    );
    text.push_str(&match to_check {
        0 if placed > 0 => ": double-click each to check and commit.".to_owned(),
        0 => ".".to_owned(),
        n => format!(" ({n} with markers to check by eye): double-click each to check and commit."),
    });
    if !unmatched.is_empty() {
        text.push_str(&format!(
            " No course matched {}: time {} by hand.",
            unmatched.join(", "),
            if unmatched.len() == 1 { "it" } else { "them" }
        ));
    }
    if !failed.is_empty() {
        text.push_str(&format!(" Failed: {}.", failed.join("; ")));
    }
    if cancel.load(Ordering::Relaxed) {
        text.push_str(" Stopped.");
    }
    text
}

/// Auto-places the ticked clips that have neither a run nor a draft yet.
fn start_auto(p: &Page) {
    let Some(db_path) = p.shared.settings().run_database.clone() else {
        p.status
            .set_text("Choose your run database first (the gear in the sidebar).");
        return;
    };
    let db = match timer_core::TimerDatabase::load(&db_path) {
        Ok(db) => db,
        Err(e) => {
            p.status
                .set_text(&format!("The run database couldn't be read: {e}"));
            return;
        }
    };
    let s = p.state.borrow();
    let references = match Library::open_default().and_then(|l| auto::references(&db, &l, &s.clips))
    {
        Ok(r) if r.is_empty() => {
            p.status.set_text(
                "No course has a run with footage to match against yet: time one run on each course by hand first.",
            );
            return;
        }
        Ok(r) => r,
        Err(e) => {
            p.status.set_text(&format!("{e:#}"));
            return;
        }
    };
    let mut skipped = 0;
    let jobs: Vec<(PathBuf, PathBuf)> = s
        .clips
        .iter()
        .filter(|c| s.selected.contains(&c.path) && c.info.is_some())
        .filter(|c| {
            let done = s.runs.contains_key(&run_key(&c.path)) || s.drafted.contains(&c.path);
            skipped += usize::from(done);
            !done
        })
        .map(|c| (c.path.clone(), auto::footage(c)))
        .collect();
    drop(s);
    if jobs.is_empty() {
        p.status.set_text(
            "The ticked clips already have runs or drafts: double-click one to place its markers again.",
        );
        return;
    }
    let mut s = p.state.borrow_mut();
    s.busy = true;
    drop(s);
    p.status.set_text(&format!(
        "Auto-placing {} {}{}…",
        jobs.len(),
        plural(jobs.len(), "clip", "clips"),
        if skipped > 0 {
            format!(" (skipping {skipped} with runs or drafts)")
        } else {
            String::new()
        }
    ));
    p.progress.set_visible(true);
    refresh_controls(p);
    let _ = p.tx.send(Command::AutoPlace(jobs, references));
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
            fill_courses(p);
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
            s.queued.remove(&path);
            s.making.insert(path);
            drop(s);
            refill(p);
        }
        Reply::Made(path) => {
            p.state.borrow_mut().making.remove(&path);
            refill(p);
        }
        Reply::Done(text) | Reply::Error(text) => {
            let mut s = p.state.borrow_mut();
            s.busy = false;
            s.making.clear();
            s.queued.clear();
            drop(s);
            p.status.set_text(&text);
            p.progress.set_visible(false);
            // Auto-placing makes drafts; show them.
            load_runs(p);
            fill_courses(p);
            refill(p);
        }
    }
}

fn start_scan(p: &Page) {
    let folders = p.shared.settings().footage_folders.clone();
    if p.state.borrow().busy {
        return;
    }
    p.state.borrow_mut().scanned = folders.clone();
    if folders.is_empty() {
        // Nothing to scan: forget clips from folders that were removed.
        let _ = Library::open_default().and_then(|l| l.retain(&HashSet::new()));
        p.state.borrow_mut().clips.clear();
        refill(p);
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
    let no_folders = p.shared.settings().footage_folders.is_empty();
    p.banner.set_visible(no_folders || s.runs_error.is_some());
    p.banner_text.set_text(if no_folders {
        "Add the folders your footage is in. Ghost Run lists every clip in them and keeps the list up to date."
    } else {
        s.runs_error.as_deref().unwrap_or("")
    });
    p.rescan.set_sensitive(!s.busy && !no_folders);
    p.proxies.set_sensitive(!s.busy && !s.clips.is_empty());
    p.time.set_sensitive(s.selected.len() == 1);
    p.auto_place
        .set_sensitive(!s.busy && !s.selected.is_empty());
    p.move_course.set_sensitive(!ticked_runs(&s).is_empty());
    p.rename.set_sensitive(matches!(s.view, View::Course(_)));
    p.proxies.set_tooltip_text(Some(if s.selected.is_empty() {
        "Make a proxy for every clip without one"
    } else {
        "Make proxies for the ticked clips"
    }));
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
        .filter(|c| shown_in(&s.view, s.runs.get(&run_key(&c.path))))
        .map(|c| glib::BoxedAnyObject::new(c.clone()))
        .collect();
    drop(s);
    p.store.remove_all();
    p.store.extend_from_slice(&rows);
    refresh_controls(p);
}

/// Title, width, and which cell ([`cell`]) each column shows, in order.
const COLUMNS: &[(&str, i32, usize)] = &[
    ("Clip", 240, 0),
    ("Course", 170, 5),
    ("Lap", 120, 6),
    ("Recorded", 175, 1),
    ("Length", 70, 2),
    ("Proxy", 90, 4),
    ("Video", 190, 3),
    ("Folder", 200, 7),
];

fn table(p: &Page) -> gtk::ColumnView {
    let selection = gtk::NoSelection::new(Some(p.store.clone()));
    let view = gtk::ColumnView::new(Some(selection));
    view.add_css_class("footage-table");
    let page = p.clone();
    view.connect_activate(move |_, position| {
        let clip = page
            .store
            .item(position)
            .and_downcast::<glib::BoxedAnyObject>()
            .map(|o| o.borrow::<Clip>().path.clone());
        if let Some(path) = clip {
            page.shared.time_clip(path);
        }
    });
    view.set_show_row_separators(true);
    for &(title, width, index) in COLUMNS {
        let factory = gtk::SignalListItemFactory::new();
        let page = p.clone();
        factory.connect_setup(move |_, item| {
            let li = item.downcast_ref::<gtk::ListItem>().unwrap();
            let text = label("", if index == 4 { "status" } else { "" });
            // Names and folders can be shortened; the rest is short and must show whole.
            if matches!(index, 0 | 5 | 7) {
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
            let ticked = s.selected.contains(&clip.path);
            // Let go first: ticking the box below runs its handler, which changes the state.
            drop(s);
            let label = if index == 0 {
                let row = li.child().and_downcast::<gtk::Box>().unwrap();
                let check = row
                    .first_child()
                    .and_downcast::<gtk::CheckButton>()
                    .unwrap();
                check.set_active(ticked);
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
        column.set_fixed_width(width);
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
            if s.making.contains(&clip.path) {
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
        5 => {
            let run = s.runs.get(&run_key(&clip.path));
            let drafted = s.drafted.contains(&clip.path);
            match run {
                Some(r) => {
                    let mut text = r.course.clone();
                    if r.ignored {
                        text.push_str(" (ignored)");
                    }
                    if drafted {
                        text.push_str(" (changed)");
                    }
                    let tip = if drafted {
                        format!("{}: markers changed, not committed yet", r.id)
                    } else {
                        r.id.clone()
                    };
                    (text, Some(tip))
                }
                None if drafted => (
                    "Draft".into(),
                    Some(
                        "Markers placed, not committed yet: double-click to check and commit"
                            .into(),
                    ),
                ),
                None => (String::new(), None),
            }
        }
        6 => match s.runs.get(&run_key(&clip.path)) {
            Some(r) => (lap_text(r), None),
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
    let path = p.shared.settings().run_database.clone();
    let mut s = p.state.borrow_mut();
    s.runs.clear();
    s.courses.clear();
    s.runs_error = None;
    s.drafted = Library::open_default()
        .and_then(|l| l.drafted())
        .unwrap_or_default();
    let Some(path) = path else { return };
    match timer_core::TimerDatabase::load(&path) {
        Ok(db) => {
            s.runs = runs_by_clip(&db);
            s.courses = db
                .courses
                .iter()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect();
        }
        Err(e) => s.runs_error = Some(format!("The run database couldn't be read: {e}")),
    }
    if let View::Course(id) = &s.view
        && !s.courses.iter().any(|(c, _)| c == id)
    {
        s.view = View::All;
    }
}

/// Each run by the name its clip would have ([`run_key`]), with its course,
/// lap and place on the course's board.
fn runs_by_clip(db: &timer_core::TimerDatabase) -> HashMap<String, RunInfo> {
    let mut places: HashMap<&str, (Option<f64>, Option<usize>)> = HashMap::new();
    let boards: Vec<_> = db
        .courses
        .iter()
        .map(|c| course_board(c, &db.runs))
        .collect();
    for board in &boards {
        for row in &board.rows {
            places.insert(row.run_id.as_str(), (row.lap_seconds, row.rank));
        }
    }
    db.runs
        .iter()
        .map(|run| {
            let course = db
                .course_by_id(&run.course_id)
                .map(|c| c.name.clone())
                .unwrap_or_else(|_| run.course_id.clone());
            let (lap, rank) = places.get(run.id.as_str()).copied().unwrap_or_default();
            (
                run_key(Path::new(&run.filename)),
                RunInfo {
                    id: run.id.clone(),
                    course_id: run.course_id.clone(),
                    course,
                    lap,
                    rank,
                    ignored: run.ignored,
                },
            )
        })
        .collect()
}

/// Whether a clip with this run (or none) shows in `view`.
fn shown_in(view: &View, run: Option<&RunInfo>) -> bool {
    match view {
        View::All => true,
        View::Untimed => run.is_none(),
        View::Course(id) => run.is_some_and(|r| &r.course_id == id),
    }
}

/// "2:31.400 · 3rd", or just the lap for a run that isn't ranked.
fn lap_text(r: &RunInfo) -> String {
    let Some(lap) = r.lap.and_then(|l| format_duration(l).ok()) else {
        return String::new();
    };
    match r.rank {
        Some(1) => format!("{lap} · best"),
        Some(n) => format!("{lap} · #{n}"),
        None => lap,
    }
}

/// The run ids of the ticked clips that have runs.
fn ticked_runs(s: &State) -> Vec<String> {
    let mut ids: Vec<String> = s
        .selected
        .iter()
        .filter_map(|path| s.runs.get(&run_key(path)).map(|r| r.id.clone()))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Rebuilds the Courses panel: all clips, not timed yet, and each course
/// with how many clips are on it; keeps the selection.
fn fill_courses(p: &Page) {
    let s = p.state.borrow();
    let count = |view: &View| {
        s.clips
            .iter()
            .filter(|c| shown_in(view, s.runs.get(&run_key(&c.path))))
            .count()
    };
    let mut rows: Vec<(String, String, usize)> = vec![
        ("all".into(), "All clips".into(), s.clips.len()),
        (
            "untimed".into(),
            "Not timed yet".into(),
            count(&View::Untimed),
        ),
    ];
    for (id, name) in &s.courses {
        rows.push((
            format!("course:{id}"),
            name.clone(),
            count(&View::Course(id.clone())),
        ));
    }
    let selected = match &s.view {
        View::All => "all".to_owned(),
        View::Untimed => "untimed".to_owned(),
        View::Course(id) => format!("course:{id}"),
    };
    drop(s);
    p.filling.set(true);
    while let Some(row) = p.course_list.first_child() {
        p.course_list.remove(&row);
    }
    for (key, name, n) in rows {
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = label(&name, "");
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&title);
        line.append(&label(&n.to_string(), "dim-label"));
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&line));
        row.set_widget_name(&key);
        row.set_tooltip_text(Some(&name));
        row.update_property(&[gtk::accessible::Property::Label(&format!("{name}, {n}"))]);
        let is_selected = key == selected;
        p.course_list.append(&row);
        if is_selected {
            p.course_list.select_row(Some(&row));
        }
    }
    p.filling.set(false);
}

/// Moves the ticked clips' runs to another course.
fn move_ticked(p: &Page) {
    let s = p.state.borrow();
    let ids = ticked_runs(&s);
    let courses: HashSet<&str> = s
        .selected
        .iter()
        .filter_map(|path| s.runs.get(&run_key(path)).map(|r| r.course_id.as_str()))
        .collect();
    let from = (courses.len() == 1).then(|| courses.iter().next().unwrap().to_string());
    drop(s);
    if ids.is_empty() {
        return;
    }
    let p2 = p.clone();
    crate::courses::move_runs(&p.shared, ids, from.as_deref(), move |result| {
        p2.state.borrow_mut().selected.clear();
        refill(&p2);
        p2.status.set_text(&result.unwrap_or_else(|e| e));
    });
}

/// Times a clip from anywhere, not just the footage folders.
fn choose_file(p: &Page) {
    let dialog = gtk::FileDialog::new();
    dialog.set_title("Open a clip to time");
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Video"));
    filter.add_mime_type("video/*");
    for pattern in ["*.mp4", "*.MP4", "*.mov", "*.MOV"] {
        filter.add_pattern(pattern);
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    let shared = p.shared.clone();
    dialog.open(Some(&p.window), gio::Cancellable::NONE, move |result| {
        if let Ok(Some(path)) = result.map(|f| f.path()) {
            shared.time_clip(path);
        }
    });
}

fn duration(seconds: f64) -> String {
    let s = seconds.round() as i64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
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
    fn shows_each_clips_course_lap_and_place() {
        use timer_core::models::{Course, MarkerFrames, RunRecord};
        let run = |id: &str, file: &str, finish: i64, ignored: bool| {
            let frames: MarkerFrames =
                [("Start".to_owned(), 0), ("Finish".to_owned(), finish)].into();
            let mut r = RunRecord::new(id, "lw", "2026-09-01", file, 30.0, frames);
            r.ignored = ignored;
            r
        };
        let db = timer_core::TimerDatabase {
            courses: vec![Course {
                id: "lw".into(),
                name: "Lower Woods".into(),
                sector_count: 1,
            }],
            runs: vec![
                run("r1", "A.MP4", 300, false),
                run("r2", "B.MP4", 270, false),
                run("r3", "C.MP4", 240, true),
            ],
        };
        let runs = runs_by_clip(&db);
        let a = &runs["a.mp4"];
        assert_eq!((a.course.as_str(), a.rank), ("Lower Woods", Some(2)));
        assert_eq!(lap_text(a), "0:10.000 · #2");
        assert_eq!(lap_text(&runs["b.mp4"]), "0:09.000 · best");
        // Ignored: its lap, no place.
        assert_eq!(lap_text(&runs["c.mp4"]), "0:08.000");

        assert!(shown_in(&View::All, None));
        assert!(shown_in(&View::Untimed, None));
        assert!(!shown_in(&View::Untimed, Some(a)));
        assert!(shown_in(&View::Course("lw".into()), Some(a)));
        assert!(!shown_in(&View::Course("other".into()), Some(a)));
        assert!(!shown_in(&View::Course("lw".into()), None));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(65.4), "1:05");
        assert_eq!(duration(3725.0), "1:02:05");
    }
}
