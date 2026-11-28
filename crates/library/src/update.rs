//! Updates from Ghost Run's GitHub releases (`docs/releasing.md`): each
//! release carries one tarball built in the same Fedora as the distrobox the
//! app runs in, and a `SHA256SUMS`. [`check`] asks GitHub for the latest
//! release, [`download`] fetches and checks it and unpacks it into a staging
//! folder, [`try_run`] makes sure the new binary starts on this machine (a
//! box on an older Fedora lacks the libraries it was built against), and
//! [`install`] swaps it in beside the running one. A running binary can be
//! replaced on Linux: the new one starts at the next launch.
//!
//! The requests carry nothing but the app's name and version.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where releases are asked for (`GHOST_RUN_UPDATES` replaces it, for tests
/// and trying it out).
pub const GITHUB_API: &str = "https://api.github.com";
/// The repository releases come from.
pub const REPO: &str = "glib/ghost-run";
/// The checksums asset.
pub const SUMS: &str = "SHA256SUMS";
/// The binaries a release replaces, in `bin/` of the tarball and beside the
/// running app.
pub const BINARIES: [&str; 2] = ["ghost-run", "rtimer"];
/// The launcher's icon, under `share/` of the tarball and the install prefix.
pub const ICON: &str = "icons/hicolor/scalable/apps/io.github.glib.GhostRun.svg";
/// The most a download may be: a release is about 30 MB.
const MAX_DOWNLOAD: u64 = 500 << 20;

/// A release newer than this build.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: Version,
    /// The release notes (Markdown, from the commits it holds).
    pub notes: String,
    /// The release's page.
    pub page: String,
    /// The tarball for this machine.
    pub tarball: Asset,
    pub sums: Asset,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

/// The tarball's name for `version` on this machine.
pub fn tarball_name(version: &Version) -> String {
    format!(
        "ghost-run-{version}-{}-linux.tar.gz",
        std::env::consts::ARCH
    )
}

/// GitHub, or `GHOST_RUN_UPDATES`.
pub fn base() -> String {
    std::env::var("GHOST_RUN_UPDATES").unwrap_or_else(|_| GITHUB_API.to_owned())
}

fn agent(current: &Version) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .user_agent(&format!("ghost-run/{current}"))
        .build()
}

/// This build's version: `GHOST_RUN_VERSION` as baked in at build time
/// (`crates/app/build.rs`). Unparseable counts as 0.0.0, so any release is newer.
pub fn parse_version(text: &str) -> Version {
    Version::parse(text.trim().trim_start_matches('v')).unwrap_or(Version::new(0, 0, 0))
}

/// Whether `current` was built from a release (no `+N.gHASH` from
/// `git describe`): only those update without asking, so a build of your own
/// branch is never replaced behind your back.
pub fn is_release_build(current: &Version) -> bool {
    current.build.is_empty() && current.pre.is_empty()
}

/// The latest release, if it's newer than `current` and has a tarball for
/// this machine.
pub fn check(base: &str, current: &Version) -> Result<Option<Release>> {
    let url = format!("{base}/repos/{REPO}/releases/latest");
    let reply = agent(current)
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call();
    let release: GithubRelease = match reply {
        Ok(r) => r.into_json().context("reading GitHub's answer")?,
        // No release yet.
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(ureq::Error::Status(code, r)) => {
            bail!(
                "GitHub said {code}: {}",
                r.into_string().unwrap_or_default().trim()
            )
        }
        Err(e) => return Err(e).context("couldn't reach GitHub"),
    };
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let Ok(version) = Version::parse(release.tag_name.trim_start_matches('v')) else {
        return Ok(None);
    };
    // Build metadata doesn't order versions; compare without it.
    let mut plain = current.clone();
    plain.build = semver::BuildMetadata::EMPTY;
    if version <= plain {
        return Ok(None);
    }
    let find = |name: &str| release.assets.iter().find(|a| a.name == name).cloned();
    let (Some(tarball), Some(sums)) = (find(&tarball_name(&version)), find(SUMS)) else {
        return Ok(None);
    };
    Ok(Some(Release {
        version,
        notes: release.body.unwrap_or_default(),
        page: release.html_url,
        tarball,
        sums,
    }))
}

