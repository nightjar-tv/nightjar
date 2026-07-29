//! Probe / subtitle status strings (ADR-0014). CHECKs dropped in migration 006;
//! writers validate here instead of risking a media_items table rebuild.

pub const PROBE_STATUSES: &[&str] = &["indexed", "probed", "error", "unavailable"];
pub const SUBTITLE_STATUSES: &[&str] = &["pending", "ready", "none", "error", "unavailable"];

pub fn parse_probe_status(s: &str) -> Result<&str, String> {
    if PROBE_STATUSES.contains(&s) {
        Ok(s)
    } else {
        Err(format!("invalid probe_status: {s}"))
    }
}

pub fn parse_subtitle_status(s: &str) -> Result<&str, String> {
    if SUBTITLE_STATUSES.contains(&s) {
        Ok(s)
    } else {
        Err(format!("invalid subtitle_status: {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_unavailable() {
        assert_eq!(parse_probe_status("unavailable").unwrap(), "unavailable");
        assert_eq!(parse_subtitle_status("unavailable").unwrap(), "unavailable");
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_probe_status("nope").is_err());
        assert!(parse_subtitle_status("nope").is_err());
    }
}
