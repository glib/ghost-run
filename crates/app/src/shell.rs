//! The window's frame: a sidebar listing the pages, and the page shown.

use adw::prelude::*;

/// Wraps the pages in a split view with a sidebar. `import` is the Import
/// page (the old resolve-ingest window, with its own header bar).
pub fn shell(window: &adw::ApplicationWindow, import: gtk::Widget) -> adw::NavigationSplitView {
    let shared = crate::shared::Shared::new(window);
    let pages: [(&str, &str, &str, gtk::Widget); 5] = [
        (
            "library",
            "Library",
            "folder-videos-symbolic",
            crate::library_page::page(&shared),
        ),
        (
            "time",
            "Time",
            "alarm-symbolic",
            crate::time_page::page(&shared),
        ),
        (
            "runs",
            "Runs",
            "view-list-ordered-symbolic",
            crate::runs_page::page(&shared),
        ),
        (
            "output",
            "Output",
            "document-export-symbolic",
            crate::output_page::page(&shared),
        ),
        ("import", "Import", "document-save-symbolic", import),
    ];
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    for (name, title, icon, widget) in pages {
        stack.add_titled(&widget, Some(name), title);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(&gtk::Image::from_icon_name(icon));
        row.append(&gtk::Label::new(Some(title)));
        let item = gtk::ListBoxRow::new();
        item.set_child(Some(&row));
        item.set_widget_name(name);
        item.update_property(&[gtk::accessible::Property::Label(title)]);
        list.append(&item);
    }

    let sidebar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Ghost Run", "")));
    let settings = gtk::Button::from_icon_name("emblem-system-symbolic");
    settings.set_tooltip_text(Some(
        "Settings: footage folders, run database, Resolve template",
    ));
    let shared2 = shared.clone();
    settings.connect_clicked(move |_| crate::settings_dialog::open(&shared2));
    header.pack_end(&settings);
    sidebar.add_top_bar(&header);
    sidebar.set_content(Some(&list));

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&adw::NavigationPage::new(&sidebar, "Ghost Run")));
    let content = adw::NavigationPage::new(&stack, "Library");
    split.set_content(Some(&content));
    split.set_min_sidebar_width(170.0);
    split.set_max_sidebar_width(220.0);

    let (stack2, content2, split2) = (stack.clone(), content.clone(), split.clone());
    list.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        let name = row.widget_name();
        stack2.set_visible_child_name(&name);
        if let Some(page) = stack2.child_by_name(&name) {
            content2.set_title(&stack2.page(&page).title().unwrap_or_default());
        }
        split2.set_show_content(true);
    });
    // A clip picked to be timed opens the Time page.
    let list2 = list.clone();
    shared.subscribe(move |change| match change {
        crate::shared::Change::TimeClip => {
            select(&list2, "time");
        }
        crate::shared::Change::Render => {
            select(&list2, "output");
        }
        _ => {}
    });
    // GHOST_RUN_PAGE=runs (etc.) opens on that page: for screenshots and testing.
    let start = std::env::var("GHOST_RUN_PAGE").unwrap_or_default();
    if !select(&list, &start) {
        list.select_row(list.row_at_index(0).as_ref());
    }
    // GHOST_RUN_CLIP=<path> opens that clip on the Time page: for screenshots.
    if let Some(clip) = std::env::var_os("GHOST_RUN_CLIP") {
        shared.time_clip(clip.into());
    }
    split
}

/// Shows the page called `name`; false if there is none.
fn select(list: &gtk::ListBox, name: &str) -> bool {
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        if row.widget_name() == name {
            list.select_row(Some(&row));
            return true;
        }
        index += 1;
    }
    false
}
