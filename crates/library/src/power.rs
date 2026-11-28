//! Whether this machine runs on a battery, from `/sys/class/power_supply`:
//! a laptop makes previews only when they're needed, and background work
//! pauses while it's unplugged.

use std::path::Path;

const SUPPLIES: &str = "/sys/class/power_supply";

/// Whether the machine has a battery (a laptop).
pub fn has_battery() -> bool {
    has_battery_in(Path::new(SUPPLIES))
}

/// Whether it's running on its battery now.
pub fn on_battery() -> bool {
    on_battery_in(Path::new(SUPPLIES))
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// The system's batteries (not a mouse's: those have `scope` Device).
fn batteries(root: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|d| read(d, "type") == "Battery" && read(d, "scope") != "Device")
        .collect()
}

fn has_battery_in(root: &Path) -> bool {
    !batteries(root).is_empty()
}

fn on_battery_in(root: &Path) -> bool {
    batteries(root)
        .iter()
        .any(|b| read(b, "status") == "Discharging")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(root: &Path, name: &str, files: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, text) in files {
            std::fs::write(dir.join(file), format!("{text}\n")).unwrap();
        }
    }

    #[test]
    fn batteries_and_whether_one_is_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(!has_battery_in(root) && !on_battery_in(root));
        supply(root, "AC", &[("type", "Mains"), ("online", "1")]);
        supply(
            root,
            "hidpp_battery_0",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("status", "Discharging"),
            ],
        );
        // A desktop with a wireless mouse.
        assert!(!has_battery_in(root) && !on_battery_in(root));
        supply(root, "BAT0", &[("type", "Battery"), ("status", "Charging")]);
        assert!(has_battery_in(root) && !on_battery_in(root));
        supply(
            root,
            "BAT0",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        assert!(on_battery_in(root));
    }
}
