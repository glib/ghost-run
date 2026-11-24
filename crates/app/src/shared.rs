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
}

type Listener = Rc<dyn Fn(Change)>;

#[derive(Clone)]
pub struct Shared {
    pub window: adw::ApplicationWindow,
    settings: Rc<RefCell<AppSettings>>,
    /// Indexed by subscription id; `None` once unsubscribed.
    listeners: Rc<RefCell<Vec<Option<Listener>>>>,
    /// Why the settings couldn't be read at start, if they couldn't.
    pub load_error: Option<String>,
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

    pub fn notify(&self, change: Change) {
        // A copy, so a listener may subscribe or unsubscribe while being called.
        let listeners: Vec<Listener> = self.listeners.borrow().iter().flatten().cloned().collect();
        for listener in listeners {
            listener(change);
        }
    }
}
