//! The Time page: step through a clip frame by frame, place Start, the sector
//! splits and Finish, watch the timing against the course's best as you go,
//! and commit it as a run in the run database.
//!
//! Frames come from the clip's proxy when it has one (every frame a key
//! frame, so stepping is quick), else from the original. A decoder thread
//! always works on the newest frame asked for and skips the rest, so
//! scrubbing never queues up. Markers are kept in the library as a draft
//! until committed, so closing the app loses nothing.
//!
//! It opens over the Library (double-click a clip, or Time it), with a back
//! arrow to return.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, glib};
use library::auto::{self, Placement, Reference};
use library::frames::{Frame, read_frame};
use library::{Clip, Draft, Library, MediaInfo, ProxyState};
use timer_core::TimerDatabase;
use timer_core::commit::{self, ClipInfo};
use timer_core::file::{DatabaseFile, SaveError};
use timer_core::models::{Course, MarkerFrames};
use timer_core::preview::{Preview, keep_for_course, preview};
use timer_core::timing::{format_delta, format_duration};

use crate::shared::{Change, Shared};

/// Frames are decoded at most this tall: plenty to place a marker by.
const VIEW_HEIGHT: u32 = 720;
/// Decoded frames kept, so stepping back and forth is instant.
const CACHE: usize = 60;

struct Request {
    path: PathBuf,
    info: MediaInfo,
    index: i64,
}

/// A decoded frame (or why it couldn't be), and which file it is from.
type Decoded = (PathBuf, i64, Result<Frame, String>);

/// What an auto-markers job found, for the clip at the path.
enum Auto {
    /// Placed from the chosen course's reference.
    Placed(PathBuf, Result<Placement, String>),
    /// Placed against every course's reference, best first.
    Matched(PathBuf, Result<Vec<Placement>, String>),
}

/// News of a preview being fetched or made before a clip opens.
enum Fetch {
    Progress(PathBuf, f64),
    /// The clip, with its preview if one was had.
    Ready(Box<Clip>, Option<PathBuf>),
}

/// The clip being timed.
struct Open {
    clip: Clip,
    /// The original's, for timing.
    info: MediaInfo,
    /// What frames are read from: the proxy if there is one, else the original.
    view: PathBuf,
    view_info: MediaInfo,
    frames: i64,
}

struct State {
    open: Option<Open>,
    position: i64,
    db: Option<TimerDatabase>,
    course: Option<String>,
    markers: MarkerFrames,
    confidence: BTreeMap<String, f64>,
    /// `hand`, or `auto` while every marker is still the auto-markers'.
    source: String,
    /// The clip's stored run, if it has one.
    run_id: Option<String>,
    /// The markers differ from the stored run (or there is none yet).
    unsaved: bool,
    /// The chosen course's reference for auto-markers.
    reference: Option<Reference>,
    /// An auto-markers job is running.
    auto_busy: bool,
    /// The clip waiting for its preview before it opens.
    waiting: Option<PathBuf>,
}

#[derive(Clone)]
struct Page {
    shared: Shared,
    root: gtk::Box,
    state: Rc<RefCell<State>>,
    requests: mpsc::Sender<Request>,
    title: adw::WindowTitle,
    views: gtk::Stack,
    picture: gtk::Picture,
    frame_error: gtk::Label,
    scale: gtk::Scale,
    /// Set while the scale is moved to follow the position, not by hand.
    following: Rc<Cell<bool>>,
    position: gtk::Label,
    set_next: gtk::Button,
    banner: gtk::Box,
    banner_text: gtk::Label,
    courses: gtk::DropDown,
    course_ids: Rc<RefCell<Vec<String>>>,
    filling: Rc<Cell<bool>>,
    markers: gtk::ListBox,
    timing: gtk::Grid,
    verdict: gtk::Label,
    commit: gtk::Button,
    discard: gtk::Button,
    status: gtk::Label,
    reference: gtk::Label,
    auto_place: gtk::Button,
    find_course: gtk::Button,
    use_reference: gtk::Button,
    spinner: gtk::Spinner,
    auto: mpsc::Sender<Auto>,
    previews: mpsc::Sender<Fetch>,
}

