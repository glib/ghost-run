//! The Library page: every clip in your footage folders, when it was recorded,
//! whether it has a proxy, and which stored run (if any) it is: its course,
//! lap and place. The panel beside it filters the table: views (not timed
//! yet, not on the NAS, drafts, picks…), collections, tags, courses and rides
//! (`library::organise`), and adds and renames courses, rides and
//! collections. Ticked clips can be flagged, tagged and put in collections
//! (the Organise menu). Each row shows a thumbnail, made on a thread of its
//! own from the clip's proxy or preview where there is one. Timing a clip opens the Time page over the
//! Library, with a back arrow.
//!
//! Clips whose original is on the NAS (Import's archive folder, the footage
//! root) are listed once, with their local copy if there is one
//! (`library::footage`), and stay listed while the NAS is offline. After each
//! scan the catalog's previews are made for NAS clips that have none
//! (`library::preview`), so timing and auto-place don't need the full-size
//! files.
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
use library::backup::Backup;
use library::footage::Roots;
use library::organise::{self, Catalog, Flag, Ride};
use library::scan::{self, Progress};
use library::{Cancel, Clip, Draft, Library, MediaInfo, ProxyState, proxy};
use timer_core::board::course_board;
use timer_core::timing::format_duration;

use crate::shared::{Change, Shared};

enum Command {
    /// Scan these folders, then back the catalog up to the NAS archive (with
    /// the run database) if there is one.
    /// Then makes previews for NAS clips without one.
    Scan(Vec<PathBuf>, Roots, Option<BackupTo>),
    /// Each clip (its Library path, the file to make the proxy of, its info).
    Proxies(Vec<(PathBuf, PathBuf, MediaInfo)>),
    /// Place markers in each clip (path, footage to read) from the best-matching course.
    AutoPlace(Vec<(PathBuf, PathBuf)>, Vec<Reference>),
    /// Copy these NAS clips down to edit, with proxies.
    Hydrate(Vec<Clip>, Roots),
    /// Remove these NAS clips' local copies.
    Dehydrate(Vec<Clip>, Roots),
}

struct BackupTo {
    /// Import's archive, checked to be the share it was set up as (an
    /// unmounted share leaves an empty local folder behind).
    archive: ingest::settings::Destination,
    run_database: Option<PathBuf>,
}

enum Reply {
    Clips(Vec<Clip>),
    Status(String, Option<f64>),
    /// A clip's proxy is being made now.
    Making(PathBuf),
    /// A clip's proxy is finished (made or not).
    Made(PathBuf),
    /// Whether each clip's original is on the NAS, after a scan.
    Backups(HashMap<PathBuf, Backup>),
    /// A clip's thumbnail: the clip, the PNG.
    Thumb(PathBuf, PathBuf),
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
    /// No verified original on the NAS.
    NotOnNas,
    /// NAS clips with a local copy.
    Local,
    /// Clips with markers placed but not committed.
    Drafts,
    Picks,
    Rejected,
    /// In no video.
    Unused,
    /// In a video not published yet.
    InVideo,
    /// In a published video's final cut.
    Released,
    Course(String),
    /// A ride, by its id ([`Ride::id`]).
    Ride(String),
    Collection(i64),
    Tag(String),
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
    drafted: HashMap<PathBuf, Draft>,
    selected: HashSet<PathBuf>,
    making: HashSet<PathBuf>,
    queued: HashSet<PathBuf>,
    busy: bool,
    only_missing: bool,
    /// Whether each clip's original is on the NAS (from the import journal).
    backups: HashMap<PathBuf, Backup>,
    /// The import journal's last change when the Library last scanned, so
    /// coming back after an import rescans.
    journal_seen: Option<std::time::SystemTime>,
    /// The NAS footage root and local root as of the last scan.
    roots: Roots,
    /// What the local copies of NAS clips take, in bytes.
    local_bytes: u64,
    /// Just imported: to auto-place once the next scan has found them.
    pending_auto: Vec<PathBuf>,
    /// A scan is wanted as soon as the worker is free.
    rescan_wanted: bool,
    /// The worker's current job is a scan.
    scanning: bool,
    /// Tags, flags, collections, ride names.
    catalog: Catalog,
    /// Clip key → the kinds of video it's in.
    uses: HashMap<String, library::videos::Used>,
    rides: Vec<Ride>,
    /// Clip path → its thumbnail, once made.
    thumbs: HashMap<PathBuf, PathBuf>,
    /// Asked for, not made yet.
    thumbs_asked: HashSet<PathBuf>,
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
    add_first: gtk::Button,
    show_local: gtk::Button,
    copy_here: gtk::Button,
    remove_local: gtk::Button,
    counts: gtk::Label,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    rescan: gtk::Button,
    proxies: gtk::Button,
    time: gtk::Button,
    auto_place: gtk::Button,
    move_course: gtk::Button,
    ticked_bar: gtk::Box,
    ticked_label: gtk::Label,
    render: gtk::Button,
    to_resolve: gtk::Button,
    stop: gtk::Button,
    course_list: gtk::ListBox,
    rename: gtk::Button,
    uncollect: gtk::Button,
    /// Clips whose thumbnails to make: path, file to read.
    thumbs_tx: mpsc::Sender<(PathBuf, PathBuf)>,
    /// The thumbnail pictures bound now, by clip path.
    pictures: Rc<RefCell<HashMap<PathBuf, gtk::Picture>>>,
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
    let replies_for_thumbs = replies.clone();
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
    let show_local = button("Show them");
    banner.append(&banner_text);
    banner.append(&add_first);
    banner.append(&show_local);
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
    toolbar.append(&proxies);
    content.append(&toolbar);

    // What can be done with the ticked clips, shown while any are ticked.
    let ticked_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    ticked_bar.add_css_class("locations");
    let ticked_label = label("", "");
    ticked_label.set_hexpand(true);
    ticked_label.set_wrap(true);
    let auto_place = button("Auto-place");
    auto_place.set_tooltip_text(Some(
        "Find each ticked clip's course and place its markers from that course's reference run, as drafts to check and commit",
    ));
    let render = button("Render overlays");
    let to_resolve = crate::resolve_menu::menu();
    let copy_here = button("Copy here");
    copy_here.set_tooltip_text(Some(
        "Copy the ticked clips from the NAS to this computer to edit, with proxies",
    ));
    let remove_local = button("Remove local copy");
    remove_local.set_tooltip_text(Some(
        "Remove the ticked clips' local copies and proxies; the originals stay on the NAS",
    ));
    let (organise, organise_items) = crate::organise_ui::menu_button(
        "Organise",
        &[
            ("Pick", "Flag the ticked clips as picks"),
            ("Reject", "Flag the ticked clips as rejects"),
            ("Clear flag", ""),
            ("Tags…", "Add or remove the ticked clips' tags"),
            ("Add to collection…", "Put the ticked clips in a collection"),
            (
                "Add to video…",
                "Put the ticked clips in a video, to build in Resolve",
            ),
            ("Remove from this collection", ""),
            (
                "Move runs to course…",
                "Move the ticked clips' runs to another course",
            ),
        ],
    );
    let move_course = organise_items[7].clone();
    organise.set_tooltip_text(Some("Flag, tag or collect the ticked clips"));
    let untick = gtk::Button::from_icon_name("edit-clear-symbolic");
    untick.add_css_class("flat");
    untick.set_tooltip_text(Some("Untick all"));
    // The buttons wrap onto a second line when the window is narrow.
    let ticked_flow = gtk::FlowBox::new();
    ticked_flow.set_selection_mode(gtk::SelectionMode::None);
    ticked_flow.set_hexpand(true);
    ticked_flow.set_max_children_per_line(12);
    ticked_flow.set_row_spacing(6);
    ticked_flow.set_column_spacing(6);
    ticked_label.set_hexpand(false);
    ticked_label.set_wrap(false);
    ticked_bar.append(&ticked_flow);
    for w in [
        ticked_label.upcast_ref::<gtk::Widget>(),
        organise.upcast_ref(),
        auto_place.upcast_ref(),
        time.upcast_ref(),
        render.upcast_ref(),
        to_resolve.button.upcast_ref(),
        copy_here.upcast_ref(),
        remove_local.upcast_ref(),
        untick.upcast_ref(),
    ] {
        let child = gtk::FlowBoxChild::new();
        child.set_focusable(false);
        child.set_child(Some(w));
        // A hidden button takes no room.
        w.bind_property("visible", &child, "visible")
            .sync_create()
            .build();
        ticked_flow.insert(&child, -1);
    }
    ticked_bar.set_visible(false);
    content.append(&ticked_bar);

