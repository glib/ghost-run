//! This machine's name, for what belongs to one machine: clips outside the
//! NAS footage root in a catalog more than one machine uses (#69), and the
//! names of catalog backups.

/// The host name, lower-cased, with anything but letters, digits, `-` and
/// `_` made `-` (`GHOST_RUN_MACHINE` overrides it, for tests).
pub fn name() -> String {
    let raw = std::env::var("GHOST_RUN_MACHINE")
        .ok()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(host_name);
    clean(&raw)
}

fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname writes at most
    // that many bytes and the result is read up to the first NUL.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    if !ok {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn clean(raw: &str) -> String {
    let name: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        "machine".into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_in_file_names() {
        assert_eq!(clean("Bazzite.local"), "bazzite-local");
        assert_eq!(clean(" liam's laptop "), "liam-s-laptop");
        assert_eq!(clean(""), "machine");
        assert!(!host_name().is_empty());
    }
}