pub fn page(shared: &Shared) -> gtk::Widget {
    let (requests, decoded) = decoder();
    let (auto, auto_done) = mpsc::channel::<Auto>();
    let (previews, preview_news) = mpsc::channel::<Fetch>();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Time a run", "");
    header.set_title_widget(Some(&title));
    let keys = gtk::MenuButton::new();
    keys.set_icon_name("input-keyboard-symbolic");
    keys.set_tooltip_text(Some("Keyboard shortcuts"));
    keys.set_popover(Some(&shortcuts_popover()));
    header.pack_end(&keys);
    root.append(&header);

    let views = gtk::Stack::new();
    views.set_vexpand(true);
    let empty = adw::StatusPage::new();
    empty.set_icon_name(Some("alarm-symbolic"));
    empty.set_title("No clip open");
    empty.set_description(Some(
        "Go back to the Library and double-click a clip (or tick one and press Time it).",
    ));
    views.add_named(&empty, Some("empty"));

    let editor = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    editor.add_css_class("content");

    // The viewer: the frame, a scrub bar and the transport buttons.
    let viewer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    viewer.set_hexpand(true);
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_vexpand(true);
    picture.set_hexpand(true);
    picture.add_css_class("frame-view");
    let frame_error = label("", "error");
    frame_error.set_wrap(true);
    frame_error.set_visible(false);
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
    scale.set_draw_value(false);
    scale.update_property(&[gtk::accessible::Property::Label("Position in the clip")]);
    let transport = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    transport.set_halign(gtk::Align::Center);
    let position = label("", "numeric");
    position.set_width_chars(30);
    position.set_xalign(0.5);
    let steps: [(&str, &str, Step); 8] = [
        ("⏮", "First frame (Home)", Step::First),
        ("−1 s", "Back one second (Page Up)", Step::By(-1, true)),
        ("−10", "Back ten frames (Shift+Left)", Step::By(-10, false)),
        ("−1", "Back one frame (Left)", Step::By(-1, false)),
        ("+1", "Forward one frame (Right)", Step::By(1, false)),
        (
            "+10",
            "Forward ten frames (Shift+Right)",
            Step::By(10, false),
        ),
        ("+1 s", "Forward one second (Page Down)", Step::By(1, true)),
        ("⏭", "Last frame (End)", Step::Last),
    ];
    let mut step_buttons = Vec::new();
    for (i, (text, tip, step)) in steps.into_iter().enumerate() {
        if i == 4 {
            transport.append(&position);
        }
        let b = gtk::Button::with_label(text);
        b.set_tooltip_text(Some(tip));
        b.add_css_class("flat");
        transport.append(&b);
        step_buttons.push((b, step));
    }
    let set_next = button("Set Start here");
    set_next.add_css_class("suggested-action");
    set_next.set_halign(gtk::Align::Center);
    set_next.set_tooltip_text(Some("Put the next marker on this frame (M)"));
    viewer.append(&picture);
    viewer.append(&frame_error);
    viewer.append(&scale);
    viewer.append(&transport);
    viewer.append(&set_next);
    editor.append(&viewer);

    // The side: course, markers, timing, commit.
    let side = gtk::Box::new(gtk::Orientation::Vertical, 12);
    side.set_width_request(330);
    let banner = gtk::Box::new(gtk::Orientation::Vertical, 8);
    banner.add_css_class("banner");
    let banner_text = label("", "");
    banner_text.set_wrap(true);
    let banner_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let choose = button("Choose database…");
    let create = button("New database…");
    banner_buttons.append(&choose);
    banner_buttons.append(&create);
    banner.append(&banner_text);
    banner.append(&banner_buttons);
    side.append(&banner);
    side.append(&label("Course", "heading"));
    let course_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let courses = gtk::DropDown::from_strings(&[]);
    courses.set_hexpand(true);
    courses.update_property(&[gtk::accessible::Property::Label("Course")]);
    let add_course = gtk::Button::from_icon_name("list-add-symbolic");
    add_course.set_tooltip_text(Some("New course…"));
    add_course.update_property(&[gtk::accessible::Property::Label("New course")]);
    add_course.set_valign(gtk::Align::Center);
    course_row.append(&courses);
    course_row.append(&add_course);
    side.append(&course_row);
    let auto_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let auto_place = button("Auto-place");
    auto_place.set_tooltip_text(Some(
        "Place the markers by matching this clip's footage with the course's reference run",
    ));
    let find_course = button("Find the course");
    find_course.set_tooltip_text(Some(
        "Match this clip against every course's reference run and use the best",
    ));
    let spinner = gtk::Spinner::new();
    auto_row.append(&auto_place);
    auto_row.append(&find_course);
    auto_row.append(&spinner);
    side.append(&auto_row);
    let reference = label("", "dim-label");
    reference.set_wrap(true);
    side.append(&reference);
    let use_reference = button("Use this run as the reference");
    use_reference.set_tooltip_text(Some(
        "Place other clips' markers on this course from this run",
    ));
    use_reference.set_halign(gtk::Align::Start);
    side.append(&use_reference);
    side.append(&label("Markers", "heading"));
    let markers = gtk::ListBox::new();
    markers.add_css_class("boxed-list");
    markers.set_selection_mode(gtk::SelectionMode::None);
    side.append(&markers);
    side.append(&label("Timing", "heading"));
    let timing = gtk::Grid::new();
    timing.set_column_spacing(14);
    timing.set_row_spacing(4);
    side.append(&timing);
    let verdict = label("", "");
    verdict.set_wrap(true);
    side.append(&verdict);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let commit = button("Commit run");
    commit.add_css_class("suggested-action");
    commit.set_hexpand(true);
    let discard = button("Discard changes");
    discard.set_tooltip_text(Some(
        "Forget the markers placed since the run was last committed",
    ));
    actions.append(&discard);
    actions.append(&commit);
    side.append(&actions);
    let side_scroll = gtk::ScrolledWindow::builder()
        .child(&side)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .build();
    editor.append(&side_scroll);
    views.add_named(&editor, Some("editor"));
    root.append(&views);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    footer.add_css_class("footer");
    let status = label("", "");
    status.set_wrap(true);
    status.set_hexpand(true);
    footer.append(&status);
    root.append(&footer);

    let page = Page {
        shared: shared.clone(),
        root: root.clone(),
        state: Rc::new(RefCell::new(State {
            open: None,
            position: 0,
            db: None,
            course: None,
            markers: MarkerFrames::new(),
            confidence: BTreeMap::new(),
            source: "hand".into(),
            run_id: None,
            unsaved: false,
            reference: None,
            auto_busy: false,
            waiting: None,
        })),
        requests,
        title,
        views,
        picture,
        frame_error,
        scale: scale.clone(),
        following: Rc::default(),
        position,
        set_next: set_next.clone(),
        banner,
        banner_text,
        courses: courses.clone(),
        course_ids: Rc::default(),
        filling: Rc::default(),
        markers,
        timing,
        verdict,
        commit: commit.clone(),
        discard: discard.clone(),
        status,
        reference,
        auto_place: auto_place.clone(),
        find_course: find_course.clone(),
        use_reference: use_reference.clone(),
        spinner,
        auto,
        previews,
    };

    for (b, step) in step_buttons {
        let p = page.clone();
        b.connect_clicked(move |_| go(&p, step));
    }
    let p = page.clone();
    scale.connect_value_changed(move |s| {
        if !p.following.get() {
            seek(&p, s.value().round() as i64);
        }
    });
    let p = page.clone();
    set_next.connect_clicked(move |_| set_next_marker(&p));
    let p = page.clone();
    add_course.connect_clicked(move |_| {
        let p2 = p.clone();
        crate::courses::new_course(&p.shared, move |result| match result {
            Ok((id, text)) => {
                change_course(&p2, id);
                p2.status.set_text(&text);
            }
            Err(e) => p2.status.set_text(&e),
        });
    });
    let p = page.clone();
    choose.connect_clicked(move |_| crate::settings_dialog::choose_database(&p.shared, false));
    let p = page.clone();
    create.connect_clicked(move |_| crate::settings_dialog::choose_database(&p.shared, true));
    let p = page.clone();
    courses.connect_selected_notify(move |d| {
        if p.filling.get() {
            return;
        }
        let id = p.course_ids.borrow().get(d.selected() as usize).cloned();
        if let Some(id) = id {
            change_course(&p, id);
        }
    });
    let p = page.clone();
    commit.connect_clicked(move |_| commit_run(&p));
    let p = page.clone();
    discard.connect_clicked(move |_| discard_changes(&p));
    let p = page.clone();
    auto_place.connect_clicked(move |_| start_auto(&p, false));
    let p = page.clone();
    find_course.connect_clicked(move |_| start_auto(&p, true));
    let p = page.clone();
    use_reference.connect_clicked(move |_| use_as_reference(&p));
    keyboard(&page);

    let p = page.clone();
    shared.subscribe(move |change| match change {
        Change::TimeClip => {
            if let Some(path) = p.shared.take_clip_to_time() {
                open_path(&p, &path);
            }
        }
        Change::Settings | Change::Runs => {
            load_database(&p);
            show(&p);
        }
        Change::Render
        | Change::Resolve
        | Change::ShowLibrary
        | Change::Imported
        | Change::Videos
        | Change::BuildVideo
        | Change::ShowVideo
        | Change::Strava
        | Change::Update => {}
    });

    let p = page.clone();
    glib::timeout_add_local(Duration::from_millis(30), move || {
        let newest = decoded.try_iter().last();
        if let Some((path, index, frame)) = newest {
            shown(&p, &path, index, frame);
        }
        for result in auto_done.try_iter() {
            auto_finished(&p, result);
        }
        for news in preview_news.try_iter() {
            preview_news_came(&p, news);
        }
        glib::ControlFlow::Continue
    });

    load_database(&page);
    show(&page);
    root.upcast()
}

