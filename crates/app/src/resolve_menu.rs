//! The "Resolve ▾" button the Library and Tracks pages share: it sends
//! the ticked runs to DaVinci Resolve, either onto the open timeline or into
//! a new project (see [`crate::shared::Shared::send_to_resolve`]).

use adw::prelude::*;

use crate::shared::ResolveTarget;

/// A button with a menu of the two targets.
#[derive(Clone)]
pub struct ResolveMenu {
    pub button: gtk::Button,
    popover: gtk::Popover,
    items: [(gtk::Button, ResolveTarget); 2],
}

pub fn menu() -> ResolveMenu {
    let to_resolve = gtk::Button::new();
    to_resolve.add_css_class("ingest-action");
    let face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    face.append(&gtk::Label::new(Some("Resolve")));
    face.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    to_resolve.set_child(Some(&face));
    to_resolve.update_property(&[gtk::accessible::Property::Label("Send to Resolve")]);
    to_resolve.set_tooltip_text(Some(
        "Put these runs in DaVinci Resolve, rendering any overlay that's missing or out of date first",
    ));
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let to_timeline = gtk::Button::with_label("Add to open timeline");
    to_timeline.set_tooltip_text(Some(
        "Put each overlay on the track above its clip in Resolve's current timeline, adding clips that aren't there yet at its end",
    ));
    let to_project = gtk::Button::with_label("New project…");
    to_project.set_tooltip_text(Some(
        "Start a Resolve project with these runs back to back: clips on V1, overlays on V2",
    ));
    for b in [&to_timeline, &to_project] {
        b.add_css_class("flat");
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.);
        }
        menu.append(b);
    }
    let popover = gtk::Popover::new();
    popover.set_child(Some(&menu));
    popover.set_parent(&to_resolve);
    let shown = popover.clone();
    to_resolve.connect_clicked(move |_| shown.popup());
    let parented = popover.clone();
    to_resolve.connect_destroy(move |_| parented.unparent());
    ResolveMenu {
        button: to_resolve,
        popover,
        items: [
            (to_timeline, ResolveTarget::Timeline),
            (to_project, ResolveTarget::NewProject),
        ],
    }
}

impl ResolveMenu {
    /// Calls `pick` with the target chosen from the menu.
    pub fn on_pick(&self, pick: impl Fn(ResolveTarget) + Clone + 'static) {
        for (b, target) in &self.items {
            let (pick, popover, target) = (pick.clone(), self.popover.clone(), *target);
            b.connect_clicked(move |_| {
                popover.popdown();
                pick(target);
            });
        }
    }
}
