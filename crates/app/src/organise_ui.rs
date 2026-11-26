//! The Library's organising controls: a small button-with-menu, and the
//! dialogs for tags, collections and names (`library::organise`).

use std::collections::BTreeSet;

use adw::prelude::*;

use crate::shared::Shared;

/// A button that opens a menu of `items` (label, tooltip). Returns the
/// button and the items, so callers can connect and show or hide them; each
/// item closes the menu when clicked.
pub fn menu_button(title: &str, items: &[(&str, &str)]) -> (gtk::Button, Vec<gtk::Button>) {
    let button = gtk::Button::new();
    button.add_css_class("ingest-action");
    let face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    face.append(&gtk::Label::new(Some(title)));
    face.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    button.set_child(Some(&face));
    button.update_property(&[gtk::accessible::Property::Label(title)]);
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let popover = gtk::Popover::new();
    let buttons: Vec<gtk::Button> = items
        .iter()
        .map(|(label, tip)| {
            let b = gtk::Button::with_label(label);
            b.add_css_class("flat");
            if !tip.is_empty() {
                b.set_tooltip_text(Some(tip));
            }
            if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.);
            }
            let p = popover.clone();
            b.connect_clicked(move |_| p.popdown());
            menu.append(&b);
            b
        })
        .collect();
    popover.set_child(Some(&menu));
    popover.set_parent(&button);
    let shown = popover.clone();
    button.connect_clicked(move |_| shown.popup());
    let parented = popover.clone();
    button.connect_destroy(move |_| parented.unparent());
    (button, buttons)
}

/// How many of the ticked clips have each tag: every tag in use, with
/// whether all, some or none of them have it.
#[derive(Debug, Clone, PartialEq)]
pub struct TagState {
    pub tag: String,
    pub all: bool,
    pub some: bool,
}

/// Asks which tags the ticked clips should have: each tag in use as a check
/// box (on if all have it, mixed if some do), and a box for new ones (comma
/// separated). `done` gets the tags to add and to remove.
pub fn tags_dialog(
    shared: &Shared,
    clips: usize,
    tags: Vec<TagState>,
    done: impl Fn(Vec<String>, Vec<String>) + 'static,
) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_max_children_per_line(3);
    let mut checks = Vec::new();
    for t in &tags {
        let check = gtk::CheckButton::with_label(&t.tag);
        check.set_active(t.all);
        check.set_inconsistent(t.some && !t.all);
        check.connect_toggled(|c| c.set_inconsistent(false));
        flow.insert(&check, -1);
        checks.push((t.clone(), check));
    }
    if !tags.is_empty() {
        let scroll = gtk::ScrolledWindow::builder()
            .child(&flow)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(240)
            .propagate_natural_height(true)
            .build();
        form.append(&scroll);
    }
    let entry = gtk::Entry::builder()
        .placeholder_text("New tags, e.g. gear: Fox helmet, people: Sam, wet")
        .activates_default(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("New tags")]);
    form.append(&entry);
    let entry2 = entry.clone();
    crate::courses::ask(
        shared,
        "Tags",
        &format!(
            "For the {clips} ticked {}. A \"kind: \" in front (gear, people, conditions) groups tags in the panel.",
            if clips == 1 { "clip" } else { "clips" }
        ),
        "Apply",
        Some(form.upcast_ref()),
        Some(entry.upcast_ref()),
        move || {
            let mut add: Vec<String> = new_tags(&entry2.text());
            let mut remove = Vec::new();
            for (t, check) in &checks {
                if check.is_inconsistent() {
                    continue;
                }
                match (check.is_active(), t.all) {
                    (true, false) => add.push(t.tag.clone()),
                    (false, _) if t.some => remove.push(t.tag.clone()),
                    _ => {}
                }
            }
            done(add, remove);
        },
    );
}

/// The tags typed in a box: comma separated, trimmed, no duplicates.
pub fn new_tags(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    text.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .filter(|t| seen.insert(t.to_lowercase()))
        .map(str::to_owned)
        .collect()
}

