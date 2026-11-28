//! Resolve's projects for the app (decision 0008): read from Resolve's own
//! database (`library::resolve_db`) on the shared catalog's server, so the
//! Videos page can list every project and timeline with how much of its
//! footage is here, keep the videos linked to timelines up to date with what
//! they use, and so removing local copies never takes away footage a project
//! still being edited uses.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use library::Library;
use library::footage::Roots;
use library::resolve_db::{self, Footage, Matcher, Project};
use library::videos::{Status, Use};

/// A project saved within this long counts as being edited: removing local
/// copies leaves its footage alone.
pub const EDITING_DAYS: i64 = 60;

/// Why Resolve's projects can't be listed, when that's expected (no shared
/// catalog set up) rather than a failure.
pub const NOT_SET_UP: &str = "Connect the shared catalog (Settings → Shared catalog) to see Resolve's projects here: they're read from Resolve's database on the same server.";

/// When a project or timeline was last saved, for a list row.
pub fn when(secs: Option<i64>) -> String {
    secs.and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

/// Every project in Resolve's database, or why not. `Ok(None)`: no shared
/// catalog is set up, so there's nowhere to read them from.
pub fn load(database: Option<&str>) -> Result<Option<Vec<Project>>> {
    let Some(cfg) = library::shared::Config::load()? else {
        return Ok(None);
    };
    let cfg = resolve_db::pick(&cfg, database)?;
    resolve_db::read(&cfg)
        .map(Some)
        .with_context(|| format!("reading Resolve's projects from {}", cfg.describe()))
}

/// A timeline as the Videos page lists it.
#[derive(Debug, Clone)]
pub struct Row {
    pub project: String,
    pub timeline: String,
    pub modified: Option<i64>,
    pub footage: Footage,
    /// The video linked to it, if any.
    pub video: Option<i64>,
    /// The files it uses that aren't here, each with what Ghost Run knows.
    pub away: Vec<String>,
}

/// What the Videos page shows of Resolve.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub projects: Vec<Project>,
    pub rows: Vec<Row>,
    /// Videos whose usage was brought up to date from their timeline.
    pub updated: usize,
}

/// Reads Resolve's projects, lists their timelines (those with files) with
/// their footage, and records what each linked video's timeline uses now.
pub fn rows(database: Option<&str>, roots: &Roots) -> Result<Option<Loaded>> {
    let Some(projects) = load(database)? else {
        return Ok(None);
    };
    let library = Library::open_default()?;
    let clips = library.clips()?;
    let videos = library.videos()?;
    let usage = library.usage()?;
    let m = Matcher::new(&clips, roots);
    let exists = |p: &Path| p.is_file();
    let mut out = Loaded::default();
    for p in &projects {
        for t in &p.timelines {
            if t.items.is_empty() {
                continue;
            }
            let video = videos.iter().find(|v| {
                v.project.as_deref() == Some(p.name.as_str())
                    && v.timeline.as_deref() == Some(t.name.as_str())
            });
            if let Some(v) = video {
                let uses: Vec<Use> = t
                    .on_timeline()
                    .iter()
                    .filter_map(|i| {
                        Some(Use {
                            video: v.id,
                            clip: library::organise::key(m.find(&i.path)?),
                            first_frame: i.first_frame,
                            frames: i.frames,
                        })
                    })
                    .collect();
                let had = usage.get(&v.id).cloned().unwrap_or_default();
                // Never wipe what a video was built with for a timeline that
                // reads as empty.
                if !uses.is_empty() && !same_uses(&had, &uses) && library.read_only().is_none() {
                    library.set_usage(v.id, "timeline", &uses)?;
                    out.updated += 1;
                }
            }
            let away = t
                .files()
                .into_iter()
                .filter(|f| !exists(f))
                .take(12)
                .map(|f| {
                    let name = f.file_name().unwrap_or_default().to_string_lossy();
                    match m.find(f) {
                        Some(c) if roots.on_nas(&c.path) => format!("{name}: on the NAS"),
                        Some(_) => format!("{name}: in the Library, somewhere else"),
                        None => format!("{name}: not in the Library ({})", f.display()),
                    }
                })
                .collect();
            out.rows.push(Row {
                project: p.name.clone(),
                timeline: t.name.clone(),
                modified: t.modified.or(p.modified),
                footage: resolve_db::footage(t, &m, &exists),
                video: video.map(|v| v.id),
                away,
            });
        }
    }
    out.rows.sort_by_key(|r| std::cmp::Reverse(r.modified));
    out.projects = projects;
    Ok(Some(out))
}

