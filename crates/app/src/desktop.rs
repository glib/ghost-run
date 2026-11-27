//! Showing a folder in the file manager. Inside a Distrobox the box usually
//! has no file manager, and GTK's launcher fails quietly there, so the folder
//! is opened on the host (`distrobox-host-exec xdg-open`); the box shares the
//! host's home and media paths.

use std::path::Path;
use std::process::{Command, Stdio};

use gtk::gio;
use gtk::prelude::*;

/// Opens `folder` in the file manager; `failed` gets the reason if it can't.
pub fn open_folder(
    window: &impl IsA<gtk::Window>,
    folder: &Path,
    failed: impl Fn(String) + 'static,
) {
    if !folder.is_dir() {
        failed(format!(
            "{} isn't there (is the share mounted?)",
            folder.display()
        ));
        return;
    }
    let in_box = Path::new("/run/.containerenv").exists()
        && crate::resolve::which("distrobox-host-exec").is_some();
    if in_box {
        let spawned = Command::new("distrobox-host-exec")
            .arg("xdg-open")
            .arg(folder)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            // Reap it in the background so it doesn't linger as a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(e) => failed(format!("couldn't open the folder: {e}")),
        }
        return;
    }
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(folder)));
    launcher.launch(Some(window), gio::Cancellable::NONE, move |r| {
        if let Err(e) = r {
            failed(format!("couldn't open the folder: {e}"));
        }
    });
}

/// Opens a web page in the browser, on the host when inside a Distrobox.
/// Safe to call off the main thread.
pub fn open_url(url: &str) -> std::io::Result<()> {
    let in_box = Path::new("/run/.containerenv").exists()
        && crate::resolve::which("distrobox-host-exec").is_some();
    let mut command = if in_box {
        let mut c = Command::new("distrobox-host-exec");
        c.arg("xdg-open");
        c
    } else {
        Command::new("xdg-open")
    };
    let mut child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}