#[derive(Debug, Clone, Copy)]
enum Step {
    First,
    Last,
    /// Frames, or seconds if `true`.
    By(i64, bool),
}

/// The decoder thread: decodes the newest frame asked for, from a small cache
/// when it can.
fn decoder() -> (mpsc::Sender<Request>, mpsc::Receiver<Decoded>) {
    let (tx, requests) = mpsc::channel::<Request>();
    let (done, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut cache: VecDeque<((PathBuf, i64), Frame)> = VecDeque::new();
        while let Ok(mut request) = requests.recv() {
            while let Ok(newer) = requests.try_recv() {
                request = newer;
            }
            let key = (request.path.clone(), request.index);
            let frame = match cache.iter().find(|(k, _)| *k == key) {
                Some((_, f)) => Ok(f.clone()),
                None => {
                    match read_frame(&request.path, &request.info, request.index, VIEW_HEIGHT) {
                        Ok(f) => {
                            cache.push_back((key, f.clone()));
                            if cache.len() > CACHE {
                                cache.pop_front();
                            }
                            Ok(f)
                        }
                        Err(e) => Err(format!("{e:#}")),
                    }
                }
            };
            if done.send((request.path, request.index, frame)).is_err() {
                break;
            }
        }
    });
    (tx, rx)
}

fn shown(p: &Page, path: &Path, index: i64, frame: Result<Frame, String>) {
    let s = p.state.borrow();
    let Some(open) = &s.open else { return };
    if open.view != path {
        return;
    }
    match frame {
        Ok(frame) => {
            let bytes = glib::Bytes::from_owned(frame.rgba);
            let texture = gdk::MemoryTexture::new(
                frame.width as i32,
                frame.height as i32,
                gdk::MemoryFormat::R8g8b8a8,
                &bytes,
                frame.width as usize * 4,
            );
            p.picture.set_paintable(Some(&texture));
            p.frame_error.set_visible(false);
        }
        Err(e) if index == s.position => {
            p.frame_error
                .set_text(&format!("Frame {index} couldn't be read: {e}"));
            p.frame_error.set_visible(true);
        }
        Err(_) => {}
    }
}

/// Opens a clip by path: from the Library if it's there, else probed now.
fn open_path(p: &Page, path: &Path) {
    let known = Library::open_default()
        .and_then(|l| l.get(path))
        .ok()
        .flatten();
    let clip = match known {
        Some(clip) if clip.info.is_some() => clip,
        _ => match library::probe::probe(path) {
            Ok(info) => {
                let proxy = library::proxy::proxy_path(path);
                Clip {
                    path: path.to_owned(),
                    size: 0,
                    modified_ns: 0,
                    recorded: None,
                    recorded_source: String::new(),
                    proxy: if library::proxy::verify(path, &info, &proxy).is_ok() {
                        ProxyState::Ready(proxy)
                    } else {
                        ProxyState::Missing
                    },
                    info: Some(info),
                    probe_error: None,
                    ..Clip::default()
                }
            }
            Err(e) => {
                p.status
                    .set_text(&format!("{} can't be read: {e:#}", path.display()));
                return;
            }
        },
    };
    open_clip(p, clip);
}

