//! The window's frame (decision 0005's workspaces): a sidebar listing the
//! pages (Inbox, Library, Videos, Tracks), the page shown,
//! and the jobs drawer (renders and Resolve sends, `output_page`) over its
//! right edge, opened from the sidebar's Jobs row.

use adw::prelude::*;

/// Wraps the pages in a split view with a sidebar. `import` is the Import
/// page's advanced view (the old resolve-ingest window, with its own header
/// bar), opened over the new Import page.
pub fn shell(window: &adw::ApplicationWindow, import: gtk::Widget) -> adw::NavigationSplitView {
    let shared = crate::shared::Shared::new(window);
    crate::catalog_ui::keep_copy();
    let pages: [(&str, &str, &str, gtk::Widget); 4] = [
        (
            "import",
            "Inbox",
            "folder-download-symbolic",
            crate::import_page::page(&shared, import),
        ),
        (
            "library",
            "Library",
            "folder-videos-symbolic",
            crate::library_page::page(&shared),
        ),
        (
            "videos",
            "Videos",
            "media-playback-start-symbolic",
            crate::videos_page::page(&shared),
        ),
        (
            "runs",
            "Tracks",
            "view-list-ordered-symbolic",
            crate::runs_page::page(&shared),
        ),
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

    // The jobs drawer, and its row at the foot of the sidebar.
    let drawer = adw::OverlaySplitView::new();
    drawer.set_sidebar_position(gtk::PackType::End);
    drawer.set_collapsed(true);
    drawer.set_show_sidebar(false);
    drawer.set_max_sidebar_width(560.0);
    drawer.set_sidebar_width_fraction(0.5);
    // A release's banner over the pages (`crate::updates`).
    let pages_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    pages_box.append(&crate::updates::banner(&shared));
    stack.set_vexpand(true);
    pages_box.append(&stack);
    drawer.set_content(Some(&pages_box));
    let activity = gtk::Label::new(None);
    activity.add_css_class("dim-label");
    activity.add_css_class("caption");
    let d = drawer.clone();
    drawer.set_sidebar(Some(&crate::output_page::page(
        &shared,
        activity.clone(),
        move || d.set_show_sidebar(false),
    )));
    let jobs_list = gtk::ListBox::new();
    jobs_list.add_css_class("navigation-sidebar");
    jobs_list.set_selection_mode(gtk::SelectionMode::None);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.append(&gtk::Image::from_icon_name("document-export-symbolic"));
    let jobs_label = gtk::Label::new(Some("Jobs"));
    jobs_label.set_hexpand(true);
    jobs_label.set_xalign(0.0);
    row.append(&jobs_label);
    row.append(&activity);
    let jobs_row = gtk::ListBoxRow::new();
    jobs_row.set_child(Some(&row));
    jobs_row.set_tooltip_text(Some("Overlay renders and sends to Resolve"));
    jobs_row.update_property(&[gtk::accessible::Property::Label("Jobs")]);
    jobs_list.append(&jobs_row);
    let d = drawer.clone();
    jobs_list.connect_row_activated(move |_, _| d.set_show_sidebar(!d.shows_sidebar()));
    let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.set_vexpand(true);
    side.append(&list);
    side.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    side.append(&jobs_list);
    sidebar.set_content(Some(&side));

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&adw::NavigationPage::new(&sidebar, "Ghost Run")));
    let content = adw::NavigationPage::new(&drawer, "Library");
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
    // A clip picked to be timed opens over the Library (library_page), a
    // video shown the Videos page; runs or videos sent to render or to
    // Resolve open the drawer.
    let (list2, drawer2) = (list.clone(), drawer.clone());
    shared.subscribe(move |change| match change {
        crate::shared::Change::TimeClip | crate::shared::Change::ShowLibrary => {
            select(&list2, "library");
        }
        crate::shared::Change::ShowVideo => {
            select(&list2, "videos");
        }
        crate::shared::Change::Render
        | crate::shared::Change::Resolve
        | crate::shared::Change::BuildVideo => {
            drawer2.set_show_sidebar(true);
        }
        _ => {}
    });
    // GHOST_RUN_PAGE=runs (etc.) opens on that page, `jobs` on the Library
    // with the drawer open, `settings` with the Settings dialog: for
    // screenshots and testing. Otherwise the Library.
    let start = std::env::var("GHOST_RUN_PAGE").unwrap_or_default();
    if start == "jobs" {
        drawer.set_show_sidebar(true);
    }
    if !select(&list, &start) {
        select(&list, "library");
    }
    // GHOST_RUN_PAGE=settings opens the Settings dialog over the Library.
    if start == "settings" {
        let shared = shared.clone();
        gtk::glib::idle_add_local_once(move || crate::settings_dialog::open(&shared));
    }
    crate::updates::start(&shared);
    // GHOST_RUN_CLIP=<path> opens that clip to time: for screenshots.
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
