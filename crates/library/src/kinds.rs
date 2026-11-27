//! Video kinds (decision 0005): a kind is a TOML template saying what a video
//! is made from (one run, the runs on a course, or the clips with a tag, in a
//! collection, a ride or a feature's attempts) and its sections in order:
//! title and end cards, the runs (whole clips or their timed part, timer on,
//! off or optional), the clips (in the order recorded, with a caption and a
//! card for each ride), a chart of the course's personal bests. [`plan`]
//! turns a kind and its input into a video's title and items; the video is
//! then an ordinary one, edited and built like any other.
//!
//! Ghost Run carries neutral examples (`kinds/` in the repo, built in);
//! a folder of your own replaces them by id or adds more ([`all`]).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use timer_core::board::{RunStatus, course_board};
use timer_core::database::TimerDatabase;
use timer_core::models::RunRecord;
use timer_core::tracks;

use crate::features::Outcome;
use crate::organise::{Catalog, Flag, key};
use crate::store::Clip;
use crate::videos::{Card, CardKind, Item};

const BUILT_IN: [&str; 6] = [
    include_str!("../../../kinds/trail.toml"),
    include_str!("../../../kinds/speed-progression.toml"),
    include_str!("../../../kinds/ridelog.toml"),
    include_str!("../../../kinds/gear-review.toml"),
    include_str!("../../../kinds/story.toml"),
    include_str!("../../../kinds/feature-progression.toml"),
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Kind {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input: Input,
    /// The video's title.
    pub title: String,
    #[serde(rename = "section")]
    pub sections: Vec<Section>,
}

/// What a kind is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    /// One run, by its id.
    Run,
    /// A course's runs, by the course's id.
    Course,
    /// The clips with a tag.
    Tag,
    /// A collection's clips, by its id.
    Collection,
    /// A ride's clips, by its id (`crate::organise::Ride`).
    Ride,
    /// A feature's attempts, by its id (`crate::features`).
    Feature,
}