/// Opens a clip; a NAS clip with neither a proxy nor a preview on this
/// machine gets its preview first (from the NAS, else made now), so it reads
/// a small local file rather than the full-size original over the network.
fn open_clip(p: &Page, clip: Clip) {
    let roots = crate::library_page::roots();
    let wanted = roots.on_nas(&clip.path)
        && !matches!(clip.proxy, ProxyState::Ready(_))
        && !library::preview::here(&roots, &clip.path, clip.size)
        && clip.info.is_some()
        && (roots.nas_online || clip.local.is_some());
    if !wanted {
        p.state.borrow_mut().waiting = None;
        open_now(p, clip);
        return;
    }
    p.state.borrow_mut().waiting = Some(clip.path.clone());
    p.status
        .set_text(&format!("Getting the preview of {}…", clip.name()));
    let tx = p.previews.clone();
    std::thread::spawn(move || {
        let path = clip.path.clone();
        let preview =
            crate::library_page::needed_preview(&clip, &roots, &library::Cancel::default(), |f| {
                let _ = tx.send(Fetch::Progress(path.clone(), f));
            });
        if let Some(preview) = &preview {
            let _ =
                Library::open_default().and_then(|l| l.set_preview(&path, Some(preview.clone())));
        }
        let _ = tx.send(Fetch::Ready(Box::new(clip), preview));
    });
}

fn preview_news_came(p: &Page, news: Fetch) {
    let waiting = p.state.borrow().waiting.clone();
    match news {
        Fetch::Progress(path, f) if waiting.as_ref() == Some(&path) => {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            p.status.set_text(&format!(
                "Making the preview of {name} (it's kept on the NAS for next time): {:.0}%",
                f * 100.0
            ));
        }
        Fetch::Ready(mut clip, preview) if waiting.as_ref() == Some(&clip.path) => {
            p.state.borrow_mut().waiting = None;
            let note = if preview.is_some() {
                String::new()
            } else {
                format!(
                    "No preview could be had for {}: reading the original, which is slower.",
                    clip.name()
                )
            };
            if preview.is_some() {
                clip.preview = preview;
            }
            p.status.set_text(&note);
            open_now(p, *clip);
        }
        _ => {}
    }
}

fn open_now(p: &Page, clip: Clip) {
    let Some(info) = clip.info.clone() else {
        return;
    };
    // The proxy, else the catalog's preview, else the local copy, else the clip.
    let source = clip.frames_from();
    let (view, view_info) = if source == clip.path {
        (clip.path.clone(), info.clone())
    } else {
        match library::probe::probe(&source) {
            Ok(i) => (source, i),
            Err(_) => (clip.path.clone(), info.clone()),
        }
    };
    let frames = info
        .frames
        .or_else(|| {
            info.duration_seconds
                .map(|d| (d * info.fps()).floor() as i64)
        })
        .unwrap_or(1)
        .max(1);
    let using_proxy = view != clip.path;
    let reading_proxy = matches!(&clip.proxy, ProxyState::Ready(p) if *p == view);
    let reading_preview = clip.preview.as_ref() == Some(&view);
    let fps = info.fps();
    p.title.set_subtitle(&clip.name());
    p.picture.set_paintable(gdk::Paintable::NONE);

    let draft = Library::open_default()
        .and_then(|l| l.draft(&clip.path))
        .ok()
        .flatten();
    let clip_info = ClipInfo::for_file(&clip.path, fps);
    let mut s = p.state.borrow_mut();
    let run =
        s.db.as_ref()
            .and_then(|db| commit::existing_run(db, &clip_info))
            .cloned();
    s.run_id = run.as_ref().map(|r| r.id.clone());
    match (&draft, &run) {
        (Some(d), _) => {
            s.course = Some(d.course_id.clone());
            s.markers = d.frames.clone();
            s.confidence = d.confidence.clone();
            s.source = d.source.clone();
            s.unsaved = true;
        }
        (None, Some(r)) => {
            s.course = Some(r.course_id.clone());
            s.markers = r.marker_frames.clone();
            s.confidence.clear();
            s.source = "hand".into();
            s.unsaved = false;
        }
        // A new clip: keep the course chosen last.
        (None, None) => {
            s.markers.clear();
            s.confidence.clear();
            s.source = "hand".into();
            s.unsaved = false;
        }
    }
    let start = s
        .markers
        .get("Start")
        .copied()
        .unwrap_or(0)
        .clamp(0, frames - 1);
    s.open = Some(Open {
        clip,
        info,
        view,
        view_info,
        frames,
    });
    s.position = -1;
    drop(s);
    find_reference(p);

    p.following.set(true);
    p.scale.set_range(0.0, (frames - 1).max(1) as f64);
    p.scale.set_increments(1.0, fps.round().max(1.0));
    p.following.set(false);
    p.views.set_visible_child_name("editor");
    p.status.set_text(&match (&draft, &run) {
        (Some(_), _) => "Carrying on with the markers you placed last time (not committed yet).".to_owned(),
        (None, Some(r)) => format!("This clip is {}: its markers are loaded.", r.id),
        (None, None) if reading_proxy => {
            "Reading frames from the proxy.".to_owned()
        }
        (None, None) if reading_preview => {
            "Reading frames from the catalog's preview (the original is on the NAS).".to_owned()
        }
        (None, None) if using_proxy => "Reading frames from the local copy.".to_owned(),
        (None, None) => "No proxy yet: frames come from the original, which is slower. Make one in the Library for quick stepping.".to_owned(),
    });
    show(p);
    seek(p, start);
    p.set_next.grab_focus();
}

fn load_database(p: &Page) {
    let path = p.shared.settings().run_database.clone();
    let mut s = p.state.borrow_mut();
    let (db, problem) = match path {
        None => (
            None,
            Some(
                "Choose your run database (timer_db.yaml) to pick a course and commit runs."
                    .to_owned(),
            ),
        ),
        Some(path) => match TimerDatabase::load(&path) {
            Ok(db) => (Some(db), None),
            Err(e) => (
                None,
                Some(format!("The run database couldn't be read: {e}")),
            ),
        },
    };
    p.banner.set_visible(problem.is_some());
    p.banner_text.set_text(problem.as_deref().unwrap_or(""));
    s.db = db;
    let ids: Vec<String> =
        s.db.as_ref()
            .map(|db| db.courses.iter().map(|c| c.id.clone()).collect())
            .unwrap_or_default();
    if !s.course.as_ref().is_some_and(|c| ids.contains(c)) {
        s.course = s.db.as_ref().and_then(|db| {
            db.runs
                .iter()
                .filter(|r| ids.contains(&r.course_id))
                .max_by_key(|r| timer_core::matching::run_order_time(r))
                .map(|r| r.course_id.clone())
                .or_else(|| ids.first().cloned())
        });
    }
    // The clip's run may have been committed, moved or deleted elsewhere.
    if let Some(open) = &s.open {
        let clip_info = ClipInfo::for_file(&open.clip.path, open.info.fps());
        let run_id =
            s.db.as_ref()
                .and_then(|db| commit::existing_run(db, &clip_info))
                .map(|r| r.id.clone());
        s.run_id = run_id;
    }
    drop(s);
    find_reference(p);
}