/// Downloads `release`, checks it against its `SHA256SUMS` and unpacks it
/// into `<staging>/<version>/` (emptied first); returns that folder.
pub fn download(release: &Release, current: &Version, staging: &Path) -> Result<PathBuf> {
    let agent = agent(current);
    let sums = agent
        .get(&release.sums.url)
        .call()
        .context("downloading SHA256SUMS")?
        .into_string()?;
    let want = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim().trim_start_matches('*') == release.tarball.name)
        .map(|(hash, _)| hash.to_ascii_lowercase())
        .with_context(|| format!("SHA256SUMS doesn't list {}", release.tarball.name))?;

    fs::create_dir_all(staging)?;
    let tarball = staging.join(&release.tarball.name);
    let reply = agent
        .get(&release.tarball.url)
        .call()
        .with_context(|| format!("downloading {}", release.tarball.name))?;
    let mut reader = reply.into_reader().take(MAX_DOWNLOAD + 1);
    let mut file = fs::File::create(&tarball)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(total <= MAX_DOWNLOAD, "the download is too big");
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
    }
    file.sync_all()?;
    let got = format!("{:x}", hasher.finalize());
    if got != want {
        let _ = fs::remove_file(&tarball);
        bail!("{} doesn't match its checksum", release.tarball.name);
    }

    let dir = staging.join(release.version.to_string());
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&dir)
        .arg("--strip-components=1")
        .arg("--no-same-owner")
        .output()
        .context("running tar")?;
    let _ = fs::remove_file(&tarball);
    ensure!(
        out.status.success(),
        "couldn't unpack {}: {}",
        release.tarball.name,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    for name in BINARIES {
        ensure!(
            dir.join("bin").join(name).is_file(),
            "the release has no {name}"
        );
    }
    Ok(dir)
}

