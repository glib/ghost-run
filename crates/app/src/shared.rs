//! What the pages share: the app's settings, and news of changes to them or
//! to the run database, so each page can bring itself up to date.

use std::cell::{Ref, RefCell};
use std::rc::Rc;

use library::settings::AppSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The settings changed (and were saved).
    Settings,
    /// A page wrote the run database.
    Runs,
    /// A clip was picked to be timed ([`Shared::time_clip`]).
    TimeClip,
    /// Runs were picked to render ([`Shared::render_runs`]).
    Render,
    /// Runs were picked to go into Resolve ([`Shared::send_to_resolve`]).
    Resolve,
    /// Show the Library page (after an import).
    ShowLibrary,
    /// An import finished ([`Shared::imported`]): the Library rescans and
    /// places markers in the new clips.
    Imported,
    /// A video was added, changed or built ([`Shared::videos_changed`]).
    Videos,
    /// A video was picked to build in Resolve ([`Shared::build_video`]).
    BuildVideo,
    /// Show the Videos page on a video ([`Shared::show_video`]).
    ShowVideo,
    /// Strava was synced or disconnected (`crate::strava_ui`).
    Strava,
    /// An update was found, installed or failed (`crate::updates`).
    Update,
}

/// Where runs sent to Resolve go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveTarget {
    /// Each overlay above its clip on the open timeline.
    Timeline,
    /// A new project with the runs back to back.
    NewProject,
}

type Listener = Rc<dyn Fn(Change)>;
/// Runs for Resolve and where they go.
type ToResolve = Option<(ResolveTarget, Vec<String>)>;

#[derive(Clone)]
pub struct Shared {
    pub window: adw::ApplicationWindow,
    settings: Rc<RefCell<AppSettings>>,
    /// Indexed by subscription id; `None` once unsubscribed.
    listeners: Rc<RefCell<Vec<Option<Listener>>>>,
    /// Why the settings couldn't be read at start, if they couldn't.
    pub load_error: Option<String>,
    /// The clip picked to be timed, for the Time page to take.
    to_time: Rc<RefCell<Option<std::path::PathBuf>>>,
    /// Runs picked to render, for the jobs drawer to take.
    to_render: Rc<RefCell<Vec<String>>>,
    /// Runs picked to go into Resolve, for the jobs drawer to take.
    to_resolve: Rc<RefCell<ToResolve>>,
    /// Originals just imported to the NAS, for the Library to take.
    imported: Rc<RefCell<Vec<std::path::PathBuf>>>,
    /// The video to build, for the jobs drawer to take.
    to_build: Rc<RefCell<Option<i64>>>,
    /// The video to show, for the Videos page to take.
    to_show: Rc<RefCell<Option<i64>>>,
}

impl Shared {
    pub fn new(window: &adw::ApplicationWindow) -> Self {
        let (settings, load_error) = match AppSettings::load() {
            Ok(s) => (s, None),
            Err(e) => (
                AppSettings::default(),
                Some(format!("Settings could not be read: {e:#}")),
            ),
        };
        Self {
            window: window.clone(),
            settings: Rc::new(RefCell::new(settings)),
            listeners: Rc::default(),
            load_error,
            to_time: Rc::default(),
            to_render: Rc::default(),
            to_resolve: Rc::default(),
            imported: Rc::default(),
            to_build: Rc::default(),
            to_show: Rc::default(),
        }
    }

    pub fn settings(&self) -> Ref<'_, AppSettings> {
        self.settings.borrow()
    }

    /// Changes the settings, saves them and tells every page.
    pub fn update(&self, change: impl FnOnce(&mut AppSettings)) -> anyhow::Result<()> {
        change(&mut self.settings.borrow_mut());
        let saved = self.settings.borrow().save();
        self.notify(Change::Settings);
        saved
    }

    /// Calls `listener` after every change; returns an id for [`Self::unsubscribe`].
    pub fn subscribe(&self, listener: impl Fn(Change) + 'static) -> usize {
        let mut listeners = self.listeners.borrow_mut();
        listeners.push(Some(Rc::new(listener)));
        listeners.len() - 1
    }

    pub fn unsubscribe(&self, id: usize) {
        if let Some(slot) = self.listeners.borrow_mut().get_mut(id) {
            *slot = None;
        }
    }

    /// Opens `clip` on the Time page.
    pub fn time_clip(&self, clip: std::path::PathBuf) {
        *self.to_time.borrow_mut() = Some(clip);
        self.notify(Change::TimeClip);
    }

    /// The clip picked to be timed, once.
    pub fn take_clip_to_time(&self) -> Option<std::path::PathBuf> {
        self.to_time.borrow_mut().take()
    }

    /// Queues these runs' overlays in the jobs drawer.
    pub fn render_runs(&self, run_ids: Vec<String>) {
        self.to_render.borrow_mut().extend(run_ids);
        self.notify(Change::Render);
    }

    /// The runs picked to render, once.
    pub fn take_runs_to_render(&self) -> Vec<String> {
        std::mem::take(&mut self.to_render.borrow_mut())
    }

    /// Sends these runs to Resolve (through the jobs drawer, which renders
    /// any overlay that's missing or out of date first).
    pub fn send_to_resolve(&self, target: ResolveTarget, run_ids: Vec<String>) {
        *self.to_resolve.borrow_mut() = Some((target, run_ids));
        self.notify(Change::Resolve);
    }

    /// The runs picked to go into Resolve, once.
    pub fn take_runs_for_resolve(&self) -> ToResolve {
        self.to_resolve.borrow_mut().take()
    }

    /// Tells the pages a video changed in the catalog.
    pub fn videos_changed(&self) {
        self.notify(Change::Videos);
    }

    /// Builds this video in Resolve (in the jobs drawer: overlays rendered,
    /// clips copied down, then the project made).
    pub fn build_video(&self, id: i64) {
        *self.to_build.borrow_mut() = Some(id);
        self.notify(Change::BuildVideo);
    }

    /// The video picked to build, once.
    pub fn take_video_to_build(&self) -> Option<i64> {
        self.to_build.borrow_mut().take()
    }

    /// Opens the Videos page on this video.
    pub fn show_video(&self, id: i64) {
        *self.to_show.borrow_mut() = Some(id);
        self.notify(Change::ShowVideo);
    }

    /// The video to show, once.
    pub fn take_video_to_show(&self) -> Option<i64> {
        self.to_show.borrow_mut().take()
    }

    /// Tells the Library these originals were just imported to the NAS.
    pub fn imported(&self, originals: Vec<std::path::PathBuf>) {
        self.imported.borrow_mut().extend(originals);
        self.notify(Change::Imported);
    }

    /// The originals just imported, once.
    pub fn take_imported(&self) -> Vec<std::path::PathBuf> {
        std::mem::take(&mut self.imported.borrow_mut())
    }

    pub fn notify(&self, change: Change) {
        // A copy, so a listener may subscribe or unsubscribe while being called.
        let listeners: Vec<Listener> = self.listeners.borrow().iter().flatten().cloned().collect();
        for listener in listeners {
            listener(change);
        }
    }
}