/// Looks up the chosen course's reference run.
fn find_reference(p: &Page) {
    let mut s = p.state.borrow_mut();
    let found = match (course(&s), &s.db) {
        (Some(course), Some(db)) => Library::open_default()
            .and_then(|l| {
                let clips = l.clips()?;
                auto::reference(db, course, &l, &clips)
            })
            .unwrap_or_else(|e| {
                eprintln!("ghost-run: no reference: {e:#}");
                None
            }),
        _ => None,
    };
    s.reference = found;
}

fn course(s: &State) -> Option<&Course> {
    let id = s.course.as_ref()?;
    s.db.as_ref()?.courses.iter().find(|c| &c.id == id)
}

fn current_preview(s: &State) -> Option<Preview> {
    let course = course(s)?;
    let db = s.db.as_ref()?;
    let fps = s.open.as_ref()?.info.fps();
    Some(preview(
        course,
        &db.runs,
        &s.markers,
        fps,
        s.run_id.as_deref(),
    ))
}

/// Fills the side panel and buttons from the state.
fn show(p: &Page) {
    let s = p.state.borrow();
    if s.open.is_none() {
        p.views.set_visible_child_name("empty");
        return;
    }

    // Courses.
    let (names, ids): (Vec<String>, Vec<String>) =
        s.db.as_ref()
            .map(|db| {
                db.courses
                    .iter()
                    .map(|c| {
                        (
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
                            c.id.clone(),
                        )
                    })
                    .unzip()
            })
            .unwrap_or_default();
    p.filling.set(true);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    p.courses.set_model(Some(&gtk::StringList::new(&names)));
    if let Some(i) = s
        .course
        .as_ref()
        .and_then(|c| ids.iter().position(|x| x == c))
    {
        p.courses.set_selected(i as u32);
    }
    p.courses.set_sensitive(!ids.is_empty());
    p.filling.set(false);
    *p.course_ids.borrow_mut() = ids;

    // Markers.
    while let Some(row) = p.markers.first_child() {
        p.markers.remove(&row);
    }
    let preview = current_preview(&s);
    let fps = s.open.as_ref().map_or(30.0, |o| o.info.fps());
    if let Some(pv) = &preview {
        for name in &pv.names {
            p.markers.append(&marker_row(p, &s, name, fps));
        }
    }

    // Timing.
    while let Some(child) = p.timing.first_child() {
        p.timing.remove(&child);
    }
    let mut verdict = String::new();
    if let Some(pv) = &preview {
        // One sector is the lap: show it once.
        let sectors = if pv.sectors.len() > 1 {
            &pv.sectors[..]
        } else {
            &[]
        };
        for (i, sector) in sectors.iter().enumerate() {
            let (time, delta) = match sector.seconds {
                Some(t) => (
                    clock(t),
                    sector.delta_best.map(format_delta).unwrap_or_default(),
                ),
                None if sector.out_of_order => ("out of order".into(), String::new()),
                None => ("—".into(), String::new()),
            };
            timing_row(
                &p.timing,
                i as i32,
                &format!("Sector {}", sector.sector),
                &time,
                &delta,
                sector.delta_best,
                sector.fastest,
            );
        }
        let (lap, delta) = match pv.lap_seconds {
            Some(t) => (
                clock(t),
                pv.delta_best.map(format_delta).unwrap_or_default(),
            ),
            None if pv.lap_out_of_order => ("out of order".into(), String::new()),
            None => ("—".into(), String::new()),
        };
        timing_row(
            &p.timing,
            sectors.len() as i32,
            "Lap",
            &lap,
            &delta,
            pv.delta_best,
            false,
        );
        verdict = match (pv.complete, pv.rank, pv.best_lap_seconds) {
            (true, Some((1, n)), _) if n > 1 => "Fastest lap on this course.".to_owned(),
            (true, Some((rank, n)), _) => {
                format!("Would be {} of {n} on this course.", ordinal(rank))
            }
            (false, _, Some(best)) => format!("Best lap here: {}.", clock(best)),
            (false, _, None) => "No runs on this course yet.".to_owned(),
            _ => String::new(),
        };
        if pv.sectors.iter().any(|s| s.out_of_order) || pv.lap_out_of_order {
            verdict.push_str(" Markers must go Start, S1…, Finish, in order.");
        }
    }
    p.verdict.set_text(verdict.trim());

    // Buttons.
    let next = preview.as_ref().and_then(|pv| pv.next.clone());
    p.set_next.set_label(&match &next {
        Some(n) => format!("Set {n} here"),
        None => "All markers set".to_owned(),
    });
    p.set_next.set_sensitive(next.is_some());
    let complete = preview.as_ref().is_some_and(|pv| pv.complete);
    p.commit.set_label(match &s.run_id {
        Some(_) if !s.unsaved => "Committed",
        Some(_) => "Update run",
        None => "Commit run",
    });
    p.commit.set_tooltip_text(Some(&match &s.run_id {
        Some(id) => format!("Save these markers to {id}"),
        None => "Save these markers as a new run".to_owned(),
    }));
    p.commit
        .set_sensitive(complete && s.unsaved && s.db.is_some());
    p.discard.set_sensitive(s.unsaved && !s.auto_busy);
    p.reference.set_text(&match &s.reference {
        Some(r) => format!(
            "Reference: {} ({}, {})",
            r.run_id,
            clock(r.lap_seconds),
            if r.chosen {
                "chosen"
            } else {
                "best lap with footage"
            }
        ),
        None if s.db.is_some() => {
            "No run on this course to place markers from yet: time one by hand first.".to_owned()
        }
        None => String::new(),
    });
    p.auto_place
        .set_sensitive(s.reference.is_some() && !s.auto_busy);
    p.find_course
        .set_sensitive(s.db.as_ref().is_some_and(|db| !db.courses.is_empty()) && !s.auto_busy);
    p.spinner.set_spinning(s.auto_busy);
    let own_run_on_course = s.run_id.as_ref().is_some_and(|id| {
        s.db.as_ref().is_some_and(|db| {
            db.runs
                .iter()
                .any(|r| &r.id == id && Some(&r.course_id) == s.course.as_ref())
        })
    });
    p.use_reference.set_visible(
        own_run_on_course
            && !s.unsaved
            && s.reference.as_ref().map(|r| &r.run_id) != s.run_id.as_ref(),
    );
    p.discard.set_label(if s.run_id.is_some() {
        "Discard changes"
    } else {
        "Clear markers"
    });
}

