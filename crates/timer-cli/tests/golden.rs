//! rtimer's database and overlay commands give the outputs in
//! `tests/fixtures/golden.txt`, recorded when they still matched the retired
//! Python tool byte for byte (decision 0008). `scripts/golden.sh --update`
//! rewrites it after a deliberate change.

use std::path::Path;
use std::process::Command;

#[test]
fn outputs_match_the_golden_transcript() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Command::new("bash")
        .arg(root.join("scripts/golden.sh"))
        .arg("--check")
        .env("RTIMER", env!("CARGO_BIN_EXE_rtimer"))
        .output()
        .expect("running scripts/golden.sh");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
