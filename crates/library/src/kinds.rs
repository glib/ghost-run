//! Video kinds (decision 0005): a kind is a TOML template saying what a video
//! is made from (one run, or the runs on a course) and its sections in order:
//! title and end cards, the runs (whole clips or their timed part, timer on,
//! off or optional), a chart of the course's personal bests. [`plan`] turns a
//! kind and its input into a video's title and items; the video is then an
//! ordinary one, edited and built like any other.
//!
//! Ghost Run carries neutral examples (`kinds/` in the repo, built in);
//! a folder of your own replaces them by id or adds more ([`all`]).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use timer_core::board::{RunStatus, course_board};
use timer_core::database::TimerDatabase;
use timer_core::models::RunRecord;
use timer_core::tracks;

use crate::store::Clip;
use crate::videos::{Card, CardKind, Item};

const BUILT_IN: [&str; 2] = [
    include_str!("../../../kinds/trail.toml"),
    include_str!("../../../kinds/speed-progression.toml"),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    /// One run, by its id.
    Run,
    /// A course's runs, by the course's id.
    Course,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Section {
    Title(CardSection),
    End(CardSection),
    Chart(CardSection),
    Runs(RunsSection),
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

/// The video `kind` makes of `input` (a run's id or a course's): the runs'
/// clips found in `clips` by file name, cards with their text filled in.
pub fn plan(kind: &Kind, input: &str, db: &TimerDatabase, clips: &[Clip]) -> Result<Plan> {
    let (course_id, given) = match kind.input {
        Input::Run => {
            let run = db
                .runs
                .iter()
                .find(|r| r.id == input)
                .with_context(|| format!("run {input} isn't in the run database"))?;
            (run.course_id.clone(), Some(run))
        }
        Input::Course => (input.to_owned(), None),
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
        }
    }
    Ok(Plan {
        title: fill(&kind.title),
        items,
    })
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
            ["trail", "speed-progression"]
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
        assert_eq!(kinds.len(), 2);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("broken.toml"));
    }

    #[test]
    fn a_trail_is_title_the_whole_run_and_an_end() {
        let clips = [clip("C.MP4", 4000), clip("D.MP4.mov", 4000)];
        let trail = &built_in()[0];
        let plan = plan(trail, "c", &db(), &clips).unwrap();
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
        assert!(super::plan(trail, "zzz", &db(), &clips).is_err());
        // Its clip must be in the Library.
        assert!(super::plan(trail, "a", &db(), &clips).is_err());
    }

    #[test]
    fn a_speed_progression_is_first_each_best_latest_then_a_chart() {
        let clips: Vec<Clip> = ["A", "B", "C", "D", "E"]
            .iter()
            .map(|n| clip(&format!("{n}.MP4"), 4000))
            .collect();
        let kind = &built_in()[1];
        let plan = plan(kind, "ridge", &db(), &clips).unwrap();
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

    #[test]
    fn laps_to_a_tenth() {
        assert_eq!(lap_text(83.45), "1:23.5");
        assert_eq!(lap_text(59.96), "1:00.0");
    }
}
