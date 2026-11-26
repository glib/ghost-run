//! The Import page: camera card in, originals verified on the NAS, editing
//! copies made, and a plain answer at the end. Cards are found from the mount
//! table every couple of seconds (`ingest::cards`), so a card is picked up
//! however often it's reinserted; a folder can be imported instead. The
//! card's clips are listed by recording day with thumbnails
//! (`library::thumbs`), and clips the import journal already verified on the
//! NAS (same path on the card, size and time; still there) are marked and
//! left out. One button plans and runs the batch through the same
//! `ingest::pipeline` as before; each clip shows how far it has got, and the
//! end says whether the card is safe to wipe.
//!
//! The old Import window (convert existing files, history, recovery, cleanup,
//! reports) opens over this page from "History and recovery". Both open the
//! journal only while they work, since it allows one user at a time.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use adw::prelude::*;
use chrono::NaiveDateTime;
use gtk::glib;
use ingest::cards::{self, Card};
use ingest::diagnostics::human_bytes;
use ingest::jobs::{Archived, Jobs};
use ingest::model::{Batch, Cancel, Candidate, Event, Item, Stage};
use ingest::settings::{self, Settings, Workflow};
use ingest::{catalog, discovery, pipeline};

use library::Clip;
use library::footage::Roots;
use library::hydrate::Settled;

use crate::shared::{Change, Shared};

enum Command {
    /// List a card or folder's clips, and which are on the NAS already.
    Scan(PathBuf),
    /// Plan and run an import of these candidates (selection set), keeping
    /// the local copies of the clips in the set (relative paths).
    Import(Box<Settings>, Vec<Candidate>, HashSet<PathBuf>),
    /// Run a stopped or failed batch again.
    Retry(String, HashSet<PathBuf>),
}

/// What happened to the local copies after an import ([`settle_batch`]).
#[derive(Default)]
struct Settle {
    kept: usize,
    removed: usize,
    freed: u64,
    /// Clip name and why, for the ones whose preview or proxy failed, or
    /// whose copy couldn't be removed.
    failed: Vec<(String, String)>,
    /// Originals verified on the NAS by the batch, for the Library.
    originals: Vec<PathBuf>,
}

enum Reply {
    Scanning(usize),
    Scanned {
        source: PathBuf,
        candidates: Vec<Candidate>,
        /// Card path → the original on the NAS.
        imported: HashMap<PathBuf, PathBuf>,
    },
    Thumb(usize, PathBuf, PathBuf),
    Started(Box<Batch>),
    /// After the copies: clip `done` of `total`, its step and how far.
    Settling {
        done: usize,
        total: usize,
        step: String,
        fraction: f64,
    },
    Finished(Box<Batch>, Box<Settle>),
    Error(String),
}

#[derive(Default, PartialEq)]
enum Busy {
    #[default]
    No,
    Scanning,
    Importing,
}

struct Progress {
    stage: String,
    bytes: u64,
    total: u64,
    began: Instant,
    first: u64,
}