/// Runs the staged `ghost-run --version` and checks it says `version`: it
/// won't start if this machine lacks the libraries it was built against.
pub fn try_run(staged: &Path, version: &Version) -> Result<()> {
    let out = Command::new(staged.join("bin/ghost-run"))
        .arg("--version")
        .output()
        .context("couldn't start the new version")?;
    let said = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        bail!(
            "the new version doesn't start here (built for a newer Fedora than this box?): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let said = said.split_whitespace().last().unwrap_or_default();
    ensure!(
        parse_version(said) == *version,
        "the new version says it's {said}, not {version}"
    );
    Ok(())
}

/// Release notes (`scripts/release.py`'s Markdown) as plain text for a
/// dialog: headings end in a colon, bullets are •, bold marks go.
pub fn plain_notes(markdown: &str) -> String {
    markdown
        .lines()
        .map(|line| {
            let line = line.replace("**", "");
            if let Some(h) = line.trim_start_matches('#').strip_prefix(' ')
                && line.starts_with('#')
            {
                format!("{h}:")
            } else if let Some(item) = line.strip_prefix("- ") {
                format!("• {item}")
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .replace("\n\n•", "\n•")
        .trim()
        .to_owned()
}

/// Where this binary was installed (`<prefix>/bin`), if it can be updated
/// there: not a build under a Cargo `target` folder, and a folder we can write.
pub fn install_dir(exe: &Path) -> Option<PathBuf> {
    if exe.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    let dir = exe.parent()?;
    if dir.file_name()? != "bin" {
        return None;
    }
    let probe = dir.join(".ghost-run-write-test");
    let writable = fs::File::create(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    writable.then(|| dir.to_path_buf())
}

/// Puts the staged binaries into `bin` (each written beside its old one, then
/// renamed over it, so a failure leaves the old one whole) and the icon into
/// `bin/../share`. The launcher is left alone: `make install` wrote it for
/// this machine's box.
pub fn install(staged: &Path, bin: &Path) -> Result<()> {
    for name in BINARIES {
        let new = bin.join(format!(".{name}.new"));
        fs::copy(staged.join("bin").join(name), &new)
            .with_context(|| format!("copying {name} into {}", bin.display()))?;
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755))?;
        fs::File::open(&new)?.sync_all()?;
        fs::rename(&new, bin.join(name)).with_context(|| format!("replacing {name}"))?;
    }
    let icon = staged.join("share").join(ICON);
    if let Some(prefix) = bin.parent()
        && icon.is_file()
    {
        let to = prefix.join("share").join(ICON);
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::copy(&icon, &to)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    /// A stand-in GitHub: `route` answers each path with a status and body.
    fn fake(route: impl Fn(&str) -> (u16, Vec<u8>) + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                }
                let (status, body) = route(&path);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        base
    }

    fn release_json(base: &str, tag: &str) -> Vec<u8> {
        let v = tag.trim_start_matches('v');
        let name = tarball_name(&Version::parse(v).unwrap());
        serde_json::json!({
            "tag_name": tag,
            "body": "- feat: a thing",
            "html_url": format!("{base}/page"),
            "assets": [
                {"name": name, "browser_download_url": format!("{base}/dl/{name}")},
                {"name": "SHA256SUMS", "browser_download_url": format!("{base}/dl/SHA256SUMS")},
            ],
        })
        .to_string()
        .into_bytes()
    }

    /// A release tarball whose `ghost-run --version` says `version`.
    fn tarball(version: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(format!("ghost-run-{version}"));
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("share").join(ICON).parent().unwrap()).unwrap();
        for name in BINARIES {
            let path = root.join("bin").join(name);
            fs::write(&path, format!("#!/bin/sh\necho \"{name} {version}\"\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(root.join("share").join(ICON), "<svg/>").unwrap();
        let out = dir.path().join("t.tar.gz");
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&out)
            .arg("-C")
            .arg(dir.path())
            .arg(format!("ghost-run-{version}"))
            .status()
            .unwrap();
        assert!(ok.success());
        fs::read(out).unwrap()
    }

    fn serve(tag: &'static str, tar: Vec<u8>, sum: Option<String>) -> String {
        let sum = sum.unwrap_or_else(|| format!("{:x}", Sha256::digest(&tar)));
        let name = tarball_name(&Version::parse(tag.trim_start_matches('v')).unwrap());
        let base_cell = std::sync::Arc::new(std::sync::OnceLock::<String>::new());
        let b = base_cell.clone();
        let base = fake(move |path| {
            let base = b.get().unwrap();
            if path == format!("/repos/{REPO}/releases/latest") {
                (200, release_json(base, tag))
            } else if path == "/dl/SHA256SUMS" {
                (200, format!("{sum}  {name}\n").into_bytes())
            } else if path == format!("/dl/{name}") {
                (200, tar.clone())
            } else {
                (404, Vec::new())
            }
        });
        base_cell.set(base.clone()).unwrap();
        base
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Version::new(1, 2, 3));
        assert_eq!(parse_version("junk"), Version::new(0, 0, 0));
        assert!(is_release_build(&parse_version("1.2.3")));
        assert!(!is_release_build(&parse_version("1.2.3+4.gabc123")));
    }

    #[test]
    fn offers_only_newer_releases() {
        let base = serve("v1.3.0", Vec::new(), Some("0".into()));
        let r = check(&base, &parse_version("1.2.9")).unwrap().unwrap();
        assert_eq!(r.version, Version::new(1, 3, 0));
        assert_eq!(r.notes, "- feat: a thing");
        assert!(r.tarball.url.ends_with(&tarball_name(&r.version)));
        // A build of a later commit than 1.3.0 is still 1.3.0.
        assert_eq!(check(&base, &parse_version("1.3.0+2.gabc")).unwrap(), None);
        assert_eq!(check(&base, &parse_version("1.3.0")).unwrap(), None);
        assert_eq!(check(&base, &parse_version("2.0.0")).unwrap(), None);
    }

    #[test]
    fn no_release_yet() {
        let base = fake(|_| (404, b"{\"message\":\"Not Found\"}".to_vec()));
        assert_eq!(check(&base, &parse_version("1.0.0")).unwrap(), None);
    }

    #[test]
    fn downloads_checks_and_installs() {
        let base = serve("v1.3.0", tarball("1.3.0"), None);
        let current = parse_version("1.2.0");
        let release = check(&base, &current).unwrap().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let staged = download(&release, &current, &tmp.path().join("staging")).unwrap();
        try_run(&staged, &release.version).unwrap();
        assert!(try_run(&staged, &Version::new(9, 0, 0)).is_err());

        let bin = tmp.path().join("prefix/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("ghost-run"), "old").unwrap();
        assert_eq!(install_dir(&bin.join("ghost-run")), Some(bin.clone()));
        install(&staged, &bin).unwrap();
        let new = fs::read_to_string(bin.join("ghost-run")).unwrap();
        assert!(new.contains("ghost-run 1.3.0"));
        assert!(bin.join("rtimer").is_file());
        let mode = fs::metadata(bin.join("ghost-run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(tmp.path().join("prefix/share").join(ICON).is_file());
        assert!(!bin.join(".ghost-run.new").exists());
    }

    #[test]
    fn refuses_a_bad_checksum() {
        let base = serve("v1.3.0", tarball("1.3.0"), Some("00".repeat(32)));
        let current = parse_version("1.2.0");
        let release = check(&base, &current).unwrap().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let e = download(&release, &current, tmp.path()).unwrap_err();
        assert!(e.to_string().contains("checksum"), "{e:#}");
        assert!(!tmp.path().join(&release.tarball.name).exists());
    }

    #[test]
    fn notes_as_plain_text() {
        let md = "### New\n\n- **updates:** updates itself (#70)\n- more\n\n### Fixes\n\n- a fix\n";
        assert_eq!(
            plain_notes(md),
            "New:\n• updates: updates itself (#70)\n• more\n\nFixes:\n• a fix"
        );
    }

    #[test]
    fn only_installed_binaries_update() {
        assert_eq!(install_dir(Path::new("/x/target/release/ghost-run")), None);
        assert_eq!(install_dir(Path::new("/nonexistent/bin/ghost-run")), None);
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(install_dir(&tmp.path().join("ghost-run")), None);
    }
}
