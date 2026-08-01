//! Probe / subtitle / map status strings (ADR-0014, ADR-0023). CHECKs dropped
//! in migration 006; writers validate here instead of rebuilding media_items.

pub const PROBE_STATUSES: &[&str] = &["indexed", "probed", "error", "unavailable"];
pub const SUBTITLE_STATUSES: &[&str] = &["pending", "ready", "none", "error", "unavailable"];
pub const MAP_STATUSES: &[&str] = &["pending", "ready", "error", "unavailable"];
pub const MAP_CONTAINER_KINDS: &[&str] = &["matroska", "mp4"];

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

pub fn parse_map_status(s: &str) -> Result<&str, String> {
    if MAP_STATUSES.contains(&s) {
        Ok(s)
    } else {
        Err(format!("invalid map_status: {s}"))
    }
}

pub fn parse_map_container_kind(s: &str) -> Result<&str, String> {
    if MAP_CONTAINER_KINDS.contains(&s) {
        Ok(s)
    } else {
        Err(format!("invalid map container_kind: {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_unavailable() {
        assert_eq!(parse_probe_status("unavailable").unwrap(), "unavailable");
        assert_eq!(parse_subtitle_status("unavailable").unwrap(), "unavailable");
        assert_eq!(parse_map_status("unavailable").unwrap(), "unavailable");
    }

    #[test]
    fn accepts_map_kinds() {
        assert_eq!(parse_map_container_kind("matroska").unwrap(), "matroska");
        assert_eq!(parse_map_container_kind("mp4").unwrap(), "mp4");
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_probe_status("nope").is_err());
        assert!(parse_subtitle_status("nope").is_err());
        assert!(parse_map_status("nope").is_err());
        assert!(parse_map_container_kind("avi").is_err());
    }
}