#[derive(Default)]
struct State {
    cards: Vec<Card>,
    /// What is listed: a card's mount point or a folder.
    source: Option<PathBuf>,
    candidates: Vec<Candidate>,
    imported: HashMap<PathBuf, PathBuf>,
    ticked: HashSet<PathBuf>,
    /// Clips whose local copy stays after the import, to edit now.
    keep: HashSet<PathBuf>,
    /// Keep every ticked clip's local copy.
    keep_all: bool,
    thumbs: HashMap<PathBuf, PathBuf>,
    batch: Option<Batch>,
    progress: HashMap<usize, Progress>,
    busy: Busy,
    /// Shown above the list: what happened.
    result: Option<(String, &'static str)>,
    // Widgets for the listed clips, to update without rebuilding.
    pictures: HashMap<PathBuf, gtk::Picture>,
    statuses: HashMap<PathBuf, gtk::Label>,
    checks: HashMap<PathBuf, gtk::CheckButton>,
    keeps: HashMap<PathBuf, gtk::ToggleButton>,
}

impl State {
    /// The ticked clips whose local copies are kept.
    fn kept(&self) -> HashSet<PathBuf> {
        self.ticked
            .iter()
            .filter(|r| self.keep_all || self.keep.contains(*r))
            .cloned()
            .collect()
    }
}

#[derive(Clone)]
struct Page {
    shared: Shared,
    state: Rc<RefCell<State>>,
    tx: mpsc::Sender<Command>,
    thumbs_tx: mpsc::Sender<(usize, Vec<(PathBuf, PathBuf)>)>,
    thumbs_gen: Arc<AtomicUsize>,
    cancel: Cancel,
    where_label: gtk::Label,
    setup: gtk::Box,
    sources: gtk::Box,
    empty: adw::StatusPage,
    list_area: gtk::Box,
    result: gtk::Box,
    result_text: gtk::Label,
    open_nas: gtk::Button,
    show_library: gtk::Button,
    retry: gtk::Button,
    list: gtk::ListBox,
    summary: gtk::Label,
    progress: gtk::ProgressBar,
    import: gtk::Button,
    stop: gtk::Button,
    keep_all: gtk::CheckButton,
}

/// The page: this view, with the old Import window opening over it.
pub fn page(shared: &Shared, advanced: gtk::Widget) -> gtk::Widget {
    let nav = adw::NavigationView::new();
    let advanced = adw::NavigationPage::builder()
        .child(&advanced)
        .title("History and recovery")
        .tag("advanced")
        .build();
    let nav2 = nav.clone();
    let main = view(shared, move || nav2.push(&advanced));
    nav.add(
        &adw::NavigationPage::builder()
            .child(&main)
            .title("Import")
            .tag("import")
            .build(),
    );
    nav.upcast()
}

fn view(shared: &Shared, open_advanced: impl Fn() + 'static) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Import", "")));
    let advanced = gtk::Button::with_label("History and recovery");
    advanced.set_tooltip_text(Some(
        "Earlier imports, retrying or cleaning up after them, reports, and converting files already on disk",
    ));
    header.pack_end(&advanced);
    root.append(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("content");
    content.set_vexpand(true);

    // Where things go, and a way to change it.
    let where_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    where_row.add_css_class("locations");
    let where_label = label("", "");
    where_label.set_wrap(true);
    where_label.set_hexpand(true);
    let change = gtk::Button::with_label("Change…");
    change.set_valign(gtk::Align::Center);
    where_row.append(&where_label);
    where_row.append(&change);
    content.append(&where_row);

    let setup = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    setup.add_css_class("banner");
    let setup_text = label(
        "Choose where originals go (a folder on your NAS) and where editing copies go, in Settings.",
        "",
    );
    setup_text.set_wrap(true);
    setup_text.set_hexpand(true);
    let setup_button = gtk::Button::with_label("Settings…");
    setup.append(&setup_text);
    setup.append(&setup_button);
    content.append(&setup);

    // Cards found, and a folder instead.
    let sources = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.append(&sources);

    let empty = adw::StatusPage::builder()
        .icon_name("media-flash-symbolic")
        .title("Insert a camera card")
        .description("Ghost Run finds it as soon as it's mounted. Or import from a folder.")
        .vexpand(true)
        .build();
    content.append(&empty);

    let list_area = gtk::Box::new(gtk::Orientation::Vertical, 12);
    list_area.set_vexpand(true);
    let result = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    result.add_css_class("banner");
    let result_text = label("", "");
    result_text.set_wrap(true);
    result_text.set_hexpand(true);
    let show_library = gtk::Button::with_label("Show in Library");
    let open_nas = gtk::Button::with_label("Open NAS folder");
    let retry = gtk::Button::with_label("Try again");
    for w in [
        result_text.upcast_ref::<gtk::Widget>(),
        retry.upcast_ref(),
        open_nas.upcast_ref(),
        show_library.upcast_ref(),
    ] {
        result.append(w);
    }
    result.set_visible(false);
    list_area.append(&result);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    list_area.append(&scroll);
    content.append(&list_area);
    root.append(&content);

    let footer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    footer.add_css_class("footer");
    let footer_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let summary = label("", "");
    summary.set_wrap(true);
    summary.set_hexpand(true);
    let keep_all = gtk::CheckButton::with_label("Keep all here to edit now");
    keep_all.set_tooltip_text(Some(
        "Local copies are removed once the originals are verified on the NAS, \
         unless kept. Kept clips get a proxy too.",
    ));
    let stop = gtk::Button::with_label("Stop");
    let import = gtk::Button::with_label("Import");
    import.add_css_class("suggested-action");
    footer_row.append(&summary);
    footer_row.append(&keep_all);
    footer_row.append(&stop);
    footer_row.append(&import);
    footer.append(&footer_row);
    let progress = gtk::ProgressBar::new();
    progress.set_visible(false);
    footer.append(&progress);
    root.append(&footer);

    let (tx, commands) = mpsc::channel();
    let (replies, rx) = mpsc::channel();
    let (events, erx) = mpsc::sync_channel(64);
    let cancel = Cancel::default();
    worker(commands, replies.clone(), events, cancel.clone());
    let thumbs_gen = Arc::new(AtomicUsize::new(0));
    let thumbs_tx = thumbs_worker(replies, thumbs_gen.clone());

    let page = Page {
        shared: shared.clone(),
        state: Rc::default(),
        tx,
        thumbs_tx,
        thumbs_gen,
        cancel,
        where_label,
        setup,
        sources,
        empty,
        list_area,
        result,
        result_text,
        open_nas,
        show_library,
        retry,
        list,
        summary,
        progress,
        import,
        stop,
        keep_all,
    };

    let s = shared.clone();
    change.connect_clicked(move |_| crate::settings_dialog::open(&s));
    let s = shared.clone();
    setup_button.connect_clicked(move |_| crate::settings_dialog::open(&s));
    advanced.connect_clicked(move |_| open_advanced());
    let p = page.clone();
    page.import.connect_clicked(move |_| start_import(&p));
    let p = page.clone();
    page.stop.connect_clicked(move |_| {
        p.cancel.request();
        p.stop.set_sensitive(false);
        p.summary
            .set_text("Stopping after the files being worked on now…");
    });
    let p = page.clone();
    page.keep_all.connect_toggled(move |b| {
        p.state.borrow_mut().keep_all = b.is_active();
        sync_keeps(&p);
        refresh(&p);
    });
    let p = page.clone();
    page.retry.connect_clicked(move |_| {
        let s = p.state.borrow();
        let id = s.batch.as_ref().map(|b| b.id.clone());
        let keep = s.kept();
        drop(s);
        if let Some(id) = id {
            begin(&p, Busy::Importing);
            let _ = p.tx.send(Command::Retry(id, keep));
        }
    });
    let p = page.clone();
    page.show_library
        .connect_clicked(move |_| p.shared.notify(Change::ShowLibrary));
    let p = page.clone();
    page.open_nas.connect_clicked(move |_| {
        let folder = p.state.borrow().batch.as_ref().and_then(nas_folder);
        if let Some(folder) = folder {
            let q = p.clone();
            crate::desktop::open_folder(&p.shared.window, &folder, move |e| q.summary.set_text(&e));
        }
    });
    let p = page.clone();
    shared.subscribe(move |change| {
        if change == Change::Settings {
            refresh(&p);
        }
    });

    let p = page.clone();
    glib::timeout_add_local(Duration::from_millis(150), move || {
        for reply in rx.try_iter() {
            handle(&p, reply);
        }
        let mut changed = false;
        for event in erx.try_iter() {
            changed |= apply_event(&p, event);
        }
        if changed {
            update_progress(&p);
        }
        glib::ControlFlow::Continue
    });
    let p = page.clone();
    glib::timeout_add_local(Duration::from_secs(2), move || {
        poll_cards(&p);
        glib::ControlFlow::Continue
    });
    poll_cards(&page);
    // GHOST_RUN_IMPORT_FROM=<folder> lists that folder at start: for screenshots.
    if let Some(folder) = std::env::var_os("GHOST_RUN_IMPORT_FROM") {
        scan(&page, folder.into());
    }
    refresh(&page);
    root.upcast()
}

fn worker(
    commands: mpsc::Receiver<Command>,
    replies: mpsc::Sender<Reply>,
    events: ingest::model::Events,
    cancel: Cancel,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("a tokio runtime");
        for command in commands {
            cancel.0.store(false, Ordering::SeqCst);
            let result: anyhow::Result<Reply> = rt.block_on(async {
                match command {
                    Command::Scan(source) => {
                        let tx = replies.clone();
                        let mut last = Instant::now();
                        let candidates =
                            discovery::scan_with_progress(&source, &cancel, false, |n, _| {
                                if last.elapsed() > Duration::from_millis(200) {
                                    last = Instant::now();
                                    let _ = tx.send(Reply::Scanning(n));
                                }
                            })
                            .await?;
                        let archived =
                            ingest::jobs::archived(&settings::data_dir()).unwrap_or_default();
                        let imported = imported(&candidates, &archived, |p| p.exists());
                        Ok(Reply::Scanned {
                            source,
                            candidates,
                            imported,
                        })
                    }
                    Command::Import(settings, candidates, keep) => {
                        let mut batch = pipeline::plan(*settings, candidates, &cancel).await?;
                        let mut jobs = Jobs::open(&settings::data_dir())?;
                        jobs.save(&batch)?;
                        let _ = replies.send(Reply::Started(Box::new(batch.clone())));
                        pipeline::run(&mut jobs, &mut batch, None, &cancel, &events).await?;
                        drop(jobs);
                        let roots = Roots::from_import(&batch.settings);
                        let settled = settle_batch(&batch, &roots, &keep, &cancel, &replies);
                        Ok(Reply::Finished(Box::new(batch), Box::new(settled)))
                    }
                    Command::Retry(id, keep) => {
                        let mut jobs = Jobs::open(&settings::data_dir())?;
                        let mut batch = jobs.load(&id)?;
                        let _ = replies.send(Reply::Started(Box::new(batch.clone())));
                        pipeline::run(&mut jobs, &mut batch, None, &cancel, &events).await?;
                        drop(jobs);
                        let roots = Roots::from_import(&batch.settings);
                        let settled = settle_batch(&batch, &roots, &keep, &cancel, &replies);
                        Ok(Reply::Finished(Box::new(batch), Box::new(settled)))
                    }
                }
            });
            let _ = replies.send(result.unwrap_or_else(|e| Reply::Error(format!("{e:#}"))));
        }
    });
}