fn marker_row(p: &Page, s: &State, name: &str, fps: f64) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_title(name);
    let frame = s.markers.get(name).copied();
    let mut subtitle = match frame {
        Some(f) => format!("frame {f} · {}", clock(f as f64 / fps)),
        None => "not set".to_owned(),
    };
    if let Some(c) = s.confidence.get(name) {
        subtitle.push_str(&format!(" · auto, {:.0}% sure", c * 100.0));
        if *c < auto::CHECK_BELOW {
            subtitle.push_str(": check it");
            row.add_css_class("check-marker");
        }
    }
    row.set_subtitle(&subtitle);
    if frame == Some(s.position) {
        row.add_css_class("current-marker");
    }
    let set = gtk::Button::with_label("Set");
    set.set_tooltip_text(Some(&format!("Put {name} on this frame")));
    set.set_valign(gtk::Align::Center);
    let goto = gtk::Button::from_icon_name("find-location-symbolic");
    goto.set_tooltip_text(Some(&format!("Go to {name}")));
    goto.set_valign(gtk::Align::Center);
    let clear = gtk::Button::from_icon_name("edit-clear-symbolic");
    clear.set_tooltip_text(Some(&format!("Clear {name}")));
    clear.set_valign(gtk::Align::Center);
    for b in [&goto, &clear] {
        b.add_css_class("flat");
        b.set_sensitive(frame.is_some());
    }
    row.add_suffix(&set);
    row.add_suffix(&goto);
    row.add_suffix(&clear);
    let (p1, n1) = (p.clone(), name.to_owned());
    set.connect_clicked(move |_| {
        let at = p1.state.borrow().position;
        set_marker(&p1, &n1, Some(at));
    });
    let (p2, n2) = (p.clone(), name.to_owned());
    goto.connect_clicked(move |_| {
        let at = p2.state.borrow().markers.get(&n2).copied();
        if let Some(at) = at {
            seek(&p2, at);
        }
    });
    let (p3, n3) = (p.clone(), name.to_owned());
    clear.connect_clicked(move |_| set_marker(&p3, &n3, None));
    row
}

fn timing_row(
    grid: &gtk::Grid,
    row: i32,
    name: &str,
    time: &str,
    delta: &str,
    delta_value: Option<f64>,
    fastest: bool,
) {
    let name = label(name, if row < 0 { "" } else { "dim-label" });
    let time = label(time, "numeric");
    if fastest {
        time.add_css_class("fastest");
        time.set_tooltip_text(Some("Fastest on this course"));
    }
    let delta = label(delta, "numeric");
    match delta_value {
        Some(d) if d < 0.0 => delta.add_css_class("success"),
        Some(d) if d > 0.0 => delta.add_css_class("warning"),
        _ => {}
    }
    delta.set_tooltip_text(Some("Against the course's best lap"));
    grid.attach(&name, 0, row, 1, 1);
    grid.attach(&time, 1, row, 1, 1);
    grid.attach(&delta, 2, row, 1, 1);
}

fn go(p: &Page, step: Step) {
    let s = p.state.borrow();
    let Some(open) = &s.open else { return };
    let target = match step {
        Step::First => 0,
        Step::Last => open.frames - 1,
        Step::By(n, false) => s.position + n,
        Step::By(n, true) => s.position + n * open.info.fps().round().max(1.0) as i64,
    };
    drop(s);
    seek(p, target);
}

/// Moves to frame `index` and asks for it to be shown.
fn seek(p: &Page, index: i64) {
    let mut s = p.state.borrow_mut();
    let Some(open) = &s.open else { return };
    let index = index.clamp(0, open.frames - 1);
    if index == s.position {
        return;
    }
    let fps = open.info.fps();
    let frames = open.frames;
    let _ = p.requests.send(Request {
        path: open.view.clone(),
        info: open.view_info.clone(),
        index,
    });
    s.position = index;
    drop(s);
    p.position.set_text(&format!(
        "Frame {index} of {} · {}",
        frames - 1,
        clock(index as f64 / fps)
    ));
    p.following.set(true);
    p.scale.set_value(index as f64);
    p.following.set(false);
    // Which marker (if any) sits here.
    let s = p.state.borrow();
    let mut child = p.markers.first_child();
    while let Some(w) = child {
        if let Some(row) = w.downcast_ref::<adw::ActionRow>() {
            let here = s.markers.get(row.title().as_str()) == Some(&index);
            if here {
                row.add_css_class("current-marker");
            } else {
                row.remove_css_class("current-marker");
            }
        }
        child = w.next_sibling();
    }
}

fn set_next_marker(p: &Page) {
    let next = current_preview(&p.state.borrow()).and_then(|pv| pv.next);
    if let Some(name) = next {
        let at = p.state.borrow().position;
        set_marker(p, &name, Some(at));
    }
}

