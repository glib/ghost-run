//! Strava (decision 0005): the cameras record no GPS, but a Strava activity
//! is usually running. Matching a clip's recording time to an activity's
//! per-second streams says where and how fast it was filmed: rides get the
//! activity's name, courses are suggested by where a clip passes, Tracks
//! shows the course's Strava segment times, captions can carry speed, heart
//! rate, distance and climb, and a camera clock that's a little off is
//! corrected from each run's segment effort.
//!
//! Ghost Run uses the owner's own Strava API application: its client id and
//! secret and the tokens are kept in `strava.json` in the data folder
//! (readable only by the owner), never in settings or the repo. Activities,
//! streams and segment efforts are kept in the catalog.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDateTime;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::store::{Clip, Library};

/// Strava's address; tests point the client at a stand-in.
pub const STRAVA: &str = "https://www.strava.com";

/// How far a clip may start outside an activity and still be matched to it.
pub const SLACK_SECONDS: i64 = 120;

/// Activities fetched in detail (streams and segment efforts) per sync at
/// most: two requests each, against Strava's 100 per 15 minutes.
pub const DETAIL_BUDGET: usize = 30;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub client_id: String,
    pub client_secret: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: i64,
    /// Who connected: their name, for Settings.
    #[serde(default)]
    pub athlete: String,
}

/// Where the tokens are kept.
pub fn tokens_path() -> PathBuf {
    crate::settings::data_dir().join("strava.json")
}

impl Tokens {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Saves them readable by the owner only.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.partial");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// The page that asks the owner to let Ghost Run read their activities.
pub fn authorize_url(base: &str, client_id: &str, redirect: &str) -> String {
    format!(
        "{base}/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&approval_prompt=auto&scope=read,activity:read_all",
        encode(client_id),
        encode(redirect)
    )
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Connects: opens Strava's page (`open`) with a redirect back to a port on
/// this machine, waits up to `timeout` for the code, and trades it for
/// tokens.
pub fn connect(
    base: &str,
    client_id: &str,
    client_secret: &str,
    timeout: Duration,
    open: impl FnOnce(&str),
) -> Result<Tokens> {
    ensure!(
        !client_id.trim().is_empty() && !client_secret.trim().is_empty(),
        "enter your Strava app's client ID and secret first"
    );
    let listener = TcpListener::bind("127.0.0.1:0").context("listening for Strava's answer")?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://localhost:{port}/ghost-run");
    open(&authorize_url(base, client_id.trim(), &redirect));
    let code = wait_for_code(&listener, timeout)?;
    exchange(base, client_id.trim(), client_secret.trim(), &code)
}

/// The `code` from the one request Strava's page redirects the browser to.
fn wait_for_code(listener: &TcpListener, timeout: Duration) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false)?;
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line)?;
                let query = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|path| path.split_once('?'))
                    .map(|(_, q)| q.to_owned())
                    .unwrap_or_default();
                let param = |name: &str| {
                    query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
                        .map(str::to_owned)
                };
                let (body, result) = match (param("code"), param("error")) {
                    (Some(code), _) => (
                        "Ghost Run is connected to Strava. You can close this tab.",
                        Ok(code),
                    ),
                    (None, Some(e)) => (
                        "Strava said no, so Ghost Run isn't connected. You can close this tab.",
                        Err(anyhow::anyhow!("Strava said no ({e})")),
                    ),
                    // Something else asking (a favicon): keep waiting.
                    _ => {
                        let _ = stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                        continue;
                    }
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                return result;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    bail!(
                        "no answer from Strava's page within {} s",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    #[serde(default)]
    athlete: Option<AthleteReply>,
}

#[derive(Deserialize)]
struct AthleteReply {
    #[serde(default)]
    firstname: String,
    #[serde(default)]
    lastname: String,
}

fn exchange(base: &str, client_id: &str, client_secret: &str, code: &str) -> Result<Tokens> {
    let reply: TokenReply = ureq::post(&format!("{base}/oauth/token"))
        .send_form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("grant_type", "authorization_code"),
        ])
        .map_err(http_error)?
        .into_json()?;
    let athlete = reply
        .athlete
        .map(|a| format!("{} {}", a.firstname, a.lastname).trim().to_owned())
        .unwrap_or_default();
    Ok(Tokens {
        client_id: client_id.to_owned(),
        client_secret: client_secret.to_owned(),
        access_token: reply.access_token,
        refresh_token: reply.refresh_token,
        expires_at: reply.expires_at,
        athlete,
    })
}

fn http_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            anyhow::anyhow!(
                "Strava answered {code}: {}",
                body.chars().take(200).collect::<String>()
            )
        }
        e => anyhow::anyhow!("couldn't reach Strava: {e}"),
    }
}

/// Requests made as the connected owner; tokens are refreshed (and saved:
/// Strava hands out a new refresh token) when they've run out.
pub struct Client {
    base: String,
    tokens: Tokens,
    path: PathBuf,
    /// Strava's 15-minute limit and usage, from the last answer.
    pub usage: Option<(u32, u32)>,
}