impl Input {
    /// Whether it's made from runs in the run database, not the catalog.
    pub fn from_runs(self) -> bool {
        matches!(self, Input::Run | Input::Course)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Section {
    Title(CardSection),
    End(CardSection),
    Chart(CardSection),
    Runs(RunsSection),
    Clips(ClipsSection),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CardSection {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default = "three")]
    pub seconds: f64,
}

fn three() -> f64 {
    3.0
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RunsSection {
    #[serde(default)]
    pub pick: Pick,
    #[serde(default)]
    pub part: Part,
    #[serde(default)]
    pub lead_in: f64,
    #[serde(default)]
    pub lead_out: f64,
    #[serde(default)]
    pub timer: Timer,
}

/// The input's clips, in the order recorded, rejects left out.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClipsSection {
    /// For clips that are stored runs.
    #[serde(default)]
    pub timer: Timer,
    /// Shown low on each clip; empty for none. Parts that come out empty
    /// between " · " are dropped, so "Attempt {attempt} · {outcome}" reads
    /// "Attempt 2" when the outcome isn't said.
    #[serde(default)]
    pub caption: String,
    /// A title card before each ride's clips; empty for none.
    #[serde(default)]
    pub ride_title: String,
    #[serde(default)]
    pub ride_subtitle: String,
    #[serde(default = "three")]
    pub ride_seconds: f64,
}

/// Which of the input's runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pick {
    /// The run given, or every counted run on the course in the order ridden.
    #[default]
    All,
    /// The first counted run, each new personal best, the latest.
    Progression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Part {
    #[default]
    Whole,
    /// From the Start marker less `lead_in` seconds to Finish plus `lead_out`.
    Timed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Timer {
    On,
    Off,
    /// Off to start with; switched on per clip.
    #[default]
    Optional,
}

pub fn parse(text: &str) -> Result<Kind> {
    let kind: Kind = toml::from_str(text)?;
    anyhow::ensure!(!kind.id.trim().is_empty(), "a kind needs an id");
    anyhow::ensure!(!kind.sections.is_empty(), "{} has no sections", kind.id);
    Ok(kind)
}

/// The kinds Ghost Run carries.
pub fn built_in() -> Vec<Kind> {
    BUILT_IN
        .iter()
        .map(|t| parse(t).expect("the built-in kinds parse"))
        .collect()
}

/// The built-in kinds with `folder`'s `*.toml` replacing them by id or added
/// after; and a line for each file that couldn't be read.
pub fn all(folder: Option<&Path>) -> (Vec<Kind>, Vec<String>) {
    let mut kinds = built_in();
    let mut errors = Vec::new();
    let Some(folder) = folder else {
        return (kinds, errors);
    };
    let mut files: Vec<_> = match std::fs::read_dir(folder) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect(),
        Err(e) => {
            errors.push(format!("{}: {e}", folder.display()));
            return (kinds, errors);
        }
    };
    files.sort();
    for file in files {
        let read = std::fs::read_to_string(&file)
            .map_err(anyhow::Error::from)
            .and_then(|t| parse(&t));
        match read {
            Ok(kind) => match kinds.iter_mut().find(|k| k.id == kind.id) {
                Some(slot) => *slot = kind,
                None => kinds.push(kind),
            },
            Err(e) => errors.push(format!(
                "{}: {e:#}",
                file.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }
    (kinds, errors)
}

/// A kind's video: its title and items.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub title: String,
    pub items: Vec<Item>,
}

/// What kinds are made from.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub db: &'a TimerDatabase,
    pub clips: &'a [Clip],
    pub catalog: &'a Catalog,
    /// Strava's activities, for captions' stats (`crate::strava`).
    pub strava: Option<&'a crate::strava::Index>,
}

/// The video `kind` makes of `input` (the id of a run, course, collection,
/// ride or feature, or a tag): the runs' clips found in the Library by file
/// name, cards with their text filled in.
pub fn plan(kind: &Kind, input: &str, sources: Sources) -> Result<Plan> {
    if !kind.input.from_runs() {
        return plan_clips(kind, input, sources);
    }
    let (db, clips) = (sources.db, sources.clips);
    let (course_id, given) = match kind.input {
        Input::Run => {
            let run = db
                .runs
                .iter()
                .find(|r| r.id == input)
                .with_context(|| format!("run {input} isn't in the run database"))?;
            (run.course_id.clone(), Some(run))
        }
        _ => (input.to_owned(), None),
    };
    let course = db
        .course_by_id(&course_id)
        .with_context(|| format!("course {course_id} isn't in the run database"))?;
    let board = course_board(course, &db.runs);
    let laps: HashMap<&str, f64> = board
        .rows
        .iter()
        .filter_map(|r| Some((r.run_id.as_str(), r.lap_seconds?)))
        .collect();
    let history = tracks::history(&board, &db.runs);
    let by_id: HashMap<&str, &RunRecord> = db.runs.iter().map(|r| (r.id.as_str(), r)).collect();
    let counted: Vec<&RunRecord> = board
        .rows
        .iter()
        .filter(|r| r.status == RunStatus::Counted)
        .filter_map(|r| by_id.get(r.run_id.as_str()).copied())
        .collect();

    let pick = |p: Pick| -> Vec<&RunRecord> {
        match (given, p) {
            (Some(run), _) => vec![run],
            (None, Pick::All) => history
                .iter()
                .filter_map(|pt| by_id.get(pt.run_id.as_str()).copied())
                .collect(),
            (None, Pick::Progression) => progression(&history)
                .into_iter()
                .filter_map(|id| by_id.get(id).copied())
                .collect(),
        }
    };
    // What the text can say, from the runs the video shows.
    let shown: Vec<&RunRecord> = kind
        .sections
        .iter()
        .find_map(|s| match s {
            Section::Runs(r) => Some(pick(r.pick)),
            _ => None,
        })
        .unwrap_or_default();
    let best = counted
        .iter()
        .filter_map(|r| laps.get(r.id.as_str()))
        .copied()
        .fold(f64::INFINITY, f64::min);
    let first_lap = shown.first().and_then(|r| laps.get(r.id.as_str()).copied());
    let mut vars: HashMap<&str, String> = HashMap::new();
    vars.insert("course", course.name.clone());
    vars.insert("runs", shown.len().to_string());
    vars.insert("date", given.map(|r| r.date.clone()).unwrap_or_default());
    vars.insert(
        "lap",
        given
            .and_then(|r| laps.get(r.id.as_str()))
            .map_or(String::new(), |l| lap_text(*l)),
    );
    vars.insert(
        "best",
        if best.is_finite() {
            lap_text(best)
        } else {
            String::new()
        },
    );
    vars.insert(
        "first_date",
        shown.first().map(|r| r.date.clone()).unwrap_or_default(),
    );
    vars.insert(
        "latest_date",
        shown.last().map(|r| r.date.clone()).unwrap_or_default(),
    );
    vars.insert(
        "faster",
        match first_lap {
            Some(f) if best.is_finite() && f > best => format!("{:.1} s", f - best),
            _ => "0.0 s".to_owned(),
        },
    );
    let fill = |text: &str| fill(text, &vars);

    let mut items = Vec::new();
    for section in &kind.sections {
        let card = |kind: CardKind, c: &CardSection| {
            Item::card(Card {
                kind,
                text: fill(&c.text),
                subtitle: fill(&c.subtitle),
                seconds: c.seconds,
                course: (kind == CardKind::Chart).then(|| course.id.clone()),
            })
        };
        match section {
            Section::Title(c) => items.push(card(CardKind::Title, c)),
            Section::End(c) => items.push(card(CardKind::End, c)),
            Section::Chart(c) => items.push(card(CardKind::Chart, c)),
            Section::Runs(r) => {
                let runs = pick(r.pick);
                if runs.is_empty() {
                    bail!("{} has no counted runs to show", course.name);
                }
                for run in runs {
                    items.push(run_item(run, r, clips)?);
                }
            }
            Section::Clips(c) => {
                let runs = pick(Pick::All);
                if runs.is_empty() {
                    bail!("{} has no counted runs to show", course.name);
                }
                let section = RunsSection {
                    pick: Pick::All,
                    part: Part::Whole,
                    lead_in: 0.0,
                    lead_out: 0.0,
                    timer: c.timer,
                };
                for run in runs {
                    items.push(run_item(run, &section, clips)?);
                }
            }
        }
    }
    Ok(Plan {
        title: fill(&kind.title),
        items,
    })
}

/// A clip's day, from its recording time: `2026-11-24`.
fn day_of(clip: &Clip) -> String {
    clip.recorded
        .as_deref()
        .and_then(|r| r.get(..10))
        .unwrap_or_default()
        .to_owned()
}

/// A video of catalogued clips: a tag's, a collection's, a ride's or a
/// feature's attempts, in the order recorded, rejects left out.
fn plan_clips(kind: &Kind, input: &str, sources: Sources) -> Result<Plan> {
    let Sources {
        db,
        clips,
        catalog,
        strava,
    } = sources;
    let all_rides = catalog.rides(clips);
    let feature = match kind.input {
        Input::Feature => Some(
            catalog
                .features
                .iter()
                .find(|f| f.id.to_string() == input)
                .context("that feature isn't in the catalog any more")?,
        ),
        _ => None,
    };
    let (name, wanted): (String, HashSet<String>) = match kind.input {
        Input::Tag => (
            // "gear: Fox helmet" is called "Fox helmet".
            input
                .split_once(':')
                .map_or(input, |(_, n)| n)
                .trim()
                .to_owned(),
            catalog
                .tags
                .iter()
                .filter(|(_, tags)| tags.contains(input))
                .map(|(k, _)| k.clone())
                .collect(),
        ),
        Input::Collection => {
            let c = catalog
                .collections
                .iter()
                .find(|c| c.id.to_string() == input)
                .context("that collection isn't in the catalog any more")?;
            (c.name.clone(), c.clips.clone())
        }
        Input::Ride => {
            let r = all_rides
                .iter()
                .find(|r| r.id == input)
                .context("that ride isn't in the Library any more")?;
            (r.title(), r.clips.iter().cloned().collect())
        }
        Input::Feature => {
            let f = feature.expect("found above");
            (
                f.name.clone(),
                f.attempts.iter().map(|a| a.clip.clone()).collect(),
            )
        }
        Input::Run | Input::Course => unreachable!("planned from runs"),
    };
    let mut chosen: Vec<&Clip> = clips
        .iter()
        .filter(|c| {
            let k = key(c);
            wanted.contains(&k) && catalog.flags.get(&k) != Some(&Flag::Reject)
        })
        .collect();
    chosen.sort_by(|a, b| (&a.recorded, &a.path).cmp(&(&b.recorded, &b.path)));
    if chosen.is_empty() {
        bail!("{name} has no clips in the Library");
    }
    // Stored runs, by the clip name they were timed from.
    let runs: HashMap<String, &RunRecord> = db
        .runs
        .iter()
        .filter(|r| r.committed && !r.ignored)
        .map(|r| (run_name(&r.filename), r))
        .collect();
    let laps: HashMap<String, f64> = db
        .courses
        .iter()
        .flat_map(|c| course_board(c, &db.runs).rows)
        .filter_map(|r| Some((r.run_id, r.lap_seconds?)))
        .collect();
    let run_of = |c: &Clip| runs.get(&run_name(&c.path.to_string_lossy())).copied();
    let outcome_of = |c: &Clip| {
        feature
            .and_then(|f| f.attempt(&key(c)))
            .map_or(Outcome::Unknown, |a| a.outcome)
    };
    let days: Vec<String> = {
        let mut d: Vec<String> = chosen.iter().map(|c| day_of(c)).collect();
        d.dedup();
        d
    };
    let first_landed = chosen
        .iter()
        .position(|c| outcome_of(c) == Outcome::Landed)
        .map(|i| i + 1);
    let mut vars: HashMap<&str, String> = HashMap::new();
    vars.insert("name", name.clone());
    let named = match kind.input {
        Input::Tag => "tag",
        Input::Collection => "collection",
        Input::Ride => "ride",
        _ => "feature",
    };
    vars.insert(named, name.clone());
    vars.insert("clips", chosen.len().to_string());
    vars.insert("attempts", chosen.len().to_string());
    vars.insert("days", days.len().to_string());
    let counted =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    vars.insert("clips_text", counted(chosen.len(), "clip", "clips"));
    vars.insert(
        "attempts_text",
        counted(chosen.len(), "attempt", "attempts"),
    );
    vars.insert("days_text", counted(days.len(), "day", "days"));
    vars.insert("date", day_of(chosen[0]));
    vars.insert("first_date", day_of(chosen[0]));
    vars.insert("latest_date", day_of(chosen[chosen.len() - 1]));
    vars.insert(
        "landed",
        chosen
            .iter()
            .filter(|c| outcome_of(c) == Outcome::Landed)
            .count()
            .to_string(),
    );
    vars.insert(
        "result",
        match first_landed {
            Some(n) => format!("Landed on attempt {n}"),
            None => "Not landed yet".to_owned(),
        },
    );

    let mut items = Vec::new();
    for section in &kind.sections {
        let card = |kind: CardKind, c: &CardSection| {
            Item::card(Card {
                kind,
                text: fill(&c.text, &vars),
                subtitle: fill(&c.subtitle, &vars),
                seconds: c.seconds,
                course: None,
            })
        };
        match section {
            Section::Title(c) => items.push(card(CardKind::Title, c)),
            Section::End(c) => items.push(card(CardKind::End, c)),
            Section::Chart(_) => bail!("{}: a chart needs a course", kind.id),
            Section::Runs(_) => bail!(
                "{}: \"runs\" is for a run or a course; use \"clips\"",
                kind.id
            ),
            Section::Clips(section) => {
                let mut last_ride: Option<&str> = None;
                for (i, clip) in chosen.iter().enumerate() {
                    let k = key(clip);
                    let ride = all_rides.iter().find(|r| r.clips.contains(&k));
                    let run = run_of(clip);
                    let mut v = vars.clone();
                    v.insert("attempt", (i + 1).to_string());
                    let day = day_of(clip);
                    v.insert("date", day.clone());
                    v.insert(
                        "day",
                        (days.iter().position(|d| *d == day).unwrap_or(0) + 1).to_string(),
                    );
                    v.insert("outcome", outcome_of(clip).label().to_owned());
                    let span = strava.and_then(|s| {
                        let length = crate::videos::frames(clip) as f64
                            / clip.info.as_ref().map_or(30.0, |i| i.fps().max(1.0));
                        s.clip_span(clip, 0.0, length)
                    });
                    let stat = |f: &dyn Fn(&crate::strava::Span) -> Option<String>| {
                        span.as_ref().and_then(f).unwrap_or_default()
                    };
                    v.insert(
                        "distance",
                        stat(&|s| Some(format!("{:.1} km in", s.distance_km?))),
                    );
                    v.insert(
                        "top_speed",
                        stat(&|s| Some(format!("{:.0} km/h top", s.top_kmh?))),
                    );
                    v.insert(
                        "speed",
                        stat(&|s| Some(format!("{:.0} km/h", s.average_kmh?))),
                    );
                    v.insert("hr", stat(&|s| Some(format!("{:.0} bpm", s.average_bpm?))));
                    v.insert("climb", stat(&|s| Some(format!("{:.0} m up", s.climb_m?))));
                    v.insert("ride", ride.map(|r| r.title()).unwrap_or_default());
                    v.insert(
                        "course",
                        run.and_then(|r| db.course_by_id(&r.course_id).ok())
                            .map(|c| c.name.clone())
                            .unwrap_or_default(),
                    );
                    v.insert(
                        "lap",
                        run.and_then(|r| laps.get(&r.id))
                            .map_or(String::new(), |l| lap_text(*l)),
                    );
                    if !section.ride_title.is_empty()
                        && let Some(r) = ride
                        && last_ride != Some(r.id.as_str())
                    {
                        last_ride = Some(r.id.as_str());
                        items.push(Item::card(Card {
                            kind: CardKind::Title,
                            text: fill(&section.ride_title, &v),
                            subtitle: fill(&section.ride_subtitle, &v),
                            seconds: section.ride_seconds,
                            course: None,
                        }));
                    }
                    let caption = caption_text(&fill(&section.caption, &v));
                    items.push(Item {
                        timer: run
                            .filter(|_| section.timer == Timer::On)
                            .map(|r| r.id.clone()),
                        caption: (!caption.is_empty()).then_some(caption),
                        ..Item::whole(clip)
                    });
                }
            }
        }
    }
    Ok(Plan {
        title: fill(&kind.title, &vars),
        items,
    })
}

/// A filled-in caption with the parts that came out empty dropped.
fn caption_text(filled: &str) -> String {
    filled
        .split(" · ")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The first point, each new best, and the last: run ids in the order ridden.
pub fn progression(history: &[tracks::Point]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for (i, p) in history.iter().enumerate() {
        if (i == 0 || p.new_best || i + 1 == history.len()) && !out.contains(&p.run_id.as_str()) {
            out.push(&p.run_id);
        }
    }
    out
}

/// A run's clip, as the section wants it.
fn run_item(run: &RunRecord, section: &RunsSection, clips: &[Clip]) -> Result<Item> {
    let name = run_name(&run.filename);
    let clip = clips
        .iter()
        .find(|c| run.source_path.as_deref().map(Path::new) == Some(c.path.as_path()))
        .or_else(|| {
            clips
                .iter()
                .find(|c| run_name(&c.path.to_string_lossy()) == name)
        })
        .with_context(|| {
            format!(
                "{}, run {}'s clip, isn't in the Library",
                run.filename, run.id
            )
        })?;
    let mut item = Item::whole(clip);
    if section.part == Part::Timed {
        let fps = clip.info.as_ref().map_or(run.source_fps, |i| i.fps());
        let start = run.marker_frames.get("Start").copied().unwrap_or(0);
        let finish = run.marker_frames.values().copied().max().unwrap_or(start);
        let first = (start - (section.lead_in * fps).round() as i64).max(0);
        let mut last = finish + (section.lead_out * fps).round() as i64;
        if item.frames > 0 {
            last = last.min(item.frames);
        }
        item.first_frame = first;
        item.frames = (last - first).max(1);
    }
    item.timer = (section.timer == Timer::On).then(|| run.id.clone());
    Ok(item)
}

/// A clip's file name as runs know it: lower case, an editing copy's
/// (`clip.MP4.mov`) as its original's.
fn run_name(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    match name.strip_suffix(".mov") {
        Some(original) if original.contains('.') => original.to_owned(),
        _ => name,
    }
}

/// A lap to a tenth: `1:23.4`.
pub fn lap_text(seconds: f64) -> String {
    let tenths = (seconds * 10.0).round() as i64;
    format!("{}:{:02}.{}", tenths / 600, tenths / 10 % 60, tenths % 10)
}

/// `text` with each `{name}` replaced; unknown names are left as they are.
fn fill(text: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = text.to_owned();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use timer_core::models::Course;

    fn run(id: &str, date: &str, file: &str, finish: i64) -> RunRecord {
        let frames = [
            ("Start", 300),
            ("S1", 300 + finish / 2),
            ("Finish", 300 + finish),
        ]
        .iter()
        .map(|(n, f)| ((*n).to_owned(), *f))
        .collect();
        let mut r = RunRecord::new(id, "ridge", date, file, 30.0, frames);
        r.capture_time = Some(format!("{date}T10:00:00Z"));
        r
    }

    fn clip(name: &str, frames: i64) -> Clip {
        Clip {
            path: format!("/nas/{name}").into(),
            size: 1,
            info: Some(crate::MediaInfo {
                codec: "h264".into(),
                width: 1920,
                height: 1080,
                fps_num: 30,
                fps_den: 1,
                frames: Some(frames),
                duration_seconds: None,
                timecode: None,
                rotation: 0,
                creation_time: None,
                has_audio: true,
            }),
            ..Clip::default()
        }
    }

    fn db() -> TimerDatabase {
        TimerDatabase {
            courses: vec![Course {
                id: "ridge".into(),
                name: "Ridge".into(),
                sector_count: 2,
            }],
            runs: vec![
                run("a", "2026-06-01", "A.MP4", 3000),
                run("b", "2026-06-08", "B.MP4", 2700),
                run("c", "2026-06-15", "C.MP4", 2900),
                run("d", "2026-07-01", "D.MP4", 2400),
                run("e", "2026-07-08", "E.MP4", 2600),
            ],
        }
    }

    #[test]
    fn built_in_kinds_parse() {
        let kinds = built_in();
        assert_eq!(
            kinds.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(),
            [
                "trail",
                "speed-progression",
                "ridelog",
                "gear-review",
                "story",
                "feature-progression"
            ]
        );
        assert!(parse("id = \"x\"\nname = \"X\"\ninput = \"run\"\ntitle = \"\"").is_err());
    }

    #[test]
    fn a_folder_replaces_and_adds_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let mine = "id = \"trail\"\nname = \"My trail\"\ninput = \"run\"\ntitle = \"{course}\"\n[[section]]\ntype = \"runs\"\n";
        std::fs::write(dir.path().join("trail.toml"), mine).unwrap();
        std::fs::write(dir.path().join("broken.toml"), "id = ").unwrap();
        let (kinds, errors) = all(Some(dir.path()));
        assert_eq!(kinds[0].name, "My trail");
        assert_eq!(kinds.len(), built_in().len());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("broken.toml"));
    }

    #[test]
    fn a_trail_is_title_the_whole_run_and_an_end() {
        let clips = [clip("C.MP4", 4000), clip("D.MP4.mov", 4000)];
        let trail = &built_in()[0];
        let (db, catalog) = (db(), Catalog::default());
        let sources = Sources {
            db: &db,
            clips: &clips,
            catalog: &catalog,
            strava: None,
        };
        let plan = plan(trail, "c", sources).unwrap();
        assert_eq!(plan.title, "Ridge");
        assert_eq!(plan.items.len(), 3);
        let title = plan.items[0].card.as_ref().unwrap();
        assert_eq!(
            (title.text.as_str(), title.subtitle.as_str()),
            ("Ridge", "2026-06-15")
        );
        let ride = &plan.items[1];
        assert_eq!(
            (ride.first_frame, ride.frames, ride.timer.as_deref()),
            (0, 4000, None)
        );
        assert_eq!(plan.items[2].card.as_ref().unwrap().text, "1:36.7");
        assert!(super::plan(trail, "zzz", sources).is_err());
        // Its clip must be in the Library.
        assert!(super::plan(trail, "a", sources).is_err());
    }

    #[test]
    fn a_speed_progression_is_first_each_best_latest_then_a_chart() {
        let clips: Vec<Clip> = ["A", "B", "C", "D", "E"]
            .iter()
            .map(|n| clip(&format!("{n}.MP4"), 4000))
            .collect();
        let kind = &built_in()[1];
        let (db, catalog) = (db(), Catalog::default());
        let sources = Sources {
            db: &db,
            clips: &clips,
            catalog: &catalog,
            strava: None,
        };
        let plan = plan(kind, "ridge", sources).unwrap();
        assert_eq!(plan.title, "Ridge: getting faster");
        let runs: Vec<(&str, i64, i64, Option<&str>)> = plan
            .items
            .iter()
            .filter(|i| i.card.is_none())
            .map(|i| (i.clip.as_str(), i.first_frame, i.frames, i.timer.as_deref()))
            .collect();
        // a (first), b and d (new bests), e (latest); c is slower than b.
        // Timed part: Start 300 less 2 s, Finish plus 2 s, clipped to the clip.
        assert_eq!(
            runs,
            [
                ("a.mp4.1", 240, 3120, Some("a")),
                ("b.mp4.1", 240, 2820, Some("b")),
                ("d.mp4.1", 240, 2520, Some("d")),
                ("e.mp4.1", 240, 2720, Some("e")),
            ]
        );
        let cards: Vec<&Card> = plan.items.iter().filter_map(|i| i.card.as_ref()).collect();
        assert_eq!(cards[0].subtitle, "2026-06-01 to 2026-07-08: 20.0 s faster");
        assert_eq!(cards[1].kind, CardKind::Chart);
        assert_eq!(cards[1].course.as_deref(), Some("ridge"));
        assert_eq!(cards[2].text, "1:20.0");
    }

    fn recorded(name: &str, when: &str) -> Clip {
        Clip {
            recorded: Some(when.into()),
            ..clip(name, 600)
        }
    }

    #[test]
    fn a_feature_progression_counts_attempts_and_days() {
        use crate::features::{Attempt, Feature};
        let clips = [
            recorded("C.MP4", "2026-06-15 10:00:00"),
            recorded("A.MP4", "2026-06-14 10:00:00"),
            recorded("B.MP4", "2026-06-14 10:05:00"),
            recorded("R.MP4", "2026-06-14 10:06:00"),
            recorded("X.MP4", "2026-06-14 10:07:00"),
        ];
        let attempt = |clip: &str, outcome| Attempt {
            clip: clip.into(),
            outcome,
        };
        let mut catalog = Catalog {
            features: vec![Feature {
                id: 4,
                name: "Big drop".into(),
                attempts: vec![
                    attempt("a.mp4.1", Outcome::Crashed),
                    attempt("b.mp4.1", Outcome::Unknown),
                    attempt("c.mp4.1", Outcome::Landed),
                    attempt("r.mp4.1", Outcome::Landed),
                ],
            }],
            ..Catalog::default()
        };
        // Rejects are left out.
        catalog.flags.insert("r.mp4.1".into(), Flag::Reject);
        let db = db();
        let sources = Sources {
            db: &db,
            clips: &clips,
            catalog: &catalog,
            strava: None,
        };
        let kind = built_in()
            .into_iter()
            .find(|k| k.id == "feature-progression")
            .unwrap();
        let plan = plan(&kind, "4", sources).unwrap();
        assert_eq!(plan.title, "Big drop: 3 attempts");
        assert_eq!(
            plan.items[0].card.as_ref().unwrap().subtitle,
            "3 attempts over 2 days"
        );
        let captions: Vec<(&str, Option<&str>)> = plan
            .items
            .iter()
            .filter(|i| i.card.is_none())
            .map(|i| (i.clip.as_str(), i.caption.as_deref()))
            .collect();
        assert_eq!(
            captions,
            [
                ("a.mp4.1", Some("Attempt 1 · Day 1 · Crashed")),
                ("b.mp4.1", Some("Attempt 2 · Day 1")),
                ("c.mp4.1", Some("Attempt 3 · Day 2 · Landed")),
            ]
        );
        let cards: Vec<&Card> = plan.items.iter().filter_map(|i| i.card.as_ref()).collect();
        assert_eq!(cards[0].subtitle, "3 attempts over 2 days");
        assert_eq!(cards[1].text, "Landed on attempt 3");
        assert!(super::plan(&kind, "99", sources).is_err());
    }

    #[test]
    fn a_story_has_a_card_per_ride_and_timers_on_runs() {
        use crate::organise::Collection;
        let clips = [
            recorded("A.MP4", "2026-06-01 10:00:00"),
            recorded("B.MP4", "2026-06-01 10:30:00"),
            recorded("Q.MP4", "2026-06-20 09:00:00"),
        ];
        let catalog = Catalog {
            collections: vec![Collection {
                id: 2,
                name: "Race build-up".into(),
                clips: ["a.mp4.1", "b.mp4.1", "q.mp4.1"].map(str::to_owned).into(),
            }],
            ride_names: [("q.mp4.1".to_owned(), "Race day".to_owned())].into(),
            ..Catalog::default()
        };
        let mut db = db();
        for r in &mut db.runs {
            r.committed = true;
        }
        let sources = Sources {
            db: &db,
            clips: &clips,
            catalog: &catalog,
            strava: None,
        };
        let kind = built_in().into_iter().find(|k| k.id == "story").unwrap();
        let plan = plan(&kind, "2", sources).unwrap();
        let shape: Vec<String> = plan
            .items
            .iter()
            .map(|i| match &i.card {
                Some(c) => format!("card {}", c.text),
                None => format!("{} {}", i.clip, i.timer.as_deref().unwrap_or("-")),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "card Race build-up",
                "card Mon 1 Jun 2026, 10:00",
                "a.mp4.1 a",
                "b.mp4.1 b",
                "card Race day",
                "q.mp4.1 -",
                "card Race build-up",
            ]
        );
    }

    #[test]
    fn a_gear_review_and_a_ridelog_take_their_clips_in_order() {
        let clips = [
            recorded("B.MP4", "2026-06-01 10:30:00"),
            recorded("A.MP4", "2026-06-01 10:00:00"),
            recorded("Z.MP4", "2026-06-09 10:00:00"),
        ];
        let mut catalog = Catalog::default();
        for k in ["a.mp4.1", "b.mp4.1", "z.mp4.1"] {
            catalog
                .tags
                .insert(k.into(), ["gear: Fox helmet".to_owned()].into());
        }
        let db = db();
        let sources = Sources {
            db: &db,
            clips: &clips,
            catalog: &catalog,
            strava: None,
        };
        let kind = |id: &str| built_in().into_iter().find(|k| k.id == id).unwrap();
        let review = plan(&kind("gear-review"), "gear: Fox helmet", sources).unwrap();
        assert_eq!(review.title, "Fox helmet: review");
        let order: Vec<&str> = review.items[1..4].iter().map(|i| i.clip.as_str()).collect();
        assert_eq!(order, ["a.mp4.1", "b.mp4.1", "z.mp4.1"]);
        assert!(plan(&kind("gear-review"), "nothing", sources).is_err());

        // The ride of the 1st: A and B, not Z eight days later.
        let ridelog = plan(&kind("ridelog"), "a.mp4.1", sources).unwrap();
        assert_eq!(ridelog.title, "Mon 1 Jun 2026, 10:00");
        assert_eq!(ridelog.items.len(), 4);
        assert!(ridelog.items.iter().all(|i| i.caption.is_none()));

        // With Strava: the ride from 9:50, at 5 m/s and 150 bpm.
        let seconds: Vec<f64> = (0..7200).map(f64::from).collect();
        let streams = crate::strava::Streams {
            velocity: vec![5.0; seconds.len()],
            heartrate: vec![150.0; seconds.len()],
            altitude: seconds.iter().map(|t| t / 10.0).collect(),
            distance: seconds.iter().map(|t| t * 5.0).collect(),
            time: seconds,
            latlng: Vec::new(),
        };
        let strava = crate::strava::Index {
            activities: vec![crate::strava::Activity {
                id: 7,
                name: "Morning ride".into(),
                sport: "MountainBikeRide".into(),
                start_local: chrono::NaiveDate::from_ymd_opt(2026, 6, 1)
                    .unwrap()
                    .and_hms_opt(9, 50, 0)
                    .unwrap(),
                elapsed_seconds: 7200,
                distance_m: 36_000.0,
                climb_m: 720.0,
            }],
            streams: [(7, streams)].into(),
            ..Default::default()
        };
        let ridelog = plan(
            &kind("ridelog"),
            "a.mp4.1",
            Sources {
                strava: Some(&strava),
                ..sources
            },
        )
        .unwrap();
        assert_eq!(
            ridelog.items[1].caption.as_deref(),
            Some("3.0 km in · 18 km/h top · 150 bpm")
        );
    }

    #[test]
    fn laps_to_a_tenth() {
        assert_eq!(lap_text(83.45), "1:23.5");
        assert_eq!(lap_text(59.96), "1:00.0");
    }
}