/// Asks which collection to add the ticked clips to: one of `names`, or a
/// new one typed in. `done` gets the name.
pub fn collection_dialog(shared: &Shared, names: Vec<String>, done: impl Fn(String) + 'static) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let mut choices: Vec<&str> = vec!["New collection"];
    choices.extend(names.iter().map(String::as_str));
    let pick = gtk::DropDown::from_strings(&choices);
    pick.update_property(&[gtk::accessible::Property::Label("Collection")]);
    let entry = gtk::Entry::builder()
        .placeholder_text("Name, e.g. Race build-up")
        .activates_default(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("New collection's name")]);
    if !names.is_empty() {
        pick.set_selected(1);
        entry.set_visible(false);
        form.append(&pick);
    }
    form.append(&entry);
    let e = entry.clone();
    pick.connect_selected_notify(move |p| e.set_visible(p.selected() == 0));
    let (pick2, entry2) = (pick.clone(), entry.clone());
    crate::courses::ask(
        shared,
        "Add to a collection",
        "Collections gather clips from any ride, like a race build-up.",
        "Add",
        Some(form.upcast_ref()),
        Some(entry.upcast_ref()),
        move || {
            let name = match pick2.selected() {
                0 => entry2.text().trim().to_owned(),
                i => names[i as usize - 1].clone(),
            };
            if !name.is_empty() {
                done(name);
            }
        },
    );
}

/// Asks which video to add the ticked clips to: one of `videos` (id,
/// title: the unpublished ones, newest first, the first picked), or a new
/// one with a title typed in. `done`
/// gets the video's id, or none and the new title.
pub fn video_dialog(
    shared: &Shared,
    clips: usize,
    videos: Vec<(i64, String)>,
    done: impl Fn(Option<i64>, String) + 'static,
) {
    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let mut choices: Vec<&str> = vec!["New video"];
    choices.extend(videos.iter().map(|(_, t)| t.as_str()));
    let pick = gtk::DropDown::from_strings(&choices);
    pick.update_property(&[gtk::accessible::Property::Label("Video")]);
    let entry = gtk::Entry::builder()
        .placeholder_text("Title, e.g. Ridge laps, November")
        .activates_default(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("New video's title")]);
    // The newest unfinished video first: usually the one being put together.
    if !videos.is_empty() {
        pick.set_selected(1);
        entry.set_visible(false);
        form.append(&pick);
    }
    form.append(&entry);
    let e = entry.clone();
    pick.connect_selected_notify(move |p| e.set_visible(p.selected() == 0));
    let (pick2, entry2) = (pick.clone(), entry.clone());
    crate::courses::ask(
        shared,
        "Add to a video",
        &format!(
            "The {clips} ticked {} go on the end, whole and in the order recorded. Timed runs get their timer; switch it off on the Videos page.",
            if clips == 1 { "clip" } else { "clips" }
        ),
        "Add",
        Some(form.upcast_ref()),
        Some(entry.upcast_ref()),
        move || match pick2.selected() {
            0 => {
                let title = entry2.text().trim().to_owned();
                if !title.is_empty() {
                    done(None, title);
                }
            }
            i => {
                let (id, title) = videos[i as usize - 1].clone();
                done(Some(id), title);
            }
        },
    );
}

/// Asks for a new name (an empty one is allowed when `clearable`, meaning
/// "go back to the default"). With `delete`, a Delete button too.
pub fn name_dialog(
    shared: &Shared,
    heading: &str,
    body: &str,
    current: &str,
    delete: Option<Box<dyn Fn()>>,
    done: impl Fn(String) + 'static,
) {
    let entry = gtk::Entry::builder()
        .text(current)
        .activates_default(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Name")]);
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "Cancel")]);
    if delete.is_some() {
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    }
    dialog.add_response("ok", "Rename");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_extra_child(Some(&entry));
    let entry2 = entry.clone();
    dialog.connect_response(None, move |_, r| match r {
        "ok" => done(entry2.text().trim().to_owned()),
        "delete" => {
            if let Some(d) = &delete {
                d();
            }
        }
        _ => {}
    });
    dialog.present(Some(&shared.window));
    dialog.set_focus(Some(&entry));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_tags_are_split_trimmed_and_deduplicated() {
        assert_eq!(
            new_tags(" gear: Fox helmet, wet,, Wet , people: Sam "),
            ["gear: Fox helmet", "wet", "people: Sam"]
        );
        assert!(new_tags(" , ").is_empty());
    }
}
