//! `rtimer`: the run-database commands of the Python `resolve-timer` CLI, with
//! the same arguments and output, so the two can be compared (`scripts/parity.sh`).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use timer_core::matching::clip_fingerprint;
use timer_core::stats::{CourseStats, StatsOptions, compute_course_stats};
use timer_core::timing::format_duration;
use timer_core::validation::validate_database;
use timer_core::{DatabaseError, TimerDatabase};

#[derive(Parser)]
#[command(name = "rtimer", about = "Resolve Timer run database tools")]
struct Cli {
    /// Path to timer YAML database
    #[arg(long, default_value = "timer_db.yaml", global = true)]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List configured courses
    Courses,
    /// Validate database consistency
    ValidateDb,
    /// Fill derived fields in the database
    NormalizeDb,
    /// List committed run records
    Runs {
        /// Only show runs for this course ID
        #[arg(long)]
        course: Option<String>,
    },
    /// Show course timing statistics
    Stats {
        /// Course ID
        #[arg(long)]
        course: String,
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, DatabaseError> {
    let mut db = TimerDatabase::load(&cli.db)?;
    match cli.command {
        Command::Courses => {
            for c in &db.courses {
                println!("{}\t{}\t{} sectors", c.id, c.name, c.sector_count);
            }
        }
        Command::ValidateDb => {
            let errors = validate_database(&db);
            if errors.is_empty() {
                println!("Database OK");
            } else {
                for e in &errors {
                    println!("{e}");
                }
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::NormalizeDb => {
            let mut updated = 0;
            for run in &mut db.runs {
                let fingerprint = clip_fingerprint(&run.filename, &run.marker_frames);
                if run.fingerprint.as_deref() != Some(fingerprint.as_str()) {
                    run.fingerprint = Some(fingerprint);
                    updated += 1;
                }
            }
            db.save(&cli.db)?;
            println!("Updated {updated} run fingerprints");
        }
        Command::Runs { course } => {
            let mut runs: Vec<_> = db
                .runs
                .iter()
                .filter(|r| course.as_ref().is_none_or(|c| &r.course_id == c))
                .collect();
            runs.sort_by(|a, b| {
                (&a.course_id, &a.date, &a.id).cmp(&(&b.course_id, &b.date, &b.id))
            });
            for r in runs {
                let mut flags = vec![if r.committed {
                    "committed"
                } else {
                    "uncommitted"
                }];
                if r.ignored {
                    flags.push("ignored");
                }
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    r.id,
                    r.course_id,
                    r.date,
                    flags.join(","),
                    r.filename
                );
            }
        }
        Command::Stats { course, json } => {
            let course = db.course_by_id(&course)?;
            let stats = compute_course_stats(course, &db.runs, StatsOptions::default());
            if json {
                println!("{}", python_json(&stats_json(&course.id, &stats)));
            } else {
                println!("Course: {}", course.name);
                println!("Eligible runs: {}", stats.eligible_runs.len());
                match &stats.best_lap {
                    Some(best) => println!(
                        "Best: {} ({})",
                        duration(best.timing.lap_seconds),
                        best.run.id
                    ),
                    None => println!("Best: --:--.---"),
                }
                match stats.optimal_seconds {
                    Some(s) => println!("Optimal: {}", duration(s)),
                    None => println!("Optimal: --:--.---"),
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn duration(seconds: f64) -> String {
    format_duration(seconds).unwrap_or_else(|_| "--:--.---".into())
}

fn stats_json(course_id: &str, stats: &CourseStats<'_>) -> Value {
    json!({
        "course_id": course_id,
        "eligible_run_count": stats.eligible_runs.len(),
        "best_lap": stats.best_lap.as_ref().map(|b| json!({
            "run_id": b.run.id,
            "seconds": b.timing.lap_seconds,
            "frames": b.timing.lap_frames,
        })),
        "optimal": stats.optimal_seconds.map(|s| json!({"seconds": s, "frames": stats.optimal_frames})),
        "fastest_sectors": stats.fastest_sectors.iter().map(|s| json!({
            "sector": s.sector,
            "run_id": s.run.id,
            "seconds": s.duration_seconds,
            "frames": s.duration_frames,
        })).collect::<Vec<_>>(),
    })
}

/// JSON as Python's `json.dumps(value, indent=2, sort_keys=True)` prints it:
/// non-ASCII escaped, floats in Python `repr` form.
fn python_json(value: &Value) -> String {
    fn write(value: &Value, indent: usize, out: &mut String) {
        let pad = |n: usize| "  ".repeat(n);
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => match n.as_i64() {
                Some(i) => out.push_str(&i.to_string()),
                None => out.push_str(&timer_core::py::float_repr(n.as_f64().unwrap_or(f64::NAN))),
            },
            Value::String(s) => out.push_str(&timer_core::py::json_string(s)),
            Value::Array(items) if items.is_empty() => out.push_str("[]"),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    out.push_str(if i == 0 { "\n" } else { ",\n" });
                    out.push_str(&pad(indent + 1));
                    write(item, indent + 1, out);
                }
                out.push('\n');
                out.push_str(&pad(indent));
                out.push(']');
            }
            Value::Object(map) if map.is_empty() => out.push_str("{}"),
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, key) in keys.iter().enumerate() {
                    out.push_str(if i == 0 { "\n" } else { ",\n" });
                    out.push_str(&pad(indent + 1));
                    out.push_str(&timer_core::py::json_string(key));
                    out.push_str(": ");
                    write(&map[*key], indent + 1, out);
                }
                out.push('\n');
                out.push_str(&pad(indent));
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    write(value, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_matches_python_layout() {
        let v = json!({"b": [1, 2.5, {"z": null}], "a": "é", "c": [], "d": 10.0});
        assert_eq!(
            python_json(&v),
            "{\n  \"a\": \"\\u00e9\",\n  \"b\": [\n    1,\n    2.5,\n    {\n      \"z\": null\n    }\n  ],\n  \"c\": [],\n  \"d\": 10.0\n}"
        );
    }
}