impl Client {
    /// The client for the tokens saved at `path`, if connected.
    pub fn open(base: &str, path: &Path) -> Result<Option<Self>> {
        Ok(Tokens::load(path)?.map(|tokens| Self {
            base: base.to_owned(),
            tokens,
            path: path.to_owned(),
            usage: None,
        }))
    }

    fn token(&mut self) -> Result<String> {
        if self.tokens.expires_at - 60 > chrono::Utc::now().timestamp() {
            return Ok(self.tokens.access_token.clone());
        }
        let reply: TokenReply = ureq::post(&format!("{}/oauth/token", self.base))
            .send_form(&[
                ("client_id", self.tokens.client_id.as_str()),
                ("client_secret", self.tokens.client_secret.as_str()),
                ("refresh_token", self.tokens.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .map_err(http_error)?
            .into_json()?;
        self.tokens.access_token = reply.access_token;
        self.tokens.refresh_token = reply.refresh_token;
        self.tokens.expires_at = reply.expires_at;
        self.tokens.save(&self.path)?;
        Ok(self.tokens.access_token.clone())
    }

    fn get(&mut self, path: &str) -> Result<serde_json::Value> {
        let token = self.token()?;
        let reply = ureq::get(&format!("{}/api/v3{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(http_error)?;
        // "100,1000" and "12,340": the 15-minute and daily limits and usage.
        let first =
            |h: &str| -> Option<u32> { reply.header(h)?.split(',').next()?.trim().parse().ok() };
        if let (Some(limit), Some(used)) = (first("X-RateLimit-Limit"), first("X-RateLimit-Usage"))
        {
            self.usage = Some((limit, used));
        }
        Ok(reply.into_json()?)
    }

    /// Whether there's room for another activity's details (two requests)
    /// in Strava's 15-minute limit.
    fn room(&self) -> bool {
        self.usage.is_none_or(|(limit, used)| used + 5 < limit)
    }

    /// Lets go of Strava's access, then forgets the tokens.
    pub fn disconnect(mut self) -> Result<()> {
        if let Ok(token) = self.token() {
            let _ = ureq::post(&format!("{}/oauth/deauthorize", self.base))
                .send_form(&[("access_token", token.as_str())]);
        }
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    pub fn athlete(&self) -> &str {
        &self.tokens.athlete
    }
}

/// An activity, as listed.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub id: i64,
    pub name: String,
    pub sport: String,
    /// Its start on the clock where it was ridden, which is what cameras
    /// record.
    pub start_local: NaiveDateTime,
    pub elapsed_seconds: i64,
    pub distance_m: f64,
    pub climb_m: f64,
}

impl Activity {
    fn from_json(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            id: v["id"].as_i64()?,
            name: v["name"].as_str().unwrap_or_default().to_owned(),
            sport: v["sport_type"]
                .as_str()
                .or(v["type"].as_str())
                .unwrap_or_default()
                .to_owned(),
            start_local: local_time(v["start_date_local"].as_str()?)?,
            elapsed_seconds: v["elapsed_time"].as_i64().unwrap_or(0),
            distance_m: v["distance"].as_f64().unwrap_or(0.0),
            climb_m: v["total_elevation_gain"].as_f64().unwrap_or(0.0),
        })
    }

    /// Whether a clip recorded at `t` (camera clock) was filmed during it.
    pub fn covers(&self, t: NaiveDateTime) -> bool {
        let from = self.start_local - chrono::Duration::seconds(SLACK_SECONDS);
        let to = self.start_local + chrono::Duration::seconds(self.elapsed_seconds + SLACK_SECONDS);
        (from..=to).contains(&t)
    }
}

/// `2026-11-24T09:28:00Z` (Strava's local times carry a Z they don't mean).
fn local_time(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s.get(..19)?, "%Y-%m-%dT%H:%M:%S").ok()
}

/// An activity's per-second record. `time` is seconds from its start; the
/// others line up with it and are empty when the activity has none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Streams {
    pub time: Vec<f64>,
    #[serde(default)]
    pub latlng: Vec<(f64, f64)>,
    /// m/s.
    #[serde(default)]
    pub velocity: Vec<f64>,
    #[serde(default)]
    pub heartrate: Vec<f64>,
    /// m.
    #[serde(default)]
    pub altitude: Vec<f64>,
    /// m from the start.
    #[serde(default)]
    pub distance: Vec<f64>,
}

/// A clip's share of an activity.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// km ridden before the clip starts.
    pub distance_km: Option<f64>,
    pub top_kmh: Option<f64>,
    pub average_kmh: Option<f64>,
    pub average_bpm: Option<f64>,
    /// m climbed during the clip.
    pub climb_m: Option<f64>,
}

impl Streams {
    fn from_json(v: &serde_json::Value) -> Self {
        let numbers = |key: &str| -> Vec<f64> {
            v[key]["data"]
                .as_array()
                .map(|a| a.iter().filter_map(serde_json::Value::as_f64).collect())
                .unwrap_or_default()
        };
        let latlng = v["latlng"]["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| Some((p[0].as_f64()?, p[1].as_f64()?)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            time: numbers("time"),
            latlng,
            velocity: numbers("velocity_smooth"),
            heartrate: numbers("heartrate"),
            altitude: numbers("altitude"),
            distance: numbers("distance"),
        }
    }