/// Makes thumbnails one at a time; a newer list (generation) replaces an older one.
fn thumbs_worker(
    replies: mpsc::Sender<Reply>,
    generation: Arc<AtomicUsize>,
) -> mpsc::Sender<(usize, Vec<(PathBuf, PathBuf)>)> {
    let (tx, rx) = mpsc::channel::<(usize, Vec<(PathBuf, PathBuf)>)>();
    std::thread::spawn(move || {
        let cache = library::thumbs::cache_dir();
        while let Ok((wanted, clips)) = rx.recv() {
            for (relative, path) in clips {
                if generation.load(Ordering::SeqCst) != wanted {
                    break;
                }
                if let Ok(png) = library::thumbs::thumbnail(&path, &cache) {
                    let _ = replies.send(Reply::Thumb(wanted, relative, png));
                }
            }
        }
    });
    tx
}

/// Card paths already verified on the NAS, matched to the import journal by
/// path on the card, size and modification time, while the original is still
/// there.
fn imported(
    candidates: &[Candidate],
    archived: &[Archived],
    exists: impl Fn(&Path) -> bool,
) -> HashMap<PathBuf, PathBuf> {
    let mut by_source: HashMap<(&Path, u64, i64), &Path> = HashMap::new();
    for a in archived {
        by_source.insert(
            (a.source_relative.as_path(), a.source_size, a.source_mtime),
            a.original.as_path(),
        );
    }
    candidates
        .iter()
        .filter_map(|c| {
            let original =
                by_source.get(&(c.relative.as_path(), c.identity.size, c.identity.mtime))?;
            exists(original).then(|| (c.relative.clone(), original.to_path_buf()))
        })
        .collect()
}

/// The clips listed: videos that get an editing copy (not sidecars, not
/// other files).
fn clips(candidates: &[Candidate]) -> impl Iterator<Item = &Candidate> {
    candidates.iter().filter(|c| c.editing)
}

/// The candidates for an import: each ticked clip and its sidecars.
fn selection(candidates: &[Candidate], ticked: &HashSet<PathBuf>) -> Vec<Candidate> {
    candidates
        .iter()
        .cloned()
        .map(|mut c| {
            c.selected = if c.editing {
                ticked.contains(&c.relative)
            } else {
                c.sidecar_for.as_ref().is_some_and(|v| ticked.contains(v))
            };
            c
        })
        .collect()
}

fn recorded(c: &Candidate) -> NaiveDateTime {
    catalog::recorded(c).date
}

/// The clips by recording day, oldest first.
fn by_day(candidates: &[Candidate]) -> Vec<(chrono::NaiveDate, Vec<&Candidate>)> {
    let mut list: Vec<&Candidate> = clips(candidates).collect();
    list.sort_by_key(|c| (recorded(c), c.relative.clone()));
    let mut days: Vec<(chrono::NaiveDate, Vec<&Candidate>)> = Vec::new();
    for c in list {
        let day = ingest::layout::day(c);
        match days.last_mut() {
            Some((d, v)) if *d == day => v.push(c),
            _ => days.push((day, vec![c])),
        }
    }
    days
}

/// How far a clip in the batch has got.
fn item_status(item: &Item, progress: Option<&Progress>) -> (String, &'static str) {
    if let Some(e) = &item.error {
        return (format!("Failed: {}", first_line(e)), "error");
    }
    if item.complete() {
        return ("Done ✓".into(), "success");
    }
    if let Some(p) = progress {
        let pct = if p.total > 0 {
            format!(" {:.0}%", p.bytes as f64 * 100.0 / p.total as f64)
        } else {
            String::new()
        };
        return (format!("{}{pct}", p.stage), "");
    }
    match (&item.archive, &item.editing) {
        (Stage::Pending, _) => ("Waiting".into(), ""),
        (Stage::Verified, Stage::Pending) => ("On the NAS · editing copy next".into(), ""),
        (Stage::Canceled, _) | (_, Stage::Canceled) => ("Stopped".into(), "warning"),
        (Stage::Interrupted, _) | (_, Stage::Interrupted) => ("Interrupted".into(), "warning"),
        (a, _) => (a.to_string(), ""),
    }
}