fn set_marker(p: &Page, name: &str, frame: Option<i64>) {
    let mut s = p.state.borrow_mut();
    match frame {
        Some(f) => s.markers.insert(name.to_owned(), f),
        None => s.markers.remove(name),
    };
    // A marker placed by hand has no confidence, and the run is now hand-timed.
    s.confidence.remove(name);
    if s.confidence.is_empty() {
        s.source = "hand".into();
    }
    s.unsaved = true;
    drop(s);
    save_draft(p);
    show(p);
}

fn change_course(p: &Page, id: String) {
    let mut s = p.state.borrow_mut();
    if s.course.as_ref() == Some(&id) {
        return;
    }
    s.course = Some(id);
    if let Some(course) = course(&s).cloned() {
        let kept = keep_for_course(&s.markers, &course);
        s.markers = kept;
        s.unsaved = true;
    }
    drop(s);
    find_reference(p);
    save_draft(p);
    show(p);
}

fn save_draft(p: &Page) {
    let s = p.state.borrow();
    let (Some(open), Some(course)) = (&s.open, &s.course) else {
        return;
    };
    let result = Library::open_default().and_then(|l| {
        if s.unsaved {
            l.set_draft(
                &open.clip.path,
                &Draft {
                    course_id: course.clone(),
                    frames: s.markers.clone(),
                    confidence: s.confidence.clone(),
                    source: s.source.clone(),
                },
            )
        } else {
            l.clear_draft(&open.clip.path)
        }
    });
    if let Err(e) = result {
        p.status
            .set_text(&format!("The draft couldn't be kept: {e:#}"));
    }
}

fn discard_changes(p: &Page) {
    let clip = p.state.borrow().open.as_ref().map(|o| o.clip.clone());
    let Some(clip) = clip else { return };
    if let Err(e) = Library::open_default().and_then(|l| l.clear_draft(&clip.path)) {
        p.status.set_text(&format!("Not discarded: {e:#}"));
        return;
    }
    let had_run = p.state.borrow().run_id.is_some();
    open_clip(p, clip);
    p.status.set_text(if had_run {
        "Back to the committed markers."
    } else {
        "Markers cleared."
    });
}

fn commit_run(p: &Page) {
    let Some(path) = p.shared.settings().run_database.clone() else {
        return;
    };
    let s = p.state.borrow();
    let (Some(open), Some(course_id)) = (&s.open, s.course.clone()) else {
        return;
    };
    let clip_path = open.clip.path.clone();
    let clip_info = ClipInfo::for_file(&clip_path, open.info.fps());
    let markers = s.markers.clone();
    let summary = current_preview(&s);
    drop(s);

    let (mut file, mut db) = match DatabaseFile::load(&path) {
        Ok(x) => x,
        Err(e) => {
            p.status.set_text(&format!(
                "Not committed: the run database couldn't be read: {e}"
            ));
            return;
        }
    };
    let id = match commit::commit(
        &mut db,
        &course_id,
        &clip_info,
        &markers,
        &commit::utc_now(),
    ) {
        Ok(id) => id,
        Err(e) => {
            p.status.set_text(&format!("Not committed: {e}"));
            return;
        }
    };
    match file.save(&db) {
        Ok(()) => {}
        Err(SaveError::ChangedOnDisk(_)) => {
            p.status.set_text(
                "The run database changed while committing (the Resolve script?). Nothing was saved; press Commit again.",
            );
            return;
        }
        Err(e) => {
            p.status.set_text(&format!("Not committed: {e}"));
            return;
        }
    }
    if let Err(e) = Library::open_default().and_then(|l| l.clear_draft(&clip_path)) {
        eprintln!("ghost-run: draft not cleared: {e:#}");
    }
    let mut s = p.state.borrow_mut();
    s.db = Some(db);
    s.run_id = Some(id.clone());
    s.unsaved = false;
    drop(s);
    let lap = summary
        .and_then(|pv| {
            pv.lap_seconds.map(|l| {
                let delta = pv
                    .delta_best
                    .map(|d| format!(", {} on the best", format_delta(d)))
                    .unwrap_or_default();
                format!(": lap {}{delta}", clock(l))
            })
        })
        .unwrap_or_default();
    p.status.set_text(&format!(
        "Saved as {id}{lap}. The previous database is in the backups folder beside it."
    ));
    show(p);
    p.shared.notify(Change::Runs);
}

/// Starts placing markers on a background thread: from the chosen course's
/// reference, or (`every_course`) against every course's.
fn start_auto(p: &Page, every_course: bool) {
    let mut s = p.state.borrow_mut();
    let Some(open) = &s.open else { return };
    let clip = open.clip.path.clone();
    let footage = open.view.clone();
    let references = if every_course {
        let Some(db) = &s.db else { return };
        match Library::open_default().and_then(|l| {
            let clips = l.clips()?;
            auto::references(db, &l, &clips)
        }) {
            Ok(r) => r,
            Err(e) => {
                p.status.set_text(&format!("{e:#}"));
                return;
            }
        }
    } else {
        s.reference.clone().into_iter().collect()
    };
    s.auto_busy = true;
    drop(s);
    p.status.set_text(if every_course {
        "Matching the clip against each course's reference run…"
    } else {
        "Placing markers from the reference run…"
    });
    show(p);
    let tx = p.auto.clone();
    std::thread::spawn(move || {
        let cache = auto::cache_dir();
        let result = if every_course {
            Auto::Matched(
                clip,
                auto::match_courses(&references, &footage, cache.as_deref())
                    .map_err(|e| format!("{e:#}")),
            )
        } else {
            Auto::Placed(
                clip,
                auto::place(&references[0], &footage, cache.as_deref())
                    .map_err(|e| format!("{e:#}")),
            )
        };
        let _ = tx.send(result);
    });
}