    /// The samples from `from` to `to` seconds into the activity.
    fn range(&self, from: f64, to: f64) -> std::ops::Range<usize> {
        let a = self.time.partition_point(|t| *t < from);
        let b = self.time.partition_point(|t| *t <= to);
        a..b.max(a)
    }

    /// Where the rider was `t` seconds in (the nearest sample).
    pub fn position(&self, t: f64) -> Option<(f64, f64)> {
        let i = self
            .time
            .partition_point(|x| *x < t)
            .min(self.time.len().checked_sub(1)?);
        self.latlng.get(i).copied()
    }

    /// The stats for `from` to `to` seconds in.
    pub fn span(&self, from: f64, to: f64) -> Option<Span> {
        let r = self.range(from, to);
        if r.is_empty() {
            return None;
        }
        let slice = |v: &[f64]| {
            v.get(r.clone())
                .filter(|s| !s.is_empty())
                .map(<[f64]>::to_vec)
        };
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let speed = slice(&self.velocity);
        Some(Span {
            distance_km: self.distance.get(r.start).map(|d| d / 1000.0),
            top_kmh: speed
                .as_ref()
                .map(|v| v.iter().copied().fold(0.0, f64::max) * 3.6),
            average_kmh: speed.as_ref().map(|v| mean(v) * 3.6),
            average_bpm: slice(&self.heartrate).map(|v| mean(&v)),
            climb_m: slice(&self.altitude)
                .map(|v| v.windows(2).map(|w| (w[1] - w[0]).max(0.0)).sum()),
        })
    }
}

/// A segment effort in an activity.
#[derive(Debug, Clone, PartialEq)]
pub struct Effort {
    pub activity: i64,
    pub segment: i64,
    pub name: String,
    pub start_local: NaiveDateTime,
    pub elapsed_seconds: f64,
}

/// What a sync did.
#[derive(Debug, Default, PartialEq)]
pub struct SyncReport {
    pub new_activities: usize,
    pub detailed: usize,
    /// Activities with clips still waiting for their details (the rate limit
    /// or the budget ran out): the next sync carries on.
    pub waiting: usize,
}

impl SyncReport {
    pub fn text(&self) -> String {
        let mut parts = vec![format!(
            "{} new {}",
            self.new_activities,
            if self.new_activities == 1 {
                "activity"
            } else {
                "activities"
            }
        )];
        if self.detailed > 0 {
            parts.push(format!("{} with clips read in detail", self.detailed));
        }
        if self.waiting > 0 {
            parts.push(format!(
                "{} more next time (Strava's rate limit)",
                self.waiting
            ));
        }
        format!("Strava: {}.", parts.join(", "))
    }
}