    // The Courses panel: all clips, those not timed yet, and each course.
    let course_list = gtk::ListBox::new();
    course_list.add_css_class("navigation-sidebar");
    course_list.update_property(&[gtk::accessible::Property::Label(
        "Show clips by view, collection, tag, course or ride",
    )]);
    let course_scroller = gtk::ScrolledWindow::builder()
        .child(&course_list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let add_course = button("New…");
    add_course.set_tooltip_text(Some("Add a course"));
    let rename = button("Rename…");
    rename.set_tooltip_text(Some("Rename the selected course, ride or collection"));
    let course_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    course_buttons.set_homogeneous(true);
    course_buttons.append(&add_course);
    course_buttons.append(&rename);
    let course_panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
    course_panel.set_size_request(180, -1);
    course_panel.append(&label("Show", "heading"));
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
            drafted: HashMap::new(),
            selected: HashSet::new(),
            making: HashSet::new(),
            queued: HashSet::new(),
            busy: false,
            only_missing: false,
            backups: HashMap::new(),
            journal_seen: None,
            roots: Roots::default(),
            local_bytes: 0,
            pending_auto: Vec::new(),
            rescan_wanted: false,
            scanning: false,
            catalog: Catalog::default(),
            uses: HashMap::new(),
            rides: Vec::new(),
            thumbs: HashMap::new(),
            thumbs_asked: HashSet::new(),
        })),
        store: store.clone(),
        tx,
        cancel,
        banner,
        banner_text,
        add_first: add_first.clone(),
        show_local: show_local.clone(),
        copy_here: copy_here.clone(),
        remove_local: remove_local.clone(),
        counts,
        status,
        progress,
        rescan: rescan.clone(),
        proxies: proxies.clone(),
        time: time.clone(),
        auto_place: auto_place.clone(),
        move_course: move_course.clone(),
        ticked_bar,
        ticked_label,
        render: render.clone(),
        to_resolve: to_resolve.button.clone(),
        stop: stop.clone(),
        course_list: course_list.clone(),
        rename: rename.clone(),
        uncollect: organise_items[6].clone(),
        thumbs_tx: thumbs_worker(replies_for_thumbs),
        pictures: Rc::default(),
        filling: Rc::default(),
    };
    // Move runs to course… (the last) has its own handler.
    for (i, item) in organise_items[..7].iter().enumerate() {
        let p = page.clone();
        item.connect_clicked(move |_| organise_ticked(&p, i));
    }

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
    render.connect_clicked(move |_| {
        let ids = ticked_runs(&p.state.borrow());
        p.shared.render_runs(ids);
    });
    let p = page.clone();
    to_resolve.on_pick(move |target| {
        let ids = ticked_runs(&p.state.borrow());
        p.shared.send_to_resolve(target, ids);
    });
    let p = page.clone();
    copy_here.connect_clicked(move |_| start_hydrate(&p));
    let p = page.clone();
    remove_local.connect_clicked(move |_| confirm_dehydrate(&p));
    let p = page.clone();
    show_local.connect_clicked(move |_| {
        p.state.borrow_mut().view = View::Local;
        fill_courses(&p);
        refill(&p);
    });
    let p = page.clone();
    untick.connect_clicked(move |_| {
        p.state.borrow_mut().selected.clear();
        refill(&p);
    });
    let p = page.clone();
    course_list.connect_row_selected(move |_, row| {
        if p.filling.get() {
            return;
        }
        let Some(row) = row else { return };
        let name = row.widget_name();
        let view = match name.as_str() {
            "untimed" => View::Untimed,
            "nas" => View::NotOnNas,
            "local" => View::Local,
            "drafts" => View::Drafts,
            "picks" => View::Picks,
            "rejected" => View::Rejected,
            "unused" => View::Unused,
            "invideo" => View::InVideo,
            "released" => View::Released,
            n => view_named(n),
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
    rename.connect_clicked(move |_| rename_selected(&p));
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
        match change {
            Change::TimeClip
            | Change::Render
            | Change::Resolve
            | Change::BuildVideo
            | Change::ShowVideo => return,
            // After an import, show what needs checking.
            Change::ShowLibrary => {
                if !p.state.borrow().drafted.is_empty() {
                    p.state.borrow_mut().view = View::Drafts;
                    fill_courses(&p);
                    refill(&p);
                }
                return;
            }
            Change::Imported => {
                let new = p.shared.take_imported();
                let mut s = p.state.borrow_mut();
                s.pending_auto.extend(new);
                s.rescan_wanted = true;
                let busy = s.busy;
                drop(s);
                if !busy {
                    start_scan(&p);
                }
                return;
            }
            // The Videos page may have removed local copies.
            Change::Videos => {
                if let Ok(clips) = Library::open_default().and_then(|l| l.clips()) {
                    p.state.borrow_mut().clips = clips;
                }
            }
            _ => {}
        }
        load_runs(&p);
        fill_courses(&p);
        refill(&p);
        let folders = scan_folders(&p.shared, &roots());
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
        Ok(clips) => {
            let mut s = page.state.borrow_mut();
            s.rides = organise::rides(&clips, &s.catalog.ride_names);
            s.clips = clips;
        }
        Err(e) => page
            .status
            .set_text(&format!("Library could not be opened: {e:#}")),
    }
    refill(&page);
    if !scan_folders(shared, &roots()).is_empty() {
        start_scan(&page);
    }
    // Back on the Library after an import: rescan, so the new clips show.
    let p = page.clone();
    root.connect_map(move |_| {
        let changed = journal_changed();
        if changed.is_some() && changed != p.state.borrow().journal_seen {
            start_scan(&p);
        }
    });
    root.upcast()
}

/// Makes the queued proxies, several at once ([`proxy::plan`]), recording
/// each in the library as it finishes. Returns how many were made and failed.
fn make_proxies(
    library: &Library,
    queue: Vec<(PathBuf, PathBuf, MediaInfo)>,
    cancel: &Cancel,
    replies: &mpsc::Sender<Reply>,
    send_clips: &dyn Fn(&Library),
) -> (usize, usize) {
    use std::sync::Mutex;
    let total = queue.len();
    let (w, h) = queue
        .iter()
        .map(|(_, _, i)| (i.width, i.height))
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
                    let Some((i, (path, source, info))) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let _ = replies.send(Reply::Making(path.clone()));
                    let result = proxy::make_with(&source, &info, plan.threads, cancel, |f| {
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

/// Makes the catalog's preview for each NAS clip that has none, a few at a
/// time; returns a sentence for the status line (empty if there were none).
fn make_previews(
    library: &Library,
    roots: &Roots,
    cancel: &Cancel,
    replies: &mpsc::Sender<Reply>,
    send_clips: &dyn Fn(&Library),
) -> String {
    use std::sync::Mutex;
    let queue: Vec<Clip> = library
        .clips()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| roots.on_nas(&c.path) && c.preview.is_none() && c.info.is_some())
        // Offline, only clips with a local copy can be read.
        .filter(|c| roots.nas_online || c.local.is_some())
        .collect();
    if queue.is_empty() {
        return String::new();
    }
    let total = queue.len();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    // Decoding 4K is the slow part; a few at once keeps the cores busy
    // without the NAS becoming the limit.
    let jobs = (cores / 8).clamp(1, 4).min(total);
    let threads = (cores / jobs).max(1);
    let queue = Mutex::new(queue.into_iter().enumerate().collect::<VecDeque<_>>());
    let progress = Mutex::new(vec![0.0f64; total]);
    let finished = Mutex::new(0usize);
    let (results, done) = mpsc::channel::<(PathBuf, Result<PathBuf, String>)>();
    let report = |finished: usize| {
        let fraction = progress.lock().unwrap().iter().sum::<f64>() / total as f64;
        let _ = replies.send(Reply::Status(
            format!("Making previews of NAS clips: {finished} of {total} done"),
            Some(fraction),
        ));
    };
    let catalog = roots.catalog.clone();
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let results = results.clone();
            let (queue, progress, finished, report, catalog) =
                (&queue, &progress, &finished, &report, &catalog);
            scope.spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some((i, clip)) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let result = library::preview::make(catalog, &clip, threads, cancel, |f| {
                        progress.lock().unwrap()[i] = f;
                        report(*finished.lock().unwrap());
                    });
                    progress.lock().unwrap()[i] = 1.0;
                    *finished.lock().unwrap() += 1;
                    report(*finished.lock().unwrap());
                    let _ = results.send((clip.path, result.map_err(|e| format!("{e:#}"))));
                }
            });
        }
        drop(results);
    });
    let (mut made, mut failed) = (0usize, Vec::new());
    for (path, result) in done {
        match result {
            Ok(preview) => {
                made += 1;
                let _ = library.set_preview(&path, Some(preview));
            }
            Err(_) if cancel.load(Ordering::Relaxed) => {}
            Err(e) => failed.push(format!(
                "{}: {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }
    send_clips(library);
    let mut text = format!(" Made {made} {}.", plural(made, "preview", "previews"));
    if let Some(first) = failed.first() {
        text.push_str(&format!(" {} failed ({first}).", failed.len()));
    }
    text
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
                Command::Scan(folders, roots, backup_to) => {
                    let result = scan::scan(&folders, &roots, &library, &cancel, |p| {
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
                    if let Ok(clips) = library.clips() {
                        let _ = replies.send(Reply::Backups(backups(&clips)));
                    }
                    let backed_up = backup_to.map(|b| {
                        ingest::storage::Root::destination(&b.archive).and_then(|_| {
                            library::catalog_backup::run(
                                &library,
                                b.run_database.as_deref(),
                                &b.archive.path,
                                chrono::Local::now().date_naive(),
                            )
                        })
                    });
                    let offline = if roots.nas.is_some() && !roots.nas_online {
                        " The NAS isn't mounted: its clips are shown as last catalogued."
                    } else {
                        ""
                    };
                    let note = offline.to_owned()
                        + &match backed_up {
                            Some(Ok(r)) if !r.written.is_empty() => {
                                " Catalog backed up to the NAS.".to_owned()
                            }
                            Some(Err(_)) if !offline.is_empty() => String::new(),
                            Some(Err(e)) => format!(" Catalog backup to the NAS failed: {e:#}."),
                            _ => String::new(),
                        };
                    let previews = match &result {
                        Ok(_) if !cancel.load(Ordering::Relaxed) => {
                            make_previews(&library, &roots, &cancel, &replies, &send_clips)
                        }
                        _ => String::new(),
                    };
                    let _ = replies.send(match result {
                        Ok(r) if cancel.load(Ordering::Relaxed) => Reply::Done(format!(
                            "Stopped after reading {} clips.{note}{previews}",
                            r.probed
                        )),
                        Ok(r) => Reply::Done(scan_summary(&r) + &note + &previews),
                        Err(e) => Reply::Error(format!("Scan failed: {e:#}.{note}")),
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
                Command::Hydrate(clips, roots) => {
                    let text = hydrate(&library, &clips, &roots, &cancel, &replies);
                    send_clips(&library);
                    let _ = replies.send(Reply::Done(text));
                }
                Command::Dehydrate(clips, roots) => {
                    let removed = library::hydrate::remove_local_copies(&library, &clips, &roots);
                    send_clips(&library);
                    let _ = replies.send(Reply::Done(removed.text()));
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
    let ticked: Vec<PathBuf> = p.state.borrow().selected.iter().cloned().collect();
    start_auto_for(p, &ticked, false);
}

/// Auto-places `paths` (clips without a run or draft). `quiet`: just after an
/// import, so the status line only speaks if there's something to place.
fn start_auto_for(p: &Page, paths: &[PathBuf], quiet: bool) {
    // After an import, a missing database or reference just means no drafts.
    let say = |text: &str| {
        if !quiet {
            p.status.set_text(text);
        }
    };
    let Some(db_path) = p.shared.settings().run_database.clone() else {
        say("Choose your run database first (the gear in the sidebar).");
        return;
    };
    let db = match timer_core::TimerDatabase::load(&db_path) {
        Ok(db) => db,
        Err(e) => {
            say(&format!("The run database couldn't be read: {e}"));
            return;
        }
    };
    let s = p.state.borrow();
    let references = match Library::open_default().and_then(|l| auto::references(&db, &l, &s.clips))
    {
        Ok(r) if r.is_empty() => {
            say(
                "No course has a run with footage to match against yet: time one run on each course by hand first.",
            );
            return;
        }
        Ok(r) => r,
        Err(e) => {
            say(&format!("{e:#}"));
            return;
        }
    };
    let mut skipped = 0;
    let jobs: Vec<(PathBuf, PathBuf)> = s
        .clips
        .iter()
        .filter(|c| paths.contains(&c.path) && c.info.is_some())
        .filter(|c| {
            let done = s.runs.contains_key(&run_key(&c.path)) || s.drafted.contains_key(&c.path);
            skipped += usize::from(done);
            !done
        })
        .map(|c| (c.path.clone(), auto::footage(c)))
        .collect();
    drop(s);
    if jobs.is_empty() {
        say(
            "The ticked clips already have runs or drafts: double-click one to place its markers again.",
        );
        return;
    }
    let mut s = p.state.borrow_mut();
    s.busy = true;
    drop(s);
    p.status.set_text(&format!(
        "{}Auto-placing {} {}{}…",
        if quiet { "Imported clips: " } else { "" },
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
            s.local_bytes = library::hydrate::local_bytes(&clips, &s.roots);
            s.rides = organise::rides(&clips, &s.catalog.ride_names);
            s.clips = clips;
            drop(s);
            fill_courses(p);
            refill(p);
        }
        Reply::Thumb(path, png) => {
            if let Some(picture) = p.pictures.borrow().get(&path)
                && *picture.widget_name() == *path.to_string_lossy()
            {
                picture.set_filename(Some(&png));
            }
            let mut s = p.state.borrow_mut();
            s.thumbs_asked.remove(&path);
            s.thumbs.insert(path, png);
        }
        Reply::Backups(backups) => {
            p.state.borrow_mut().backups = backups;
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
            let was_scan = std::mem::take(&mut s.scanning);
            let rescan = s.rescan_wanted;
            let imported = if was_scan && !rescan {
                std::mem::take(&mut s.pending_auto)
            } else {
                Vec::new()
            };
            drop(s);
            p.status.set_text(&text);
            p.progress.set_visible(false);
            // Auto-placing makes drafts; show them.
            load_runs(p);
            fill_courses(p);
            refill(p);
            if rescan {
                start_scan(p);
            } else if !imported.is_empty() {
                start_auto_for(p, &imported, true);
            }
        }
    }
}

fn start_scan(p: &Page) {
    let roots = roots();
    let folders = scan_folders(&p.shared, &roots);
    if p.state.borrow().busy {
        return;
    }
    p.state.borrow_mut().roots = roots.clone();
    p.state.borrow_mut().scanned = folders.clone();
    p.state.borrow_mut().rescan_wanted = false;
    p.state.borrow_mut().journal_seen = journal_changed();
    if folders.is_empty() && (roots.nas.is_none() || roots.nas_online) {
        // Nothing to scan: forget clips from folders that were removed.
        let _ = Library::open_default().and_then(|l| l.retain(&HashSet::new()));
        p.state.borrow_mut().clips.clear();
        refill(p);
        return;
    }
    p.state.borrow_mut().busy = true;
    p.state.borrow_mut().scanning = true;
    refresh_controls(p);
    let backup_to = ingest::settings::Settings::load()
        .ok()
        .and_then(|s| s.archive)
        .map(|a| BackupTo {
            archive: a,
            run_database: p.shared.settings().run_database.clone(),
        });
    let _ = p.tx.send(Command::Scan(folders, roots, backup_to));
}

/// The clips "Make proxies" would work on: the ticked ones, or else every
/// readable clip without a good proxy. A NAS clip gets one only while it has
/// a local copy (the proxy goes beside that).
fn proxy_queue(s: &State) -> Vec<(PathBuf, PathBuf, MediaInfo)> {
    s.clips
        .iter()
        .filter(|c| s.selected.is_empty() || s.selected.contains(&c.path))
        .filter(|c| !matches!(c.proxy, ProxyState::Ready(_)))
        .filter(|c| !s.roots.on_nas(&c.path) || c.local.is_some())
        .filter_map(|c| Some((c.path.clone(), c.editable().to_owned(), c.info.clone()?)))
        .collect()
}

fn confirm_proxies(p: &Page) {
    let queue = proxy_queue(&p.state.borrow());
    if queue.is_empty() {
        p.status.set_text(
            "Every clip here with a local copy already has a proxy (clips only on the NAS get one with a local copy).",
        );
        return;
    }
    let seconds: f64 = queue
        .iter()
        .filter_map(|(_, _, i)| i.duration_seconds)
        .sum();
    // ProRes 422 Proxy runs about 45 Mb/s at 1920x1080, 29.97 fps.
    let bytes: f64 = queue
        .iter()
        .map(|(_, _, i)| {
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
        s.queued = queue.iter().map(|(path, _, _)| path.clone()).collect();
        drop(s);
        refill(&p2);
        let _ = p2.tx.send(Command::Proxies(queue.clone()));
    });
    dialog.present(Some(&p.window));
}

fn refresh_controls(p: &Page) {
    let s = p.state.borrow();
    let no_folders = s.scanned.is_empty();
    let limit = p.shared.settings().local_copies_limit();
    let too_big = s.local_bytes > limit;
    p.banner
        .set_visible(no_folders || s.runs_error.is_some() || too_big);
    p.add_first.set_visible(no_folders);
    p.show_local
        .set_visible(!no_folders && s.runs_error.is_none() && too_big);
    p.banner_text.set_text(&if no_folders {
        "Add the folders your footage is in. Ghost Run lists every clip in them and keeps the list up to date.".to_owned()
    } else if let Some(e) = &s.runs_error {
        e.clone()
    } else {
        format!(
            "Local copies of NAS clips take {}, more than the {} set in Settings. Remove the ones you're not editing: they can be copied down again any time.",
            gb(s.local_bytes),
            gb(limit)
        )
    });
    let ticked: Vec<&Clip> = s
        .clips
        .iter()
        .filter(|c| s.selected.contains(&c.path) && s.roots.on_nas(&c.path))
        .collect();
    p.copy_here.set_visible(!ticked.is_empty());
    p.copy_here
        .set_sensitive(!s.busy && s.roots.nas_online && ticked.iter().any(|c| c.local.is_none()));
    p.remove_local.set_visible(!ticked.is_empty());
    p.remove_local
        .set_sensitive(!s.busy && ticked.iter().any(|c| c.local.is_some()));
    p.rescan.set_sensitive(!s.busy && !no_folders);
    p.proxies.set_sensitive(!s.busy && !s.clips.is_empty());
    p.time.set_sensitive(s.selected.len() == 1);
    p.auto_place
        .set_sensitive(!s.busy && !s.selected.is_empty());
    let runs = ticked_runs(&s).len();
    p.move_course.set_sensitive(runs > 0);
    p.render.set_sensitive(runs > 0);
    p.to_resolve.set_sensitive(runs > 0);
    let why = (runs == 0).then_some("None of the ticked clips is timed yet: time them first");
    p.render.set_tooltip_text(Some(
        why.unwrap_or("Render the ticked clips' timer overlays (in Jobs)"),
    ));
    p.to_resolve.set_tooltip_text(Some(why.unwrap_or(
        "Put the ticked clips' runs in DaVinci Resolve, rendering any overlay that's missing or out of date first",
    )));
    p.ticked_bar.set_visible(!s.selected.is_empty());
    p.ticked_label
        .set_text(&ticked_text(s.selected.len(), runs));
    p.rename.set_sensitive(matches!(
        s.view,
        View::Course(_) | View::Ride(_) | View::Collection(_)
    ));
    p.uncollect
        .set_visible(matches!(s.view, View::Collection(_)));
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
        .filter(|c| shows(&s, &s.view, c))
        .map(|c| glib::BoxedAnyObject::new(c.clone()))
        .collect();
    drop(s);
    p.store.remove_all();
    p.store.extend_from_slice(&rows);
    refresh_controls(p);
}

/// Title, width, and which cell ([`cell`]) each column shows, in order.
const COLUMNS: &[(&str, i32, usize)] = &[
    ("Clip", 300, 0),
    ("Tags", 200, 9),
    ("Course", 170, 5),
    ("Lap", 120, 6),
    ("Recorded", 175, 1),
    ("Length", 70, 2),
    ("Proxy", 90, 4),
    ("Stored", 120, 8),
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
            let text = label("", if matches!(index, 4 | 8) { "status" } else { "" });
            // Names, tags and folders can be shortened; the rest is short and must show whole.
            if matches!(index, 0 | 5 | 7) {
                text.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            }
            if index == 9 {
                text.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
            if index == 0 {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let check = gtk::CheckButton::new();
                row.append(&check);
                let picture = gtk::Picture::new();
                picture.set_size_request(64, 36);
                picture.set_content_fit(gtk::ContentFit::Cover);
                picture.add_css_class("thumb");
                row.append(&picture);
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
            let backup_class = match s.backups.get(&clip.path) {
                _ if s.roots.on_nas(&clip.path) => Some("success"),
                Some(Backup::OnNas { .. } | Backup::IsOriginal { .. }) => Some("success"),
                Some(Backup::LocalOnly) => Some("warning"),
                Some(Backup::Missing { .. }) => Some("error"),
                None => None,
            };
            let ticked = s.selected.contains(&clip.path);
            let thumb = s.thumbs.get(&clip.path).cloned();
            let ask_thumb = index == 0 && thumb.is_none() && !s.thumbs_asked.contains(&clip.path);
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
                let picture = check.next_sibling().and_downcast::<gtk::Picture>().unwrap();
                // Named by its clip, so a late thumbnail for a recycled row is ignored.
                picture.set_widget_name(&clip.path.to_string_lossy());
                picture.set_filename(thumb.as_deref());
                page.pictures
                    .borrow_mut()
                    .insert(clip.path.clone(), picture.clone());
                if ask_thumb {
                    page.state
                        .borrow_mut()
                        .thumbs_asked
                        .insert(clip.path.clone());
                    let _ = page.thumbs_tx.send((clip.path.clone(), clip.frames_from()));
                }
                row.last_child().and_downcast::<gtk::Label>().unwrap()
            } else {
                li.child().and_downcast::<gtk::Label>().unwrap()
            };
            label.set_text(&text);
            label.set_tooltip_text(tip.as_deref());
            if index == 8 {
                for class in ["success", "warning", "error"] {
                    label.remove_css_class(class);
                }
                if let Some(class) = backup_class {
                    label.add_css_class(class);
                }
            }
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
                ProxyState::Missing if s.roots.on_nas(&clip.path) && clip.local.is_none() => (
                    String::new(),
                    Some("Made with a local copy, when the clip is pulled down to edit".into()),
                ),
                ProxyState::Missing => ("None".into(), None),
            }
        }
        5 => {
            let run = s.runs.get(&run_key(&clip.path));
            let draft = s.drafted.get(&clip.path);
            let drafted = draft.is_some();
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
                None => match draft {
                    Some(d) => draft_text(s, d),
                    None => (String::new(), None),
                },
            }
        }
        9 => tags_text(s, clip),
        6 => match s.runs.get(&run_key(&clip.path)) {
            Some(r) => (lap_text(r), None),
            None => (String::new(), None),
        },
        8 if s.roots.on_nas(&clip.path) => nas_text(s, clip),
        8 => backup_text(s.backups.get(&clip.path)),
        _ => {
            let folder = clip.path.parent().unwrap_or(Path::new(""));
            (
                folder.display().to_string(),
                Some(folder.display().to_string()),
            )
        }
    }
}

/// The Tags cell: the flag, then the tags.
fn tags_text(s: &State, clip: &Clip) -> (String, Option<String>) {
    let key = organise::key(clip);
    let mut parts: Vec<String> = Vec::new();
    match s.catalog.flags.get(&key) {
        Some(Flag::Pick) => parts.push("★ Pick".into()),
        Some(Flag::Reject) => parts.push("✕ Reject".into()),
        None => {}
    }
    if let Some(tags) = s.catalog.tags_of(&key) {
        parts.extend(tags.iter().cloned());
    }
    let text = parts.join(" · ");
    let tip = (!text.is_empty()).then(|| text.clone());
    (text, tip)
}

/// A draft's Course cell: its course and, for auto-placed markers, how many
/// are worth checking by eye.
fn draft_text(s: &State, d: &Draft) -> (String, Option<String>) {
    let course = s
        .courses
        .iter()
        .find(|(id, _)| *id == d.course_id)
        .map_or("Draft", |(_, name)| name.as_str());
    let tip = "Markers placed, not committed yet: double-click to check and commit";
    match (d.source.as_str(), d.to_check()) {
        ("auto", 0) => (
            format!("{course}: looks right"),
            Some(format!("{tip}. Auto-placed, all markers confident.")),
        ),
        ("auto", n) => (
            format!("{course}: {n} to check"),
            Some(format!(
                "{tip}. Auto-place wasn't sure of {n} {}.",
                plural(n, "marker", "markers")
            )),
        ),
        _ => (format!("{course}: draft"), Some(tip.into())),
    }
}

/// The name a stored run would know the clip by: an editing copy
/// (`clip.MP4.mov`) is the original (`clip.MP4`).
pub fn run_key(path: &Path) -> String {
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
        .and_then(|l| l.drafts())
        .unwrap_or_default();
    s.catalog = Library::open_default()
        .and_then(|l| l.catalog())
        .unwrap_or_default();
    s.rides = organise::rides(&s.clips, &s.catalog.ride_names);
    s.uses = Library::open_default()
        .and_then(|l| Ok(library::videos::clip_uses(&l.videos()?, &l.usage()?)))
        .unwrap_or_default();
    let still_there = match &s.view {
        View::Ride(id) => s.rides.iter().any(|r| &r.id == id),
        View::Collection(id) => s.catalog.collections.iter().any(|c| c.id == *id),
        View::Tag(t) => s.catalog.tags.values().any(|tags| tags.contains(t)),
        _ => true,
    };
    if !still_there {
        s.view = View::All;
    }
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

/// Whether `clip` shows in `view`.
fn shows(s: &State, view: &View, clip: &Clip) -> bool {
    let key = || organise::key(clip);
    match view {
        View::Drafts => return s.drafted.contains_key(&clip.path),
        View::Picks => return s.catalog.flags.get(&key()) == Some(&Flag::Pick),
        View::Rejected => return s.catalog.flags.get(&key()) == Some(&Flag::Reject),
        View::Unused => return !s.uses.contains_key(&key()),
        View::InVideo => return s.uses.get(&key()).is_some_and(|u| u.unpublished),
        View::Released => return s.uses.get(&key()).is_some_and(|u| u.published),
        View::Ride(id) => {
            return s
                .rides
                .iter()
                .find(|r| &r.id == id)
                .is_some_and(|r| r.clips.contains(&key()));
        }
        View::Collection(id) => {
            return s
                .catalog
                .collections
                .iter()
                .find(|c| c.id == *id)
                .is_some_and(|c| c.clips.contains(&key()));
        }
        View::Tag(t) => {
            return s
                .catalog
                .tags_of(&key())
                .is_some_and(|tags| tags.contains(t));
        }
        _ => {}
    }
    let local = s.roots.on_nas(&clip.path) && clip.local.is_some();
    shown_in(
        view,
        s.runs.get(&run_key(&clip.path)),
        safe(s, &clip.path),
        local,
    )
}

/// Whether a clip with this run (or none) shows in `view`.
/// `safe`: its original is verified on the NAS. `local`: it's a NAS clip with
/// a local copy.
fn shown_in(view: &View, run: Option<&RunInfo>, safe: bool, local: bool) -> bool {
    match view {
        View::All => true,
        View::Untimed => run.is_none(),
        View::NotOnNas => !safe,
        View::Local => local,
        View::Course(id) => run.is_some_and(|r| &r.course_id == id),
        // Decided by the catalog, in `shows`.
        View::Drafts
        | View::Picks
        | View::Rejected
        | View::Unused
        | View::InVideo
        | View::Released
        | View::Ride(_)
        | View::Collection(_)
        | View::Tag(_) => false,
    }
}

/// Bytes as GB, one decimal.
fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// Asks the worker to copy the ticked NAS clips down.
fn start_hydrate(p: &Page) {
    let s = p.state.borrow();
    let clips: Vec<Clip> = s
        .clips
        .iter()
        .filter(|c| s.selected.contains(&c.path) && s.roots.on_nas(&c.path))
        .filter(|c| c.local.is_none() && c.info.is_some())
        .cloned()
        .collect();
    let roots = s.roots.clone();
    drop(s);
    if clips.is_empty() {
        return;
    }
    p.state.borrow_mut().busy = true;
    p.status.set_text("Copying from the NAS…");
    p.progress.set_visible(true);
    refresh_controls(p);
    let _ = p.tx.send(Command::Hydrate(clips, roots));
}

/// Asks before removing the ticked clips' local copies.
fn confirm_dehydrate(p: &Page) {
    let s = p.state.borrow();
    let clips: Vec<Clip> = s
        .clips
        .iter()
        .filter(|c| s.selected.contains(&c.path) && c.local.is_some())
        .cloned()
        .collect();
    let roots = s.roots.clone();
    let bytes = library::hydrate::local_bytes(&clips, &roots);
    drop(s);
    if clips.is_empty() {
        return;
    }
    let body = format!(
        "{} {}, {}. The originals stay on the NAS, and they can be copied down again any time. Rendered overlays are kept.",
        clips.len(),
        plural(clips.len(), "clip", "clips"),
        gb(bytes)
    );
    let dialog = adw::AlertDialog::new(Some("Remove local copies?"), Some(&body));
    dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("cancel");
    let p2 = p.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "remove" || p2.state.borrow().busy {
            return;
        }
        p2.state.borrow_mut().busy = true;
        refresh_controls(&p2);
        let _ = p2.tx.send(Command::Dehydrate(clips.clone(), roots.clone()));
    });
    dialog.present(Some(&p.window));
}

/// Copies each clip down from the NAS, then makes its proxy; returns what
/// happened.
fn hydrate(
    library: &Library,
    clips: &[Clip],
    roots: &Roots,
    cancel: &Cancel,
    replies: &mpsc::Sender<Reply>,
) -> String {
    let total = clips.len();
    let (mut copied, mut failed) = (0usize, Vec::new());
    for (i, clip) in clips.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let name = clip.name();
        let status = |stage: &str, f: f64| {
            let _ = replies.send(Reply::Status(
                format!("{stage} {name} ({} of {total})", i + 1),
                Some((i as f64 + f) / total as f64),
            ));
        };
        let result = library::hydrate::hydrate(clip, roots, cancel, |f| {
            status("Copying from the NAS:", f * 0.5)
        })
        .and_then(|local| {
            let info = clip.info.as_ref().expect("filtered on info");
            proxy::make(&local, info, cancel, |f| {
                status("Making the proxy of", 0.5 + f * 0.5)
            })
        });
        let mut updated = clip.clone();
        scan::place(&mut updated, roots);
        let _ = library.upsert(&updated);
        match result {
            Ok(_) => copied += 1,
            Err(_) if cancel.load(Ordering::Relaxed) => break,
            Err(e) => failed.push(format!("{name}: {e:#}")),
        }
    }
    let mut text = format!(
        "Copied {copied} {} here to edit.",
        plural(copied, "clip", "clips")
    );
    if let Some(first) = failed.first() {
        text.push_str(&format!(" {} failed: {first}", failed.len()));
    }
    if cancel.load(Ordering::Relaxed) {
        text.push_str(" Stopped.");
    }
    text
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

/// "3 clips ticked · 2 timed" for the bar over the table.
fn ticked_text(clips: usize, runs: usize) -> String {
    let timed = if runs == clips {
        if clips == 1 {
            "timed".to_owned()
        } else {
            "all timed".to_owned()
        }
    } else if runs == 0 {
        "not timed yet".to_owned()
    } else {
        format!("{runs} timed")
    };
    format!(
        "{clips} {} ticked · {timed}",
        plural(clips, "clip", "clips")
    )
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
    let count = |view: &View| s.clips.iter().filter(|c| shows(&s, view, c)).count();
    // Key, title, count; a key of "" is a section heading.
    let mut rows: Vec<(String, String, usize)> = vec![
        ("all".into(), "All clips".into(), s.clips.len()),
        (
            "untimed".into(),
            "Not timed yet".into(),
            count(&View::Untimed),
        ),
        (
            "nas".into(),
            "Not on the NAS".into(),
            count(&View::NotOnNas),
        ),
        ("local".into(), "Local copies".into(), count(&View::Local)),
        (
            "drafts".into(),
            "Drafts to check".into(),
            count(&View::Drafts),
        ),
        ("picks".into(), "Picks".into(), count(&View::Picks)),
        ("rejected".into(), "Rejected".into(), count(&View::Rejected)),
        (
            "unused".into(),
            "Not in a video".into(),
            count(&View::Unused),
        ),
        ("invideo".into(), "In a video".into(), count(&View::InVideo)),
        ("released".into(), "Released".into(), count(&View::Released)),
    ];
    let section =
        |rows: &mut Vec<(String, String, usize)>, title: &str, items: Vec<(View, String)>| {
            if items.is_empty() {
                return;
            }
            rows.push((String::new(), title.to_owned(), 0));
            for (view, name) in items {
                let n = count(&view);
                rows.push((view_key(&view), name, n));
            }
        };
    section(
        &mut rows,
        "Collections",
        s.catalog
            .collections
            .iter()
            .map(|c| (View::Collection(c.id), c.name.clone()))
            .collect(),
    );
    section(
        &mut rows,
        "Tags",
        s.catalog
            .all_tags()
            .into_iter()
            .map(|(t, _)| (View::Tag(t.clone()), t))
            .collect(),
    );
    section(
        &mut rows,
        "Courses",
        s.courses
            .iter()
            .map(|(id, name)| (View::Course(id.clone()), name.clone()))
            .collect(),
    );
    section(
        &mut rows,
        "Rides",
        s.rides
            .iter()
            .map(|r| (View::Ride(r.id.clone()), r.title()))
            .collect(),
    );
    let selected = view_key(&s.view);
    drop(s);
    p.filling.set(true);
    while let Some(row) = p.course_list.first_child() {
        p.course_list.remove(&row);
    }
    for (key, name, n) in rows {
        let row = gtk::ListBoxRow::new();
        if key.is_empty() {
            let heading = label(&name, "heading");
            heading.set_margin_top(10);
            row.set_child(Some(&heading));
            row.set_selectable(false);
            row.set_activatable(false);
            p.course_list.append(&row);
            continue;
        }
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = label(&name, "");
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&title);
        line.append(&label(&n.to_string(), "dim-label"));
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

/// The panel row's name for a view.
fn view_key(view: &View) -> String {
    match view {
        View::All => "all".to_owned(),
        View::Untimed => "untimed".to_owned(),
        View::NotOnNas => "nas".to_owned(),
        View::Local => "local".to_owned(),
        View::Drafts => "drafts".to_owned(),
        View::Picks => "picks".to_owned(),
        View::Rejected => "rejected".to_owned(),
        View::Unused => "unused".to_owned(),
        View::InVideo => "invideo".to_owned(),
        View::Released => "released".to_owned(),
        View::Course(id) => format!("course:{id}"),
        View::Ride(id) => format!("ride:{id}"),
        View::Collection(id) => format!("collection:{id}"),
        View::Tag(t) => format!("tag:{t}"),
    }
}

/// The view for a course, ride, collection or tag row (else All clips).
fn view_named(key: &str) -> View {
    let Some((kind, rest)) = key.split_once(':') else {
        return View::All;
    };
    match kind {
        "course" => View::Course(rest.to_owned()),
        "ride" => View::Ride(rest.to_owned()),
        "collection" => rest.parse().map_or(View::All, View::Collection),
        "tag" => View::Tag(rest.to_owned()),
        _ => View::All,
    }
}

/// The Organise menu's item `item` (in the order built) on the ticked clips.
fn organise_ticked(p: &Page, item: usize) {
    let s = p.state.borrow();
    let ticked: Vec<&Clip> = s
        .clips
        .iter()
        .filter(|c| s.selected.contains(&c.path))
        .collect();
    let keys: Vec<String> = ticked.iter().map(|c| organise::key(c)).collect();
    let tags: Vec<crate::organise_ui::TagState> = s
        .catalog
        .all_tags()
        .into_iter()
        .map(|(tag, _)| {
            let having = keys
                .iter()
                .filter(|k| s.catalog.tags_of(k).is_some_and(|t| t.contains(&tag)))
                .count();
            crate::organise_ui::TagState {
                tag,
                all: having == keys.len(),
                some: having > 0,
            }
        })
        .collect();
    let collections: Vec<String> = s
        .catalog
        .collections
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let view = s.view.clone();
    drop(s);
    if keys.is_empty() {
        return;
    }
    let n = keys.len();
    let what = format!("{n} {}", plural(n, "clip", "clips"));
    let apply = {
        let p = p.clone();
        move |result: anyhow::Result<String>| {
            match result {
                Ok(text) => p.status.set_text(&text),
                Err(e) => p.status.set_text(&format!("Not saved: {e:#}")),
            }
            load_runs(&p);
            fill_courses(&p);
            refill(&p);
        }
    };
    match item {
        0..=2 => {
            let (flag, text) = match item {
                0 => (Some(Flag::Pick), format!("Picked {what}.")),
                1 => (Some(Flag::Reject), format!("Rejected {what}.")),
                _ => (None, format!("Cleared the flag on {what}.")),
            };
            apply(
                Library::open_default()
                    .and_then(|l| l.set_flag(&keys, flag))
                    .map(|()| text),
            );
        }
        3 => crate::organise_ui::tags_dialog(&p.shared, n, tags, move |add, remove| {
            let result = Library::open_default().and_then(|l| {
                for t in &add {
                    l.add_tag(&keys, t)?;
                }
                for t in &remove {
                    l.remove_tag(&keys, t)?;
                }
                Ok(format!("Tags saved for {what}."))
            });
            apply(result);
        }),
        4 => crate::organise_ui::collection_dialog(&p.shared, collections, move |name| {
            let result = Library::open_default().and_then(|l| {
                let id = l.collection(&name)?;
                l.add_to_collection(id, &keys)?;
                Ok(format!("Added {what} to {name}."))
            });
            apply(result);
        }),
        5 => add_to_video(p, ticked_items(p)),
        _ => {
            if let View::Collection(id) = view {
                apply(
                    Library::open_default()
                        .and_then(|l| l.remove_from_collection(id, &keys))
                        .map(|()| format!("Took {what} out of the collection.")),
                );
            }
        }
    }
}

/// The ticked clips as video items, in the order recorded: whole, with the
/// timer on for a committed run.
fn ticked_items(p: &Page) -> Vec<library::videos::Item> {
    let runs = crate::videos_page::runs(&p.shared);
    let s = p.state.borrow();
    let mut ticked: Vec<&Clip> = s
        .clips
        .iter()
        .filter(|c| s.selected.contains(&c.path))
        .collect();
    ticked.sort_by(|a, b| (&a.recorded, &a.path).cmp(&(&b.recorded, &b.path)));
    ticked
        .into_iter()
        .map(|c| library::videos::Item {
            timer: runs.get(&run_key(&c.path)).map(|r| r.id.clone()),
            ..library::videos::Item::whole(c)
        })
        .collect()
}

/// Asks which video `items` go in (a new one, or one not published yet) and
/// adds them; a new video opens on the Videos page.
fn add_to_video(p: &Page, items: Vec<library::videos::Item>) {
    let videos = match Library::open_default().and_then(|l| l.videos()) {
        Ok(v) => v,
        Err(e) => {
            p.status
                .set_text(&format!("The videos couldn't be read: {e:#}"));
            return;
        }
    };
    let open: Vec<(i64, String)> = videos
        .into_iter()
        .filter(|v| v.status != library::videos::Status::Published)
        .map(|v| (v.id, v.title))
        .collect();
    let p2 = p.clone();
    crate::organise_ui::video_dialog(&p.shared, items.len(), open, move |id, title| {
        let result = Library::open_default().and_then(|l| {
            let id = match id {
                Some(id) => id,
                None => l.new_video(&title, "clips")?,
            };
            Ok((id, l.add_items(id, &items)?))
        });
        match result {
            Ok((video, added)) => {
                let skipped = items.len() - added;
                p2.status.set_text(&format!(
                    "Added {added} {} to {title}{}.",
                    plural(added, "clip", "clips"),
                    if skipped > 0 {
                        format!(" ({skipped} already in it)")
                    } else {
                        String::new()
                    }
                ));
                p2.shared.videos_changed();
                if id.is_none() {
                    p2.shared.show_video(video);
                }
            }
            Err(e) => p2.status.set_text(&format!("Not added: {e:#}")),
        }
    });
}

/// Renames the course, ride or collection shown.
fn rename_selected(p: &Page) {
    let s = p.state.borrow();
    let view = s.view.clone();
    match view {
        View::Course(id) => {
            let Some((id, name)) = s.courses.iter().find(|(c, _)| *c == id).cloned() else {
                return;
            };
            drop(s);
            let p2 = p.clone();
            crate::courses::rename_course(&p.shared, &id, &name, move |result| {
                p2.status.set_text(&result.unwrap_or_else(|e| e));
            });
        }
        View::Ride(id) => {
            let Some(ride) = s.rides.iter().find(|r| r.id == id).cloned() else {
                return;
            };
            drop(s);
            let p2 = p.clone();
            crate::organise_ui::name_dialog(
                &p.shared,
                "Name this ride",
                &format!(
                    "{} clips from {}. Leave it empty to go back to the date. Only the catalog changes: no files move.",
                    ride.clips.len(),
                    ride.start.format("%a %-d %b %Y, %H:%M")
                ),
                &ride.name.clone().unwrap_or_default(),
                None,
                move |name| {
                    let result = Library::open_default().and_then(|l| l.name_ride(&ride, &name));
                    if let Err(e) = result {
                        p2.status.set_text(&format!("Not saved: {e:#}"));
                    }
                    load_runs(&p2);
                    fill_courses(&p2);
                    refill(&p2);
                },
            );
        }
        View::Collection(id) => {
            let Some(name) = s
                .catalog
                .collections
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone())
            else {
                return;
            };
            drop(s);
            let (p2, p3) = (p.clone(), p.clone());
            crate::organise_ui::name_dialog(
                &p.shared,
                "Rename collection",
                "Delete takes the collection away; its clips stay in the Library.",
                &name,
                Some(Box::new(move || {
                    if let Err(e) = Library::open_default().and_then(|l| l.delete_collection(id)) {
                        p3.status.set_text(&format!("Not deleted: {e:#}"));
                    }
                    load_runs(&p3);
                    fill_courses(&p3);
                    refill(&p3);
                })),
                move |name| {
                    if let Err(e) =
                        Library::open_default().and_then(|l| l.rename_collection(id, &name))
                    {
                        p2.status.set_text(&format!("Not renamed: {e:#}"));
                    }
                    load_runs(&p2);
                    fill_courses(&p2);
                    refill(&p2);
                },
            );
        }
        _ => {}
    }
}

/// Makes thumbnails on a thread of its own, the most recently asked first
/// (the rows on screen now), and sends each back as [`Reply::Thumb`].
fn thumbs_worker(replies: mpsc::Sender<Reply>) -> mpsc::Sender<(PathBuf, PathBuf)> {
    let (tx, rx) = mpsc::channel::<(PathBuf, PathBuf)>();
    std::thread::spawn(move || {
        let cache = library::thumbs::cache_dir();
        let mut wanted: Vec<(PathBuf, PathBuf)> = Vec::new();
        loop {
            if wanted.is_empty() {
                match rx.recv() {
                    Ok(job) => wanted.push(job),
                    Err(_) => return,
                }
            }
            wanted.extend(rx.try_iter());
            let Some((clip, source)) = wanted.pop() else {
                continue;
            };
            if let Ok(png) = library::thumbs::thumbnail(&source, &cache)
                && replies.send(Reply::Thumb(clip, png)).is_err()
            {
                return;
            }
        }
    });
    tx
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

fn safe(s: &State, clip: &Path) -> bool {
    s.roots.on_nas(clip) || s.backups.get(clip).is_some_and(Backup::is_safe)
}

/// The Stored column for a clip whose original is on the NAS.
fn nas_text(s: &State, clip: &Clip) -> (String, Option<String>) {
    let mut tip = format!("Original on the NAS: {}", clip.path.display());
    if !s.roots.nas_online {
        tip.push_str("\nThe NAS isn't mounted now: shown as last catalogued.");
    }
    if clip.preview.is_none() {
        tip.push_str("\nNo preview yet: it's made after the next scan.");
    }
    match &clip.local {
        Some(local) => (
            "NAS · copy here".into(),
            Some(format!("{tip}\nLocal copy to edit: {}", local.display())),
        ),
        None => ("NAS".into(), Some(tip)),
    }
}

/// The NAS column: whether the clip's original is verified there.
fn backup_text(backup: Option<&Backup>) -> (String, Option<String>) {
    match backup {
        None => (String::new(), None),
        Some(Backup::LocalOnly) => (
            "Local only".into(),
            Some("Not imported through Ghost Run: no verified copy on the NAS".into()),
        ),
        Some(Backup::OnNas {
            original,
            verified_at,
        }) => (
            "Backed up".into(),
            Some(format!(
                "Original verified on the NAS {}: {}",
                short_time(verified_at),
                original.display()
            )),
        ),
        Some(Backup::IsOriginal { verified_at }) => (
            "Original".into(),
            Some(format!(
                "This is the original, verified on the NAS {}",
                short_time(verified_at)
            )),
        ),
        Some(Backup::Missing { original }) => (
            "Missing".into(),
            Some(format!(
                "Verified once, but not there now (moved, deleted, or the share isn't mounted): {}",
                original.display()
            )),
        ),
    }
}

/// `2026-08-26T20:00:00.123Z` → `2026-08-26 20:00`.
fn short_time(stamp: &str) -> String {
    let t = stamp.replace('T', " ");
    t.get(..16).unwrap_or(&t).to_owned()
}

/// The NAS footage root and the local root, from Import's settings; the NAS
/// counts as online only while it's mounted as it was set up.
pub fn roots() -> Roots {
    Roots::from_import(&ingest::settings::Settings::load().unwrap_or_default())
}

/// The folders a scan covers: the footage folders, plus Import's editing
/// destination, so imported clips show up without adding it by hand, and the
/// NAS footage root while it's mounted.
fn scan_folders(shared: &Shared, roots: &Roots) -> Vec<PathBuf> {
    let mut folders = shared.settings().footage_folders.clone();
    let nas = roots.nas.as_ref().filter(|_| roots.nas_online);
    for root in [roots.local.as_ref(), nas].into_iter().flatten() {
        if !folders.iter().any(|f| root.starts_with(f)) {
            folders.push(root.clone());
        }
    }
    folders
}

/// When the import journal last changed (its database or write-ahead log).
fn journal_changed() -> Option<std::time::SystemTime> {
    let dir = ingest::settings::data_dir();
    ["jobs.sqlite3", "jobs.sqlite3-wal"]
        .iter()
        .filter_map(|name| {
            std::fs::metadata(dir.join(name))
                .and_then(|m| m.modified())
                .ok()
        })
        .max()
}

/// Each clip's backup state, from the import journal (read-only) and a stat
/// of each archived original.
fn backups(clips: &[Clip]) -> HashMap<PathBuf, Backup> {
    let archived: Vec<library::backup::Archived> =
        ingest::jobs::archived(&ingest::settings::data_dir())
            .unwrap_or_default()
            .into_iter()
            .map(|a| library::backup::Archived {
                original: a.original,
                editing: a.editing,
                verified_at: a.verified_at,
            })
            .collect();
    library::backup::states(clips.iter().map(|c| c.path.as_path()), &archived, |p| {
        p.exists()
    })
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

        assert!(shown_in(&View::All, None, false, false));
        assert!(shown_in(&View::Untimed, None, false, false));
        assert!(!shown_in(&View::Untimed, Some(a), false, false));
        assert!(shown_in(&View::Course("lw".into()), Some(a), false, false));
        assert!(!shown_in(
            &View::Course("other".into()),
            Some(a),
            false,
            false
        ));
        assert!(!shown_in(&View::Course("lw".into()), None, false, false));
        assert!(shown_in(&View::NotOnNas, Some(a), false, false));
        assert!(!shown_in(&View::NotOnNas, None, true, false));
        assert!(shown_in(&View::Local, None, true, true));
        assert!(!shown_in(&View::Local, None, true, false));
    }

    #[test]
    fn the_ticked_bar_says_how_many_are_timed() {
        assert_eq!(ticked_text(1, 1), "1 clip ticked · timed");
        assert_eq!(ticked_text(3, 3), "3 clips ticked · all timed");
        assert_eq!(ticked_text(3, 2), "3 clips ticked · 2 timed");
        assert_eq!(ticked_text(2, 0), "2 clips ticked · not timed yet");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(65.4), "1:05");
        assert_eq!(duration(3725.0), "1:02:05");
    }
}