fn same_uses(a: &[Use], b: &[Use]) -> bool {
    let key = |u: &Use| (u.clip.clone(), u.first_frame, u.frames);
    let mut a: Vec<_> = a.iter().map(key).collect();
    let mut b: Vec<_> = b.iter().map(key).collect();
    a.sort();
    b.sort();
    a == b
}

/// Takes out of `clips` the ones a Resolve project saved in the last
/// [`EDITING_DAYS`] uses (published videos' timelines aside), and says which
/// and why. Without a shared catalog there's nothing to check: all pass.
pub fn guard(clips: Vec<library::Clip>, roots: &Roots) -> (Vec<library::Clip>, String) {
    let database = library::settings::AppSettings::load()
        .ok()
        .and_then(|s| s.resolve_database);
    let projects = match load(database.as_deref()) {
        Ok(Some(p)) => p,
        Ok(None) => return (clips, String::new()),
        Err(e) => {
            return (
                clips,
                format!(" (Resolve's projects couldn't be checked for these clips: {e:#})"),
            );
        }
    };
    let published: HashSet<(String, String)> = Library::open_default()
        .and_then(|l| l.videos())
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v.status == Status::Published)
        .filter_map(|v| Some((v.project?, v.timeline?)))
        .collect();
    let all = Library::open_default()
        .and_then(|l| l.clips())
        .unwrap_or_default();
    let m = Matcher::new(&all, roots);
    let since = now_secs() - EDITING_DAYS * 86_400;
    let used = resolve_db::in_use(&projects, &m, since, &published);
    keep_used(clips, &used)
}

fn keep_used(
    clips: Vec<library::Clip>,
    used: &HashMap<String, Vec<String>>,
) -> (Vec<library::Clip>, String) {
    let mut kept_by: Vec<String> = Vec::new();
    let mut kept = 0;
    let free: Vec<library::Clip> = clips
        .into_iter()
        .filter(|c| match used.get(&library::organise::key(c)) {
            Some(projects) => {
                kept += 1;
                for p in projects {
                    if !kept_by.contains(p) {
                        kept_by.push(p.clone());
                    }
                }
                false
            }
            None => true,
        })
        .collect();
    let note = match kept {
        0 => String::new(),
        n => format!(
            " Kept {n} {} that Resolve projects saved in the last {EDITING_DAYS} days use ({}).",
            if n == 1 { "copy" } else { "copies" },
            kept_by.join(", ")
        ),
    };
    (free, note)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_clips_a_project_uses_and_names_it() {
        let clip = |name: &str| library::Clip {
            path: format!("/nas/{name}").into(),
            size: 1,
            ..Default::default()
        };
        let (a, b) = (clip("a.mp4"), clip("b.mp4"));
        let used = HashMap::from([(
            library::organise::key(&a),
            vec!["Lower A-line".to_owned(), "Pingu".to_owned()],
        )]);
        let (free, note) = keep_used(vec![a, b.clone()], &used);
        assert_eq!(free, vec![b.clone()]);
        assert_eq!(
            note,
            " Kept 1 copy that Resolve projects saved in the last 60 days use (Lower A-line, Pingu)."
        );
        assert_eq!(keep_used(vec![b], &used).1, "");
    }

    #[test]
    fn usage_compares_without_order() {
        let u = |clip: &str, f| Use {
            video: 1,
            clip: clip.into(),
            first_frame: f,
            frames: 10,
        };
        assert!(same_uses(&[u("a", 1), u("b", 2)], &[u("b", 2), u("a", 1)]));
        assert!(!same_uses(&[u("a", 1)], &[u("a", 2)]));
    }
}
