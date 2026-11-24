//! Talking to DaVinci Resolve: Ghost Run runs a small Python bridge
//! (`resolve_bridge.py`, built into the app) with the Python where Resolve is
//! installed, usually inside the `davincibox` Distrobox, and exchanges one JSON
//! request and reply with it.
//!
//! Resolve's external scripting needs Resolve Studio, running, with
//! Preferences > System > General > External scripting using set to Local.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

/// The bridge's source, written out before each run.
pub const SCRIPT: &str = include_str!("resolve_bridge.py");
/// The Distrobox Resolve is in, if none is set.
pub const DEFAULT_CONTAINER: &str = "davincibox";
/// How long Resolve gets to answer (the first `distrobox enter` can start the box).
const TIMEOUT: Duration = Duration::from_secs(90);

/// How to run the bridge.
#[derive(Debug, Clone, PartialEq)]
pub struct Bridge {
    /// The Distrobox to run it in; `None`: run it here (Resolve installed natively).
    pub container: Option<String>,
    pub python: String,
    /// Extra environment for the bridge (tests).
    pub env: Vec<(String, String)>,
}

/// Resolve said no, with its reason (`not_running`, `no_project`, ...).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct Refused {
    pub code: String,
    pub message: String,
}

impl Bridge {
    /// From the app's settings: Resolve installed here if it is, else in `container`.
    pub fn new(container: Option<&str>) -> Self {
        let native = Path::new("/opt/resolve/bin/resolve").is_file();
        Self {
            container: if native {
                None
            } else {
                Some(
                    container
                        .filter(|c| !c.trim().is_empty())
                        .unwrap_or(DEFAULT_CONTAINER)
                        .trim()
                        .to_owned(),
                )
            },
            python: "python3".into(),
            env: Vec::new(),
        }
    }

    /// The command that runs `script`.
    pub fn command(&self, script: &Path) -> Command {
        let mut cmd = match &self.container {
            None => Command::new(&self.python),
            Some(container) => {
                // Inside another Distrobox (a dev box), go through the host.
                let in_box = Path::new("/run/.containerenv").exists()
                    && which("distrobox-host-exec").is_some();
                let mut cmd = if in_box {
                    let mut c = Command::new("distrobox-host-exec");
                    c.arg("distrobox");
                    c
                } else {
                    Command::new("distrobox")
                };
                cmd.args(["enter", "-n", container, "--", &self.python]);
                cmd
            }
        };
        cmd.arg(script);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    /// Sends `request`; the reply, or why there isn't one.
    pub fn request(&self, request: &Value) -> Result<Value> {
        let script = write_script()?;
        let mut child = self
            .command(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| match &self.container {
                Some(_) => "couldn't run distrobox (is it installed?)".to_owned(),
                None => format!("couldn't run {}", self.python),
            })?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(request.to_string().as_bytes())?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let out = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stdout.read_to_string(&mut s);
            s
        });
        let err = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let began = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if began.elapsed() > TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Resolve didn't answer within {} s", TIMEOUT.as_secs());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let (out, err) = (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        );
        let reply = parse_reply(&out).ok_or_else(|| {
            let detail = err.trim().lines().last().unwrap_or("").to_owned();
            match &self.container {
                Some(c) if !status.success() => anyhow!(
                    "the bridge couldn't run in the {c} container ({}): {detail}",
                    status
                ),
                _ => anyhow!("no answer from the bridge ({status}): {detail}"),
            }
        })?;
        if reply["ok"] == Value::Bool(true) {
            Ok(reply)
        } else {
            Err(Refused {
                code: reply["error"].as_str().unwrap_or("failed").to_owned(),
                message: reply["message"]
                    .as_str()
                    .unwrap_or("Resolve refused")
                    .to_owned(),
            }
            .into())
        }
    }
}

/// The reply: the last line of output that is a JSON object (Resolve's module
/// may print its own lines first).
pub fn parse_reply(stdout: &str) -> Option<Value> {
    stdout
        .lines()
        .rev()
        .filter(|l| l.trim_start().starts_with('{'))
        .find_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(Value::is_object)
}

/// Writes the bridge where the container can read it (the home folder is
/// shared with Distrobox containers).
fn write_script() -> Result<PathBuf> {
    let dir = library::settings::data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("resolve_bridge.py");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SCRIPT) {
        let tmp = path.with_extension("py.tmp");
        std::fs::write(&tmp, SCRIPT)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(program))
            .find(|p| p.is_file())
    })
}

/// A one-line summary of a status reply.
pub fn describe_status(reply: &Value) -> String {
    let product = reply["product"].as_str().unwrap_or("Resolve");
    let version = reply["version"].as_str().unwrap_or("");
    match (reply["project"].as_str(), reply["timeline"].as_str()) {
        (Some(p), Some(t)) => format!("{product} {version}: project {p}, timeline {t}"),
        (Some(p), None) => format!("{product} {version}: project {p}, no timeline open"),
        _ => format!("{product} {version}: no project open"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runs_in_the_container_unless_resolve_is_here() {
        let b = Bridge {
            container: Some("davincibox".into()),
            python: "python3".into(),
            env: vec![],
        };
        let cmd = b.command(Path::new("/h/bridge.py"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.ends_with(&[
                "enter".into(),
                "-n".into(),
                "davincibox".into(),
                "--".into(),
                "python3".into(),
                "/h/bridge.py".into()
            ]),
            "{args:?}"
        );
        let direct = Bridge {
            container: None,
            ..b
        };
        assert_eq!(
            direct.command(Path::new("/h/bridge.py")).get_program(),
            "python3"
        );
    }

    #[test]
    fn the_reply_is_the_last_json_line() {
        let out = "Resolve says hi\n{\"partial\": \n{\"ok\": true, \"project\": \"P\"}\n\n";
        assert_eq!(parse_reply(out), Some(json!({"ok": true, "project": "P"})));
        assert_eq!(parse_reply("nothing"), None);
    }

    fn have_python() -> bool {
        Command::new("python3").arg("--version").output().is_ok()
    }

    /// The real bridge, run with a stand-in for Resolve's scripting module.
    #[test]
    fn talks_to_the_bridge() {
        if !have_python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("DaVinciResolveScript.py"),
            r#"
class _Timeline:
    def GetName(self): return "Edit"
class _Project:
    def GetName(self): return "Race day"
    def GetCurrentTimeline(self): return _Timeline()
class _Resolve:
    def GetProjectManager(self): return self
    def GetCurrentProject(self): return _Project()
    def GetProductName(self): return "DaVinci Resolve Studio"
    def GetVersionString(self): return "20.2"
def scriptapp(name):
    print("fusionscript chatter")
    return _Resolve()
"#,
        )
        .unwrap();
        let data = tempfile::tempdir().unwrap();
        // SAFETY: tests in this module don't read XDG_DATA_HOME concurrently.
        unsafe { std::env::set_var("XDG_DATA_HOME", data.path()) };
        let bridge = Bridge {
            container: None,
            python: "python3".into(),
            env: vec![
                ("PYTHONPATH".into(), dir.path().display().to_string()),
                ("RESOLVE_SCRIPT_API".into(), "/nonexistent".into()),
            ],
        };
        let reply = bridge.request(&json!({"command": "status"})).unwrap();
        assert_eq!(
            describe_status(&reply),
            "DaVinci Resolve Studio 20.2: project Race day, timeline Edit"
        );
        let err = bridge.request(&json!({"command": "nope"})).unwrap_err();
        assert_eq!(err.downcast_ref::<Refused>().unwrap().code, "bad_request");
    }
}