/// The Library's clip for an item whose original is verified on the NAS and
/// whose local copy was made and verified: `None` for anything else.
fn imported_clip(item: &Item, roots: &Roots) -> Option<Clip> {
    if !(item.source.editing
        && item.error.is_none()
        && item.archive == Stage::Verified
        && item.editing == Stage::Verified)
    {
        return None;
    }
    let path = roots.nas.as_ref()?.join(&item.archive_path);
    let local = roots.local_copy(&path).filter(|l| l.is_file())?;
    let info = library::probe::probe(&local).ok()?;
    Some(Clip {
        path,
        size: item.source.identity.size,
        info: Some(info),
        local: Some(local),
        ..Clip::default()
    })
}

/// After the copies (decision 0005): each new clip's preview is made from its
/// local copy, then the copy is kept (with a proxy) if asked, else removed,
/// now that its original is verified on the NAS. Failures are noted and
/// leave the copy in place; nothing here fails the import.
fn settle_batch(
    batch: &Batch,
    roots: &Roots,
    keep: &HashSet<PathBuf>,
    cancel: &Cancel,
    replies: &mpsc::Sender<Reply>,
) -> Settle {
    let mut out = Settle {
        originals: batch
            .items
            .iter()
            .filter(|i| i.source.editing && i.archive == Stage::Verified)
            .filter_map(|i| Some(batch.settings.archive.as_ref()?.path.join(&i.archive_path)))
            .collect(),
        ..Settle::default()
    };
    let clips: Vec<(Clip, bool)> = batch
        .items
        .iter()
        .filter_map(|i| Some((imported_clip(i, roots)?, keep.contains(&i.source.relative))))
        .collect();
    let total = clips.len();
    for (done, (clip, kept)) in clips.into_iter().enumerate() {
        if cancel.0.load(Ordering::SeqCst) {
            break;
        }
        let mut last = Instant::now();
        let result = library::hydrate::settle(&clip, roots, kept, &cancel.0, |step, fraction| {
            if last.elapsed() > Duration::from_millis(200) {
                last = Instant::now();
                let _ = replies.send(Reply::Settling {
                    done,
                    total,
                    step: step.to_owned(),
                    fraction,
                });
            }
        });
        match result {
            Ok(Settled::Kept) => out.kept += 1,
            Ok(Settled::Removed(bytes)) => {
                out.removed += 1;
                out.freed += bytes;
            }
            Err(e) => out.failed.push((clip.name(), format!("{e:#}"))),
        }
    }
    out
}

/// What happened, in a sentence or two.
fn result_text(b: &Batch, settled: &Settle) -> (String, &'static str) {
    let videos: Vec<&Item> = b.items.iter().filter(|i| i.source.editing).collect();
    let done = videos.iter().filter(|i| i.complete()).count();
    let failed: Vec<&&Item> = videos.iter().filter(|i| i.error.is_some()).collect();
    let bytes: u64 = b
        .items
        .iter()
        .filter(|i| i.complete())
        .map(|i| i.source.identity.size)
        .sum();
    let mut text = format!(
        "Imported {done} of {} {} ({}).",
        videos.len(),
        if videos.len() == 1 { "clip" } else { "clips" },
        human_bytes(bytes)
    );
    if let Some(first) = failed.first() {
        text.push_str(&format!(
            " {} failed: {}",
            failed.len(),
            first_line(first.error.as_deref().unwrap_or_default())
        ));
    }
    match (settled.kept, settled.removed) {
        (0, 0) => {}
        (0, n) => text.push_str(&format!(
            " Previews made; {n} local {} removed ({}).",
            if n == 1 { "copy" } else { "copies" },
            human_bytes(settled.freed)
        )),
        (k, 0) => text.push_str(&format!(
            " Previews made; {k} local {} kept here to edit.",
            if k == 1 { "copy" } else { "copies" }
        )),
        (k, n) => text.push_str(&format!(
            " Previews made; {k} local {} kept here to edit, {n} removed ({}).",
            if k == 1 { "copy" } else { "copies" },
            human_bytes(settled.freed)
        )),
    }
    if let Some((name, why)) = settled.failed.first() {
        text.push_str(&format!(
            " {} local {} left in place: {name}: {}",
            settled.failed.len(),
            if settled.failed.len() == 1 {
                "copy was"
            } else {
                "copies were"
            },
            first_line(why)
        ));
    }
    if b.safe_to_wipe() {
        text.push_str(" Every original is verified on the NAS: you can wipe the card.");
        (text, "success")
    } else {
        text.push_str(" Keep the card: not every original is verified on the NAS yet.");
        (text, "warning")
    }
}