fn auto_finished(p: &Page, result: Auto) {
    p.state.borrow_mut().auto_busy = false;
    let (clip, placement, note) = match result {
        Auto::Placed(clip, Ok(placement)) => (clip, placement, String::new()),
        Auto::Matched(clip, Ok(ranked)) => {
            let names = course_names(p);
            let best = ranked[0].clone();
            let name = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_owned());
            let next = ranked
                .get(1)
                .map(|n| {
                    format!(
                        " (next closest: {}, {:.2})",
                        name(&n.course_id),
                        n.match_cost
                    )
                })
                .unwrap_or_default();
            if best.match_cost >= auto::POOR_MATCH {
                p.status.set_text(&format!(
                    "No course matched well (closest: {}, {:.2}){next}. Is the run in this clip? Pick the course and place the markers by hand.",
                    name(&best.course_id),
                    best.match_cost
                ));
                show(p);
                return;
            }
            let note = format!("Looks like {}{next}. ", name(&best.course_id));
            (clip, best, note)
        }
        Auto::Placed(_, Err(e)) | Auto::Matched(_, Err(e)) => {
            p.status.set_text(&format!("Markers not placed: {e}"));
            show(p);
            return;
        }
    };
    let mut s = p.state.borrow_mut();
    if s.open.as_ref().map(|o| &o.clip.path) != Some(&clip) {
        // Another clip was opened meanwhile.
        drop(s);
        show(p);
        return;
    }
    let course_changed = s.course.as_ref() != Some(&placement.course_id);
    s.course = Some(placement.course_id.clone());
    s.markers = placement.markers.clone();
    s.confidence = placement.confidence.clone();
    s.source = "auto".into();
    s.unsaved = true;
    let start = s.markers.get("Start").copied();
    drop(s);
    if course_changed {
        find_reference(p);
    }
    save_draft(p);
    show(p);
    if let Some(start) = start {
        seek(p, start);
    }
    let check = placement.to_check();
    let advice = if check.is_empty() {
        "Step through each marker to be sure, then commit.".to_owned()
    } else {
        format!("Check {} by eye, then commit.", check.join(", "))
    };
    let quality = if placement.quality() == "poor" {
        "poor: is the run in this clip?"
    } else {
        placement.quality()
    };
    p.status.set_text(&format!(
        "{note}Placed from {}: match {quality} ({:.2}). {advice}",
        placement.reference_run, placement.match_cost
    ));
}

/// Course id → name.
fn course_names(p: &Page) -> std::collections::HashMap<String, String> {
    p.state
        .borrow()
        .db
        .as_ref()
        .map(|db| {
            db.courses
                .iter()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn use_as_reference(p: &Page) {
    let s = p.state.borrow();
    let (Some(course), Some(run)) = (s.course.clone(), s.run_id.clone()) else {
        return;
    };
    drop(s);
    match Library::open_default().and_then(|l| l.set_reference(&course, &run)) {
        Ok(()) => {
            find_reference(p);
            p.status.set_text(&format!(
                "{run} is now this course's reference: other clips' markers are placed from it."
            ));
        }
        Err(e) => p.status.set_text(&format!("Not changed: {e:#}")),
    }
    show(p);
}

/// Arrow keys and friends step through the clip while this page is showing.
fn keyboard(p: &Page) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let page = p.clone();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let p = &page;
        if !p.root.is_mapped()
            || p.state.borrow().open.is_none()
            || p.views.visible_child_name().as_deref() != Some("editor")
            || p.shared.window.visible_dialog().is_some()
        {
            return glib::Propagation::Proceed;
        }
        // Leave typing alone.
        let typing = gtk::prelude::GtkWindowExt::focus(&p.shared.window)
            .is_some_and(|w| w.is::<gtk::Text>() || w.is::<gtk::TextView>());
        if typing {
            return glib::Propagation::Proceed;
        }
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
        let other = modifiers.intersects(
            gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::SUPER_MASK,
        );
        if other {
            return glib::Propagation::Proceed;
        }
        let step = match key {
            gdk::Key::Left | gdk::Key::KP_Left => Step::By(if shift { -10 } else { -1 }, false),
            gdk::Key::Right | gdk::Key::KP_Right => Step::By(if shift { 10 } else { 1 }, false),
            gdk::Key::Page_Up => Step::By(-1, true),
            gdk::Key::Page_Down => Step::By(1, true),
            gdk::Key::Home => Step::First,
            gdk::Key::End => Step::Last,
            gdk::Key::m | gdk::Key::M => {
                set_next_marker(p);
                return glib::Propagation::Stop;
            }
            _ => return glib::Propagation::Proceed,
        };
        go(p, step);
        glib::Propagation::Stop
    });
    p.shared.window.add_controller(keys);
}

fn shortcuts_popover() -> gtk::Popover {
    let grid = gtk::Grid::new();
    grid.set_column_spacing(16);
    grid.set_row_spacing(6);
    grid.set_margin_top(8);
    grid.set_margin_bottom(8);
    grid.set_margin_start(8);
    grid.set_margin_end(8);
    let rows = [
        ("← →", "One frame"),
        ("Shift ← →", "Ten frames"),
        ("Page Up / Down", "One second"),
        ("Home / End", "First / last frame"),
        ("M", "Set the next marker here"),
    ];
    for (i, (key, what)) in rows.iter().enumerate() {
        grid.attach(&label(key, "heading"), 0, i as i32, 1, 1);
        grid.attach(&label(what, ""), 1, i as i32, 1, 1);
    }
    let popover = gtk::Popover::new();
    popover.set_child(Some(&grid));
    popover
}

/// `M:SS.mmm`.
fn clock(seconds: f64) -> String {
    format_duration(seconds.max(0.0)).unwrap_or_default()
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
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
    fn ordinals_read_naturally() {
        let got: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 101, 111]
            .into_iter()
            .map(ordinal)
            .collect();
        assert_eq!(
            got,
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st",
                "111th"
            ]
        );
    }
}