/// Brings the catalog up to date: new activities listed, then the streams
/// and segment efforts of activities that have clips, newest first, as far
/// as `budget` and Strava's rate limit allow.
pub fn sync(
    library: &Library,
    client: &mut Client,
    clips: &[Clip],
    budget: usize,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let known = library.strava_activities()?;
    // A week back from the newest known, for activities uploaded late.
    let after = known
        .iter()
        .map(|a| a.start_local)
        .max()
        .map(|t| (t - chrono::Duration::days(7)).and_utc().timestamp());
    for page in 1..=50 {
        let mut path = format!("/athlete/activities?per_page=100&page={page}");
        if let Some(after) = after {
            path.push_str(&format!("&after={after}"));
        }
        let listed = client.get(&path)?;
        let listed = listed.as_array().context("Strava's activity list")?;
        if listed.is_empty() {
            break;
        }
        for a in listed.iter().filter_map(Activity::from_json) {
            if !known.iter().any(|k| k.id == a.id) {
                report.new_activities += 1;
            }
            library.set_strava_activity(&a)?;
        }
    }
    let detailed = library.strava_detailed()?;
    let mut wanted: Vec<Activity> = library
        .strava_activities()?
        .into_iter()
        .filter(|a| !detailed.contains(&a.id))
        .filter(|a| {
            clips
                .iter()
                .any(|c| recorded(c).is_some_and(|t| a.covers(t)))
        })
        .collect();
    wanted.sort_by_key(|a| std::cmp::Reverse(a.start_local));
    for a in &wanted {
        if report.detailed >= budget || !client.room() {
            report.waiting += 1;
            continue;
        }
        let streams = client.get(&format!(
            "/activities/{}/streams?keys=time,latlng,velocity_smooth,heartrate,altitude,distance&key_by_type=true",
            a.id
        ))?;
        let detail = client.get(&format!("/activities/{}?include_all_efforts=true", a.id))?;
        let efforts: Vec<Effort> = detail["segment_efforts"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|e| {
                        Some(Effort {
                            activity: a.id,
                            segment: e["segment"]["id"].as_i64()?,
                            name: e["segment"]["name"].as_str().unwrap_or_default().to_owned(),
                            start_local: local_time(e["start_date_local"].as_str()?)?,
                            elapsed_seconds: e["elapsed_time"].as_f64()?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        library.set_strava_detail(a.id, &Streams::from_json(&streams), &efforts)?;
        report.detailed += 1;
    }
    Ok(report)
}

/// When a clip's recording started, by the camera's clock.
pub fn recorded(clip: &Clip) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(clip.recorded.as_deref()?, "%Y-%m-%d %H:%M:%S").ok()
}

/// Everything synced, read at once.
#[derive(Debug, Clone, Default)]
pub struct Index {
    pub activities: Vec<Activity>,
    pub streams: HashMap<i64, Streams>,
    pub efforts: Vec<Effort>,
    /// Activity → how far the camera's clock was off, in seconds (positive:
    /// the camera ahead), when runs say so ([`Index::correct_clocks`]).
    pub offsets: HashMap<i64, f64>,
}

impl Index {
    /// The activity a clip recorded at `t` was filmed in.
    pub fn activity_at(&self, t: NaiveDateTime) -> Option<&Activity> {
        self.activities.iter().find(|a| a.covers(t))
    }

    /// Where in its activity a moment by the camera's clock falls: the
    /// activity and the seconds in, the camera's offset taken off.
    pub fn locate(&self, t: NaiveDateTime) -> Option<(&Activity, f64)> {
        let a = self.activity_at(t)?;
        let offset = self.offsets.get(&a.id).copied().unwrap_or(0.0);
        Some((
            a,
            (t - a.start_local).num_milliseconds() as f64 / 1000.0 - offset,
        ))
    }

    /// A clip's stats: the part of its activity from `from` to `to` seconds
    /// into the clip.
    pub fn clip_span(&self, clip: &Clip, from: f64, to: f64) -> Option<Span> {
        let (a, t) = self.locate(recorded(clip)?)?;
        self.streams.get(&a.id)?.span(t + from, t + to)
    }

    /// Works out each activity's camera clock offset from runs: a run's
    /// start by the camera against the start of a segment effort on its
    /// course's segment in the same activity. `runs` are (when the run's
    /// Start was by the camera, its course's segment); the median of each
    /// activity's offsets is kept if it's under two minutes.
    pub fn correct_clocks(&mut self, runs: &[(NaiveDateTime, i64)]) {
        let mut found: HashMap<i64, Vec<f64>> = HashMap::new();
        for (start, segment) in runs {
            let Some(a) = self.activity_at(*start) else {
                continue;
            };
            let best = self
                .efforts
                .iter()
                .filter(|e| e.activity == a.id && e.segment == *segment)
                .map(|e| (*start - e.start_local).num_milliseconds() as f64 / 1000.0)
                .filter(|d| d.abs() < SLACK_SECONDS as f64)
                .min_by(|x, y| x.abs().total_cmp(&y.abs()));
            if let Some(d) = best {
                found.entry(a.id).or_default().push(d);
            }
        }
        for (id, mut ds) in found {
            ds.sort_by(f64::total_cmp);
            self.offsets.insert(id, ds[ds.len() / 2]);
        }
    }

    /// The segment most of `windows` (a course's runs: start and finish by
    /// the camera) overlap with an effort on, if any: an effort counts when
    /// it covers most of the run and is between half and twice its length,
    /// so a long segment around a course isn't taken for it.
    pub fn segment_for(&self, windows: &[(NaiveDateTime, NaiveDateTime)]) -> Option<i64> {
        let mut counts: HashMap<i64, usize> = HashMap::new();
        for (start, finish) in windows {
            let mut seen = Vec::new();
            for e in &self.efforts {
                let end = e.start_local
                    + chrono::Duration::milliseconds((e.elapsed_seconds * 1000.0) as i64);
                let overlap = (*finish).min(end) - (*start).max(e.start_local);
                let run = *finish - *start;
                let ratio = e.elapsed_seconds * 1000.0 / run.num_milliseconds().max(1) as f64;
                if overlap.num_milliseconds() * 2 > run.num_milliseconds()
                    && (0.5..=2.0).contains(&ratio)
                    && !seen.contains(&e.segment)
                {
                    seen.push(e.segment);
                    *counts.entry(e.segment).or_default() += 1;
                }
            }
        }
        counts
            .into_iter()
            .max_by_key(|(id, n)| (*n, std::cmp::Reverse(*id)))
            .map(|(id, _)| id)
    }

    /// A segment's efforts, oldest first.
    pub fn efforts_on(&self, segment: i64) -> Vec<&Effort> {
        let mut e: Vec<&Effort> = self
            .efforts
            .iter()
            .filter(|e| e.segment == segment)
            .collect();
        e.sort_by_key(|e| e.start_local);
        e
    }
}

/// What timed runs tell about courses through Strava.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Courses {
    /// Course id → the Strava segment its runs are efforts on.
    pub segments: HashMap<String, i64>,
    /// Course id → where its runs start (the median of their positions).
    pub starts: HashMap<String, (f64, f64)>,
}

/// How close a clip must pass a course's start for the course to be
/// suggested.
pub const NEAR_METRES: f64 = 40.0;

/// Learns each course's segment and start from its stored runs, and (into
/// `index`) each activity's camera clock offset.
pub fn learn(index: &mut Index, db: &timer_core::TimerDatabase, clips: &[Clip]) -> Courses {
    // Each run: course, and its Start and Finish by the camera's clock.
    let mut runs: Vec<(String, NaiveDateTime, NaiveDateTime)> = Vec::new();
    for run in db.runs.iter().filter(|r| r.committed && !r.ignored) {
        let Some(clip) = crate::auto::run_clip(run, clips) else {
            continue;
        };
        let Some(t0) = recorded(&clip) else { continue };
        let fps = clip
            .info
            .as_ref()
            .map_or(run.source_fps, |i| i.fps())
            .max(1.0);
        let at = |marker: &str| {
            run.marker_frames
                .get(marker)
                .map(|f| t0 + chrono::Duration::milliseconds((*f as f64 / fps * 1000.0) as i64))
        };
        if let (Some(start), Some(finish)) = (at("Start"), at("Finish")) {
            runs.push((run.course_id.clone(), start, finish));
        }
    }
    let mut courses = Courses::default();
    let mut by_course: HashMap<&str, Vec<(NaiveDateTime, NaiveDateTime)>> = HashMap::new();
    for (course, start, finish) in &runs {
        by_course.entry(course).or_default().push((*start, *finish));
    }
    for (course, windows) in &by_course {
        if let Some(segment) = index.segment_for(windows) {
            courses.segments.insert((*course).to_owned(), segment);
        }
    }
    let timed: Vec<(NaiveDateTime, i64)> = runs
        .iter()
        .filter_map(|(course, start, _)| Some((*start, *courses.segments.get(course)?)))
        .collect();
    index.correct_clocks(&timed);
    let mut points: HashMap<&str, Vec<(f64, f64)>> = HashMap::new();
    for (course, start, _) in &runs {
        if let Some((a, t)) = index.locate(*start)
            && let Some(p) = index.streams.get(&a.id).and_then(|s| s.position(t))
        {
            points.entry(course).or_default().push(p);
        }
    }
    for (course, mut ps) in points {
        let mid = ps.len() / 2;
        ps.sort_by(|a, b| a.0.total_cmp(&b.0));
        let lat = ps[mid].0;
        ps.sort_by(|a, b| a.1.total_cmp(&b.1));
        courses.starts.insert(course.to_owned(), (lat, ps[mid].1));
    }
    courses
}

impl Courses {
    /// The course whose start a clip passes within [`NEAR_METRES`] of, and
    /// how many seconds into the clip; the first passed if several.
    pub fn near(&self, index: &Index, clip: &Clip) -> Option<(String, f64)> {
        let (a, t0) = index.locate(recorded(clip)?)?;
        let streams = index.streams.get(&a.id)?;
        let length = clip.info.as_ref().and_then(|i| {
            i.duration_seconds
                .or_else(|| i.frames.map(|f| f as f64 / i.fps().max(1.0)))
        })?;
        let r = streams.range(t0, t0 + length);
        for i in r {
            let Some(p) = streams.latlng.get(i) else {
                break;
            };
            if let Some((course, _)) = self
                .starts
                .iter()
                .filter(|(_, s)| metres(*p, **s) <= NEAR_METRES)
                .min_by(|x, y| metres(*p, *x.1).total_cmp(&metres(*p, *y.1)))
            {
                return Some((course.clone(), streams.time[i] - t0));
            }
        }
        None
    }

    /// A course's segment and its efforts, oldest first.
    pub fn segment<'a>(&self, index: &'a Index, course: &str) -> Option<(String, Vec<&'a Effort>)> {
        let efforts = index.efforts_on(*self.segments.get(course)?);
        Some((efforts.first()?.name.clone(), efforts))
    }
}

/// Metres between two points.
pub fn metres((lat1, lng1): (f64, f64), (lat2, lng2): (f64, f64)) -> f64 {
    let r = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lng2 - lng1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

impl Library {
    pub(crate) fn create_strava_tables(db: &rusqlite::Connection) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS strava_activities (
               id INTEGER PRIMARY KEY,
               name TEXT NOT NULL,
               sport TEXT NOT NULL,
               start_local TEXT NOT NULL,
               elapsed_seconds INTEGER NOT NULL,
               distance_m REAL NOT NULL,
               climb_m REAL NOT NULL
             );
             CREATE TABLE IF NOT EXISTS strava_streams (
               activity INTEGER PRIMARY KEY,
               streams TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS strava_efforts (
               activity INTEGER NOT NULL,
               segment INTEGER NOT NULL,
               name TEXT NOT NULL,
               start_local TEXT NOT NULL,
               elapsed_seconds REAL NOT NULL
             );",
        )?;
        Ok(())
    }

    pub fn strava_activities(&self) -> Result<Vec<Activity>> {
        let mut stmt = self.db.prepare(
            "SELECT id, name, sport, start_local, elapsed_seconds, distance_m, climb_m
             FROM strava_activities ORDER BY start_local",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, f64>(5)?,
                r.get::<_, f64>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, sport, start, elapsed_seconds, distance_m, climb_m) = row?;
            if let Some(start_local) = local_time(&start) {
                out.push(Activity {
                    id,
                    name,
                    sport,
                    start_local,
                    elapsed_seconds,
                    distance_m,
                    climb_m,
                });
            }
        }
        Ok(out)
    }

    pub fn set_strava_activity(&self, a: &Activity) -> Result<()> {
        self.db.execute(
            "INSERT INTO strava_activities VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (id) DO UPDATE SET name = ?2, sport = ?3, start_local = ?4,
               elapsed_seconds = ?5, distance_m = ?6, climb_m = ?7",
            params![
                a.id,
                a.name,
                a.sport,
                a.start_local.format("%Y-%m-%dT%H:%M:%S").to_string(),
                a.elapsed_seconds,
                a.distance_m,
                a.climb_m
            ],
        )?;
        Ok(())
    }

    /// Activities whose streams and efforts have been read.
    pub fn strava_detailed(&self) -> Result<Vec<i64>> {
        let mut stmt = self.db.prepare("SELECT activity FROM strava_streams")?;
        Ok(stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_strava_detail(
        &self,
        activity: i64,
        streams: &Streams,
        efforts: &[Effort],
    ) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO strava_streams VALUES (?1, ?2)",
            params![activity, serde_json::to_string(streams)?],
        )?;
        tx.execute("DELETE FROM strava_efforts WHERE activity = ?1", [activity])?;
        for e in efforts {
            tx.execute(
                "INSERT INTO strava_efforts VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    e.activity,
                    e.segment,
                    e.name,
                    e.start_local.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    e.elapsed_seconds
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn strava_streams(&self, activity: i64) -> Result<Option<Streams>> {
        let json: Option<String> = self
            .db
            .query_row(
                "SELECT streams FROM strava_streams WHERE activity = ?1",
                [activity],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    /// Everything synced.
    pub fn strava_index(&self) -> Result<Index> {
        let activities = self.strava_activities()?;
        let mut streams = HashMap::new();
        for id in self.strava_detailed()? {
            if let Some(s) = self.strava_streams(id)? {
                streams.insert(id, s);
            }
        }
        let mut stmt = self.db.prepare(
            "SELECT activity, segment, name, start_local, elapsed_seconds FROM strava_efforts",
        )?;
        let efforts = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, f64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|(activity, segment, name, start, elapsed_seconds)| {
                Some(Effort {
                    activity,
                    segment,
                    name,
                    start_local: local_time(&start)?,
                    elapsed_seconds,
                })
            })
            .collect();
        Ok(Index {
            activities,
            streams,
            efforts,
            offsets: HashMap::new(),
        })
    }

    /// Forgets everything synced from Strava.
    pub fn clear_strava(&self) -> Result<()> {
        self.db.execute_batch(
            "DELETE FROM strava_activities; DELETE FROM strava_streams; DELETE FROM strava_efforts;",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    /// A stand-in Strava: answers each request from `route` (path and body →
    /// JSON) and records the paths asked for.
    fn fake(
        route: impl Fn(&str, &str) -> String + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
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
                let mut length = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                log.lock().unwrap().push(path.clone());
                let reply = route(&path, &body);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-RateLimit-Limit: 100,1000\r\nX-RateLimit-Usage: 10,50\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (base, asked)
    }

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn clip(name: &str, recorded: &str) -> Clip {
        Clip {
            path: format!("/nas/{name}").into(),
            size: 1,
            recorded: Some(recorded.into()),
            ..Clip::default()
        }
    }

    #[test]
    fn connecting_trades_the_code_for_tokens_kept_private() {
        let (base, _) = fake(|path, body| {
            assert_eq!(path, "/oauth/token");
            assert!(
                body.contains("code=abc") && body.contains("client_secret=s3cret"),
                "{body}"
            );
            r#"{"access_token":"A","refresh_token":"R","expires_at":4102444800,"athlete":{"firstname":"Sam","lastname":"Rider"}}"#.into()
        });
        let tokens = connect(&base, "123", "s3cret", Duration::from_secs(10), |url| {
            assert!(
                url.contains("client_id=123") && url.contains("activity:read_all"),
                "{url}"
            );
            // The browser comes back with a code.
            let redirect = url
                .split("redirect_uri=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap();
            let redirect = redirect.replace("%3A", ":").replace("%2F", "/");
            std::thread::spawn(move || {
                let _ = ureq::get(&format!("{redirect}?state=&code=abc&scope=read")).call();
            });
        })
        .unwrap();
        assert_eq!(tokens.athlete, "Sam Rider");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strava.json");
        tokens.save(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(Tokens::load(&path).unwrap(), Some(tokens));
        assert!(connect(&base, "", "", Duration::from_secs(1), |_| {}).is_err());
    }

    #[test]
    fn syncing_lists_activities_and_reads_the_ones_with_clips() {
        let (base, asked) = fake(|path, _| {
            if path.starts_with("/oauth/token") {
                return r#"{"access_token":"A2","refresh_token":"R2","expires_at":4102444800}"#
                    .into();
            }
            let page = path.split('&').find_map(|kv| kv.strip_prefix("page="));
            if page == Some("1") {
                return r#"[
                  {"id":1,"name":"Morning laps","sport_type":"MountainBikeRide","start_date_local":"2026-11-24T09:00:00Z","elapsed_time":7200,"distance":20000,"total_elevation_gain":800},
                  {"id":2,"name":"Commute","sport_type":"Ride","start_date_local":"2026-11-25T08:00:00Z","elapsed_time":1200,"distance":5000,"total_elevation_gain":30}
                ]"#.into();
            }
            if page.is_some() {
                return "[]".into();
            }
            if path.starts_with("/api/v3/activities/1/streams") {
                return r#"{"time":{"data":[0,1,2,3]},"velocity_smooth":{"data":[5,10,15,10]},"altitude":{"data":[100,102,101,105]},"distance":{"data":[0,10,25,35]},"latlng":{"data":[[49.7,-123.1],[49.7,-123.1],[49.7,-123.1],[49.7,-123.1]]}}"#.into();
            }
            if path.starts_with("/api/v3/activities/1") {
                return r#"{"segment_efforts":[{"segment":{"id":77,"name":"Ridge DH"},"start_date_local":"2026-11-24T09:30:05Z","elapsed_time":95}]}"#.into();
            }
            panic!("unexpected {path}");
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strava.json");
        Tokens {
            client_id: "1".into(),
            client_secret: "s".into(),
            access_token: "old".into(),
            refresh_token: "R".into(),
            // Run out: refreshed first, and the new ones saved.
            expires_at: 0,
            athlete: String::new(),
        }
        .save(&path)
        .unwrap();
        let library = Library::open(&dir.path().join("l.sqlite3")).unwrap();
        let mut client = Client::open(&base, &path).unwrap().unwrap();
        let clips = [clip("A.MP4", "2026-11-24 09:30:00")];
        let report = sync(&library, &mut client, &clips, DETAIL_BUDGET).unwrap();
        assert_eq!(
            report,
            SyncReport {
                new_activities: 2,
                detailed: 1,
                waiting: 0
            }
        );
        assert_eq!(Tokens::load(&path).unwrap().unwrap().refresh_token, "R2");
        assert_eq!(client.usage, Some((100, 10)));
        // Only the activity with a clip was read in detail.
        let asked = asked.lock().unwrap().clone();
        assert!(
            !asked.iter().any(|p| p.contains("activities/2")),
            "{asked:?}"
        );
        let index = library.strava_index().unwrap();
        assert_eq!(index.activities.len(), 2);
        assert_eq!(index.efforts[0].name, "Ridge DH");
        assert_eq!(index.streams[&1].velocity, [5.0, 10.0, 15.0, 10.0]);

        // Again: nothing new; the list asks from a week before the newest.
        let again = sync(&library, &mut client, &clips, DETAIL_BUDGET).unwrap();
        assert_eq!(again, SyncReport::default());
        library.clear_strava().unwrap();
        assert!(library.strava_activities().unwrap().is_empty());
    }

    #[test]
    fn a_clips_stats_come_from_its_part_of_the_activity() {
        let streams = Streams {
            time: (0..=10).map(f64::from).collect(),
            velocity: vec![0.0, 5.0, 10.0, 15.0, 20.0, 10.0, 5.0, 5.0, 5.0, 5.0, 5.0],
            heartrate: vec![120.0; 11],
            altitude: vec![
                100.0, 101.0, 103.0, 102.0, 106.0, 106.0, 106.0, 106.0, 106.0, 106.0, 106.0,
            ],
            distance: (0..=10).map(|d| f64::from(d) * 100.0).collect(),
            latlng: (0..=10)
                .map(|i| (49.0 + f64::from(i) * 0.001, -123.0))
                .collect(),
        };
        let span = streams.span(1.0, 4.0).unwrap();
        assert_eq!(span.distance_km, Some(0.1));
        assert!((span.top_kmh.unwrap() - 72.0).abs() < 1e-9);
        assert!((span.average_kmh.unwrap() - 45.0).abs() < 1e-9);
        assert_eq!(span.average_bpm, Some(120.0));
        assert_eq!(span.climb_m, Some(6.0));
        assert_eq!(streams.span(20.0, 30.0), None);
        assert_eq!(streams.position(2.4), Some((49.003, -123.0)));

        let mut index = Index {
            activities: vec![Activity {
                id: 1,
                name: "Laps".into(),
                sport: "MountainBikeRide".into(),
                start_local: at("2026-11-24 09:00:00"),
                elapsed_seconds: 10,
                distance_m: 1000.0,
                climb_m: 6.0,
            }],
            streams: [(1, streams)].into(),
            ..Index::default()
        };
        // Recorded a second in; stats for its first three seconds.
        let c = clip("A.MP4", "2026-11-24 09:00:01");
        assert_eq!(index.clip_span(&c, 0.0, 3.0).unwrap().climb_m, Some(6.0));
        // Two minutes' slack either side, no more.
        assert!(index.activity_at(at("2026-11-24 08:58:00")).is_some());
        assert!(index.activity_at(at("2026-11-24 09:02:11")).is_none());
        // A camera 1 s ahead: its 09:00:02 is the activity's 1 s.
        index.offsets.insert(1, 1.0);
        assert_eq!(index.locate(at("2026-11-24 09:00:02")).unwrap().1, 1.0);
    }

    #[test]
    fn runs_teach_courses_their_segment_start_and_the_camera_clock() {
        use timer_core::models::{Course, RunRecord};
        let info = crate::MediaInfo {
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            fps_num: 10,
            fps_den: 1,
            frames: Some(1200),
            duration_seconds: Some(120.0),
            timecode: None,
            rotation: 0,
            creation_time: None,
            has_audio: true,
        };
        let with_info = |c: Clip| Clip {
            info: Some(info.clone()),
            ..c
        };
        // The camera is 5 s ahead. The run's clip starts at 09:30:00 by it;
        // Start is 10 s in (09:30:10 by the camera, 09:30:05 really).
        let clips = [
            with_info(clip("A.MP4", "2026-11-24 09:30:00")),
            // An untimed clip from later, passing the course's start 30 s in.
            with_info(clip("B.MP4", "2026-11-24 10:00:00")),
        ];
        let frames = [("Start", 100), ("S1", 500), ("Finish", 1000)]
            .iter()
            .map(|(n, f)| ((*n).to_owned(), *f))
            .collect();
        let mut run = RunRecord::new("r1", "ridge", "2026-11-24", "A.MP4", 10.0, frames);
        run.committed = true;
        let db = timer_core::TimerDatabase {
            courses: vec![Course {
                id: "ridge".into(),
                name: "Ridge".into(),
                sector_count: 2,
            }],
            runs: vec![run],
        };
        // The activity from 09:00 (real), one sample a second; the rider is
        // at the course's start at 09:30:05 and again at 10:00:25.
        let start = (49.70, -123.15);
        let away = (49.80, -123.0);
        let latlng: Vec<(f64, f64)> = (0..=3700)
            .map(|t| if t == 1805 || t == 3625 { start } else { away })
            .collect();
        let time: Vec<f64> = (0..latlng.len()).map(|i| i as f64).collect();
        let mut index = Index {
            activities: vec![Activity {
                id: 1,
                name: "Laps".into(),
                sport: "MountainBikeRide".into(),
                start_local: at("2026-11-24 09:00:00"),
                elapsed_seconds: 3700,
                distance_m: 0.0,
                climb_m: 0.0,
            }],
            streams: [(
                1,
                Streams {
                    time,
                    latlng,
                    ..Streams::default()
                },
            )]
            .into(),
            efforts: vec![Effort {
                activity: 1,
                segment: 77,
                name: "Ridge DH".into(),
                start_local: at("2026-11-24 09:30:05"),
                elapsed_seconds: 92.0,
            }],
            ..Index::default()
        };
        let courses = learn(&mut index, &db, &clips);
        assert_eq!(courses.segments["ridge"], 77);
        assert_eq!(index.offsets[&1], 5.0);
        assert_eq!(courses.starts["ridge"], start);
        // B, by the camera 10:00:00, is really 09:59:55: the start is passed
        // at 10:00:25 real, 30 s into the clip.
        assert_eq!(
            courses.near(&index, &clips[1]),
            Some(("ridge".into(), 30.0))
        );
        let (name, efforts) = courses.segment(&index, "ridge").unwrap();
        assert_eq!((name.as_str(), efforts.len()), ("Ridge DH", 1));
    }

    #[test]
    fn runs_find_their_segment_and_the_cameras_clock() {
        let effort = |activity, segment, start: &str, secs| Effort {
            activity,
            segment,
            name: format!("seg {segment}"),
            start_local: at(start),
            elapsed_seconds: secs,
        };
        let mut index = Index {
            activities: vec![Activity {
                id: 1,
                name: "Laps".into(),
                sport: "MountainBikeRide".into(),
                start_local: at("2026-11-24 09:00:00"),
                elapsed_seconds: 7200,
                distance_m: 0.0,
                climb_m: 0.0,
            }],
            efforts: vec![
                // The course's segment, ridden twice; a long one around both.
                effort(1, 77, "2026-11-24 09:30:00", 95.0),
                effort(1, 77, "2026-11-24 10:00:00", 92.0),
                effort(1, 5, "2026-11-24 09:20:00", 3600.0),
            ],
            ..Index::default()
        };
        // Two runs by the camera, 8 and 12 s after the efforts started.
        let windows = [
            (at("2026-11-24 09:30:08"), at("2026-11-24 09:31:41")),
            (at("2026-11-24 10:00:12"), at("2026-11-24 10:01:42")),
        ];
        // The long segment covers both runs but is far longer than them.
        assert_eq!(index.segment_for(&windows), Some(77));
        assert_eq!(index.segment_for(&[]), None);
        index.correct_clocks(&[(windows[0].0, 77), (windows[1].0, 77)]);
        // The median of 8 and 12 (the higher of two): the camera 12 s ahead.
        assert_eq!(index.offsets[&1], 12.0);
        assert_eq!(index.efforts_on(77).len(), 2);
        assert!((metres((49.0, -123.0), (49.001, -123.0)) - 111.2).abs() < 0.5);
    }
}