/// The NAS folder of the batch's first original.
fn nas_folder(b: &Batch) -> Option<PathBuf> {
    let archive = &b.settings.archive.as_ref()?.path;
    let first = b.items.first()?;
    Some(archive.join(&first.archive_path).parent()?.to_path_buf())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

fn poll_cards(p: &Page) {
    let found = cards::find();
    let mut s = p.state.borrow_mut();
    if found == s.cards {
        return;
    }
    let new: Vec<Card> = found
        .iter()
        .filter(|c| !s.cards.contains(c))
        .cloned()
        .collect();
    // The listed card was taken out (a folder stays listed).
    let listed_card_gone = s.source.as_ref().is_some_and(|src| {
        s.cards.iter().any(|c| &c.path == src) && !found.iter().any(|c| &c.path == src)
    });
    s.cards = found;
    let idle = s.busy == Busy::No;
    drop(s);
    if idle && listed_card_gone {
        let mut s = p.state.borrow_mut();
        s.source = None;
        s.candidates.clear();
        s.result = None;
        s.batch = None;
    }
    // A card just put in is listed straight away, unless something's running.
    if idle && let Some(card) = new.first() {
        scan(p, card.path.clone());
    } else {
        refresh(p);
    }
}

fn scan(p: &Page, source: PathBuf) {
    if p.state.borrow().busy != Busy::No {
        return;
    }
    {
        let mut s = p.state.borrow_mut();
        s.source = Some(source.clone());
        s.candidates.clear();
        s.batch = None;
        s.result = None;
        s.progress.clear();
    }
    begin(p, Busy::Scanning);
    p.summary.set_text("Reading the card…");
    let _ = p.tx.send(Command::Scan(source));
}

fn begin(p: &Page, busy: Busy) {
    p.state.borrow_mut().busy = busy;
    p.stop.set_sensitive(true);
    refresh(p);
}

fn choose_folder(p: &Page) {
    let dialog = gtk::FileDialog::builder()
        .title("Import from a folder")
        .build();
    let p = p.clone();
    dialog.select_folder(
        Some(&p.shared.window.clone()),
        gtk::gio::Cancellable::NONE,
        move |r| {
            if let Ok(file) = r
                && let Some(path) = file.path()
            {
                scan(&p, path);
            }
        },
    );
}

fn start_import(p: &Page) {
    let settings = match Settings::load() {
        Ok(s) => s,
        Err(e) => {
            p.summary
                .set_text(&format!("Import settings couldn't be read: {e:#}"));
            return;
        }
    };
    let s = p.state.borrow();
    let Some(source) = s.source.clone() else {
        return;
    };
    let candidates = selection(&s.candidates, &s.ticked);
    let keep = s.kept();
    drop(s);
    if !candidates.iter().any(|c| c.selected) {
        return;
    }
    let settings = Settings {
        source,
        workflow: Workflow::Ingest,
        ..settings
    };
    {
        let mut s = p.state.borrow_mut();
        s.result = None;
        s.progress.clear();
    }
    begin(p, Busy::Importing);
    p.summary.set_text("Checking the NAS and free space…");
    let _ =
        p.tx.send(Command::Import(Box::new(settings), candidates, keep));
}

fn handle(p: &Page, reply: Reply) {
    match reply {
        Reply::Scanning(n) => {
            p.summary
                .set_text(&format!("Reading the card… {n} files so far"));
        }
        Reply::Scanned {
            source,
            candidates,
            imported,
        } => {
            let mut s = p.state.borrow_mut();
            if s.source.as_ref() != Some(&source) {
                return;
            }
            s.ticked = clips(&candidates)
                .filter(|c| !imported.contains_key(&c.relative))
                .map(|c| c.relative.clone())
                .collect();
            let wanted: Vec<(PathBuf, PathBuf)> = clips(&candidates)
                .filter(|c| !s.thumbs.contains_key(&c.relative))
                .map(|c| (c.relative.clone(), source.join(&c.relative)))
                .collect();
            s.candidates = candidates;
            s.imported = imported;
            s.thumbs.clear();
            s.busy = Busy::No;
            drop(s);
            let generation_no = p.thumbs_gen.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = p.thumbs_tx.send((generation_no, wanted));
            fill(p);
            refresh(p);
        }
        Reply::Thumb(generation_no, relative, png) => {
            if generation_no != p.thumbs_gen.load(Ordering::SeqCst) {
                return;
            }
            let mut s = p.state.borrow_mut();
            if let Some(picture) = s.pictures.get(&relative) {
                picture.set_filename(Some(&png));
            }
            s.thumbs.insert(relative, png);
        }
        Reply::Started(batch) => {
            p.state.borrow_mut().batch = Some(*batch);
            update_progress(p);
        }
        Reply::Settling {
            done,
            total,
            step,
            fraction,
        } => {
            p.summary.set_text(&format!(
                "Making {} · clip {} of {total}",
                if step == "proxy" {
                    "a proxy"
                } else {
                    "previews"
                },
                done + 1
            ));
            p.progress.set_visible(true);
            p.progress
                .set_fraction((done as f64 + fraction) / total.max(1) as f64);
        }
        Reply::Finished(batch, settled) => {
            let result = result_text(&batch, &settled);
            if !settled.originals.is_empty() {
                p.shared.imported(settled.originals.clone());
            }
            let mut s = p.state.borrow_mut();
            s.busy = Busy::No;
            s.progress.clear();
            // What's verified now counts as on the NAS.
            if let Some(archive) = &batch.settings.archive {
                for i in &batch.items {
                    if i.archive == Stage::Verified {
                        s.imported.insert(
                            i.source.relative.clone(),
                            archive.path.join(&i.archive_path),
                        );
                    }
                }
            }
            s.ticked.retain(|r| {
                !batch
                    .items
                    .iter()
                    .any(|i| &i.source.relative == r && i.complete())
            });
            let ticked = s.ticked.clone();
            s.keep.retain(|r| ticked.contains(r));
            s.result = Some(result);
            s.batch = Some(*batch);
            drop(s);
            update_progress(p);
            refresh(p);
        }
        Reply::Error(e) => {
            let mut s = p.state.borrow_mut();
            let importing = s.busy == Busy::Importing;
            s.busy = Busy::No;
            s.result = Some((
                if importing {
                    format!("The import didn't run: {}", first_line(&e))
                } else {
                    format!("The card couldn't be read: {}", first_line(&e))
                },
                "error",
            ));
            drop(s);
            refresh(p);
        }
    }
}

/// Applies a pipeline event; true if something shown changed.
fn apply_event(p: &Page, event: Event) -> bool {
    let mut s = p.state.borrow_mut();
    match event {
        Event::Progress {
            item,
            stage,
            bytes,
            total,
        } => {
            let entry = s.progress.entry(item).or_insert_with(|| Progress {
                stage: stage.clone(),
                bytes,
                total,
                began: Instant::now(),
                first: bytes,
            });
            if entry.stage != stage {
                *entry = Progress {
                    stage,
                    bytes,
                    total,
                    began: Instant::now(),
                    first: bytes,
                };
            } else {
                entry.bytes = bytes;
                entry.total = total;
            }
            true
        }
        Event::Item { index, item } => {
            s.progress.remove(&index);
            if let Some(b) = &mut s.batch
                && let Some(slot) = b.items.get_mut(index)
            {
                *slot = *item;
            }
            true
        }
        Event::Activity { .. } | Event::Message(_) => false,
    }
}

/// Updates each clip's status and the footer while a batch runs.
fn update_progress(p: &Page) {
    let s = p.state.borrow();
    let Some(batch) = &s.batch else { return };
    for (index, item) in batch.items.iter().enumerate() {
        if let Some(label) = s.statuses.get(&item.source.relative) {
            let (text, class) = item_status(item, s.progress.get(&index));
            set_status(label, &text, class);
        }
    }
    if s.busy == Busy::Importing {
        let videos = batch.items.iter().filter(|i| i.source.editing).count();
        let done = batch
            .items
            .iter()
            .filter(|i| i.source.editing && i.complete())
            .count();
        let speed = s
            .progress
            .values()
            .filter(|p| p.stage.contains("original") || p.stage.contains("local"))
            .filter(|p| p.began.elapsed().as_secs_f64() > 1.0)
            .map(|p| (p.bytes.saturating_sub(p.first)) as f64 / p.began.elapsed().as_secs_f64())
            .sum::<f64>();
        let mut text = format!("Importing · {done} of {videos} done");
        if speed > 0.0 {
            text.push_str(&format!(" · {}/s", human_bytes(speed as u64)));
        }
        p.summary.set_text(&text);
        p.progress.set_visible(true);
        p.progress.set_fraction(batch.verified_fraction());
    }
}

/// Rebuilds the clip list.
fn fill(p: &Page) {
    while let Some(row) = p.list.first_child() {
        p.list.remove(&row);
    }
    let mut s = p.state.borrow_mut();
    s.pictures.clear();
    s.statuses.clear();
    s.checks.clear();
    s.keeps.clear();
    let source = s.source.clone().unwrap_or_default();
    let days: Vec<(chrono::NaiveDate, Vec<Candidate>)> = by_day(&s.candidates)
        .into_iter()
        .map(|(d, v)| (d, v.into_iter().cloned().collect()))
        .collect();
    for (day, list) in days {
        let bytes: u64 = list.iter().map(|c| c.identity.size).sum();
        let new = list
            .iter()
            .filter(|c| !s.imported.contains_key(&c.relative))
            .count();
        let heading = label(
            &format!(
                "{} · {} {} · {}{}",
                day.format("%a %-d %b %Y"),
                list.len(),
                if list.len() == 1 { "clip" } else { "clips" },
                human_bytes(bytes),
                if new < list.len() {
                    format!(" · {} already on the NAS", list.len() - new)
                } else {
                    String::new()
                }
            ),
            "heading",
        );
        heading.set_margin_top(8);
        heading.set_margin_bottom(4);
        heading.set_margin_start(8);
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_child(Some(&heading));
        p.list.append(&row);
        for c in list {
            let row = clip_row(p, &mut s, &source, &c);
            p.list.append(&row);
        }
    }
}

fn clip_row(p: &Page, s: &mut State, source: &Path, c: &Candidate) -> gtk::ListBoxRow {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    line.set_margin_top(4);
    line.set_margin_bottom(4);
    line.set_margin_start(8);
    line.set_margin_end(8);
    let check = gtk::CheckButton::new();
    let on_nas = s.imported.get(&c.relative).cloned();
    check.set_active(s.ticked.contains(&c.relative));
    check.set_sensitive(on_nas.is_none());
    let name = c
        .relative
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    check.update_property(&[gtk::accessible::Property::Label(&format!("Import {name}"))]);
    line.append(&check);
    let picture = gtk::Picture::new();
    picture.set_size_request(160, 90);
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.add_css_class("thumb");
    if let Some(png) = s.thumbs.get(&c.relative) {
        picture.set_filename(Some(png));
    }
    line.append(&picture);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let title = label(&name, "");
    title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    text.append(&title);
    let duration = c
        .probe
        .as_ref()
        .map(|pr| pr.duration())
        .filter(|d| *d > 0.0)
        .map(|d| format!("{}:{:02}", d as u64 / 60, d as u64 % 60))
        .unwrap_or_default();
    let detail = format!(
        "{} · {duration} · {}",
        recorded(c).format("%H:%M"),
        human_bytes(c.identity.size)
    );
    text.append(&label(&detail, "dim-label"));
    line.append(&text);
    let status = label("", "status");
    status.set_valign(gtk::Align::Center);
    match &on_nas {
        Some(original) => {
            set_status(&status, "On the NAS", "success");
            status.set_tooltip_text(Some(&original.display().to_string()));
        }
        None => set_status(&status, "New", ""),
    }
    line.append(&status);
    let keep = gtk::ToggleButton::with_label("Keep here");
    keep.add_css_class("flat");
    keep.set_valign(gtk::Align::Center);
    keep.set_tooltip_text(Some(
        "Keep this clip's local copy after the import, with a proxy, to edit now",
    ));
    keep.update_property(&[gtk::accessible::Property::Label(&format!(
        "Keep {name} here to edit"
    ))]);
    line.append(&keep);
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_child(Some(&line));
    row.set_tooltip_text(Some(&source.join(&c.relative).display().to_string()));
    let relative = c.relative.clone();
    let page = p.clone();
    check.connect_toggled(move |check| {
        let mut s = page.state.borrow_mut();
        if check.is_active() {
            s.ticked.insert(relative.clone());
        } else {
            s.ticked.remove(&relative);
        }
        drop(s);
        refresh(&page);
    });
    s.pictures.insert(c.relative.clone(), picture);
    s.statuses.insert(c.relative.clone(), status);
    let relative = c.relative.clone();
    let page = p.clone();
    keep.connect_toggled(move |keep| {
        let mut s = page.state.borrow_mut();
        if s.keep_all {
            return;
        }
        if keep.is_active() {
            s.keep.insert(relative.clone());
        } else {
            s.keep.remove(&relative);
        }
        drop(s);
        refresh(&page);
    });
    s.checks.insert(c.relative.clone(), check);
    s.keeps.insert(c.relative.clone(), keep);
    row
}

/// Shows each row's "Keep here" as the state has it: only for clips being
/// imported, and all on while "Keep all" is.
fn sync_keeps(p: &Page) {
    let s = p.state.borrow();
    let idle = s.busy == Busy::No;
    let kept = s.kept();
    let toggles: Vec<(gtk::ToggleButton, bool, bool)> = s
        .keeps
        .iter()
        .map(|(r, b)| {
            let ticked = s.ticked.contains(r) && !s.imported.contains_key(r);
            (b.clone(), ticked, kept.contains(r))
        })
        .collect();
    let keep_all = s.keep_all;
    drop(s);
    for (b, ticked, on) in toggles {
        b.set_visible(ticked);
        b.set_active(on);
        b.set_sensitive(idle && !keep_all);
    }
}

fn set_status(label: &gtk::Label, text: &str, class: &str) {
    label.set_text(text);
    for c in ["success", "warning", "error"] {
        label.remove_css_class(c);
    }
    if !class.is_empty() {
        label.add_css_class(class);
    }
}

/// Everything but the clip rows, from the state.
fn refresh(p: &Page) {
    let settings = Settings::load().unwrap_or_default();
    let configured = settings.archive.is_some() && settings.editing.is_some();
    p.where_label.set_markup(&format!(
        "Originals go to <b>{}</b> · local copies to <b>{}</b>, removed once \
         the originals are verified unless kept here",
        glib::markup_escape_text(
            &settings
                .archive
                .as_ref()
                .map_or("(not set)".into(), |d| d.path.display().to_string())
        ),
        glib::markup_escape_text(
            &settings
                .editing
                .as_ref()
                .map_or("(not set)".into(), |d| d.path.display().to_string())
        ),
    ));
    p.setup.set_visible(!configured);

    let s = p.state.borrow();
    // The cards, and a folder instead.
    while let Some(w) = p.sources.first_child() {
        p.sources.remove(&w);
    }
    let mut first: Option<gtk::ToggleButton> = None;
    let idle = s.busy == Busy::No;
    for card in &s.cards {
        let b = gtk::ToggleButton::with_label(&format!("Card: {}", card.name));
        b.set_tooltip_text(Some(&card.path.display().to_string()));
        b.set_active(s.source.as_ref() == Some(&card.path));
        b.set_sensitive(idle);
        if let Some(f) = &first {
            b.set_group(Some(f));
        } else {
            first = Some(b.clone());
        }
        let (page, path) = (p.clone(), card.path.clone());
        b.connect_clicked(move |b| {
            if b.is_active() && page.state.borrow().source.as_ref() != Some(&path) {
                scan(&page, path.clone());
            }
        });
        p.sources.append(&b);
    }
    let folder_listed = s
        .source
        .as_ref()
        .filter(|src| !s.cards.iter().any(|c| &c.path == *src));
    if let Some(folder) = folder_listed {
        let l = label(&format!("Folder: {}", folder.display()), "");
        l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        p.sources.append(&l);
    }
    let pick = gtk::Button::with_label("From a folder…");
    pick.set_sensitive(idle);
    let page = p.clone();
    pick.connect_clicked(move |_| choose_folder(&page));
    p.sources.append(&pick);
    if let Some(src) = s.source.clone() {
        let rescan = gtk::Button::from_icon_name("view-refresh-symbolic");
        rescan.set_tooltip_text(Some("Read the card again"));
        rescan.set_sensitive(idle);
        let page = p.clone();
        rescan.connect_clicked(move |_| scan(&page, src.clone()));
        p.sources.append(&rescan);
    }

    let listing = s.source.is_some();
    p.empty.set_visible(!listing);
    p.list_area.set_visible(listing);
    match &s.result {
        Some((text, class)) => {
            p.result_text.set_text(text);
            for c in ["success", "warning", "error"] {
                p.result_text.remove_css_class(c);
            }
            p.result_text.add_css_class(class);
            p.result.set_visible(true);
        }
        None => p.result.set_visible(false),
    }
    let finished = s.batch.is_some() && idle;
    p.show_library.set_visible(finished);
    p.open_nas.set_visible(finished);
    p.retry.set_visible(
        finished
            && s.batch
                .as_ref()
                .is_some_and(|b| b.items.iter().any(|i| !i.complete())),
    );

    let ticked: Vec<&Candidate> = clips(&s.candidates)
        .filter(|c| s.ticked.contains(&c.relative))
        .collect();
    let bytes: u64 = ticked.iter().map(|c| c.identity.size).sum();
    for (relative, check) in &s.checks {
        check.set_sensitive(idle && !s.imported.contains_key(relative));
    }
    p.stop.set_visible(!idle);
    p.keep_all.set_visible(listing && !ticked.is_empty());
    p.keep_all.set_sensitive(idle);
    p.progress.set_visible(s.busy == Busy::Importing);
    p.import
        .set_sensitive(idle && configured && !ticked.is_empty());
    p.import.set_label(&match ticked.len() {
        0 => "Import".to_owned(),
        1 => "Import 1 clip".to_owned(),
        n => format!("Import {n} clips"),
    });
    if idle {
        let total = clips(&s.candidates).count();
        let on_nas = clips(&s.candidates)
            .filter(|c| s.imported.contains_key(&c.relative))
            .count();
        p.summary.set_text(&if !listing {
            String::new()
        } else if total == 0 {
            "No clips on this card.".to_owned()
        } else {
            let kept = s.kept().len();
            format!(
                "{total} {} · {on_nas} already on the NAS · {} ticked ({}){}",
                if total == 1 { "clip" } else { "clips" },
                ticked.len(),
                human_bytes(bytes),
                if kept > 0 && !s.keep_all {
                    format!(" · {kept} kept here")
                } else {
                    String::new()
                }
            )
        });
    }
    drop(s);
    sync_keeps(p);
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
    use ingest::model::Identity;
    use ingest::settings::Destination;

    /// A whole import as the page runs it: originals verified on the "NAS",
    /// previews made from the local copies, the kept clip's copy and proxy
    /// left, the other's removed.
    #[test]
    fn an_import_makes_previews_and_keeps_only_the_local_copies_asked_for() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("card/DCIM/100MEDIA");
        for folder in [&source, &d.path().join("nas"), &d.path().join("edit")] {
            std::fs::create_dir_all(folder).unwrap();
        }
        for name in [
            "DJI_20261124093012_0001_D.MP4",
            "DJI_20261124094500_0002_D.MP4",
        ] {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=size=320x180:rate=30:duration=1")
                .args(["-f", "lavfi", "-i", "sine=duration=1"])
                .args(["-c:v", "libx264", "-c:a", "aac", "-shortest"])
                .arg(source.join(name))
                .status()
                .unwrap();
            assert!(ok.success());
        }
        let card = d.path().join("card");
        let settings = Settings {
            source: card.clone(),
            archive: Some(Destination::select(d.path().join("nas"), false).unwrap()),
            editing: Some(Destination::select(d.path().join("edit"), false).unwrap()),
            workflow: Workflow::Ingest,
            ..Default::default()
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cancel = Cancel::default();
        let batch = rt.block_on(async {
            let candidates = discovery::scan(&card, &cancel).await.unwrap();
            let mut batch = pipeline::plan(settings, candidates, &cancel).await.unwrap();
            let mut jobs = Jobs::open(&d.path().join("jobs")).unwrap();
            let (tx, rx) = mpsc::sync_channel(64);
            std::thread::spawn(move || while rx.recv().is_ok() {});
            pipeline::run(&mut jobs, &mut batch, None, &cancel, &tx)
                .await
                .unwrap();
            batch
        });
        assert!(batch.safe_to_wipe(), "{}", batch.summary());
        let roots = Roots {
            catalog: d.path().join("catalog"),
            ..Roots::from_import(&batch.settings)
        };
        assert!(roots.nas_online);
        let kept_rel = PathBuf::from("DCIM/100MEDIA/DJI_20261124093012_0001_D.MP4");
        let (replies, _rx) = mpsc::channel();
        let settled = settle_batch(
            &batch,
            &roots,
            &[kept_rel.clone()].into(),
            &cancel,
            &replies,
        );
        assert_eq!(
            (settled.kept, settled.removed),
            (1, 1),
            "{:?}",
            settled.failed
        );
        assert!(settled.freed > 0);
        assert_eq!(settled.originals.len(), 2);
        for item in &batch.items {
            let original = d.path().join("nas").join(&item.archive_path);
            assert!(original.is_file());
            let size = item.source.identity.size;
            assert!(library::preview::preview_path(&roots.catalog, &original, size).is_file());
            let local = roots.local_copy(&original).unwrap();
            let kept = item.source.relative == kept_rel;
            assert_eq!(local.is_file(), kept, "{}", local.display());
            assert_eq!(library::proxy::proxy_path(&local).is_file(), kept);
        }
        let (text, _) = result_text(&batch, &settled);
        assert!(
            text.contains("1 local copy kept here to edit, 1 removed"),
            "{text}"
        );
    }

    fn candidate(relative: &str, editing: bool, sidecar_for: Option<&str>) -> Candidate {
        Candidate {
            relative: relative.into(),
            identity: Identity {
                dev: 0,
                ino: 0,
                size: 10,
                mtime: 100,
                mtime_ns: 0,
                ctime: 0,
                ctime_ns: 0,
            },
            probe: None,
            probe_error: None,
            selected: true,
            editing,
            sidecar_for: sidecar_for.map(PathBuf::from),
        }
    }

    #[test]
    fn a_clip_is_imported_while_its_journaled_original_is_there() {
        let cands = [
            candidate("DCIM/A.MP4", true, None),
            candidate("DCIM/B.MP4", true, None),
        ];
        let archived = vec![Archived {
            original: "/nas/2026/2026-08-26/A.MP4".into(),
            editing: None,
            verified_at: String::new(),
            source_relative: "DCIM/A.MP4".into(),
            source_size: 10,
            source_mtime: 100,
        }];
        let found = imported(&cands, &archived, |_| true);
        assert_eq!(
            found.get(Path::new("DCIM/A.MP4")),
            Some(&PathBuf::from("/nas/2026/2026-08-26/A.MP4"))
        );
        assert!(!found.contains_key(Path::new("DCIM/B.MP4")));
        // Deleted from the NAS since: not imported.
        assert!(imported(&cands, &archived, |_| false).is_empty());
        // A different file at the same path on another card: not imported.
        let mut other = cands.clone();
        other[0].identity.size = 11;
        assert!(imported(&other, &archived, |_| true).is_empty());
    }

    #[test]
    fn ticked_clips_take_their_sidecars_and_nothing_else() {
        let cands = [
            candidate("DCIM/A.MP4", true, None),
            candidate("DCIM/A.LRF", false, Some("DCIM/A.MP4")),
            candidate("DCIM/B.MP4", true, None),
            candidate("DCIM/B.SRT", false, Some("DCIM/B.MP4")),
            candidate("MISC/x.db", false, None),
        ];
        let ticked: HashSet<PathBuf> = [PathBuf::from("DCIM/A.MP4")].into();
        let picked: Vec<String> = selection(&cands, &ticked)
            .iter()
            .filter(|c| c.selected)
            .map(|c| c.relative.display().to_string())
            .collect();
        assert_eq!(picked, ["DCIM/A.MP4", "DCIM/A.LRF"]);
        let listed: Vec<String> = clips(&cands)
            .map(|c| c.relative.display().to_string())
            .collect();
        assert_eq!(listed, ["DCIM/A.MP4", "DCIM/B.MP4"]);
    }

    #[test]
    fn clips_are_grouped_by_recording_day() {
        let cands = [
            candidate("DCIM/DJI_20260827070000_0002_D.MP4", true, None),
            candidate("DCIM/DJI_20260826184226_0001_D.MP4", true, None),
            candidate("DCIM/DJI_20260826190000_0003_D.MP4", true, None),
        ];
        let days: Vec<(String, usize)> = by_day(&cands)
            .iter()
            .map(|(d, v)| (d.to_string(), v.len()))
            .collect();
        assert_eq!(days, [("2026-08-26".into(), 2), ("2026-08-27".into(), 1)]);
    }
}
