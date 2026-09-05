//! Linux memory readings used by HLS session admission.

#![cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncoderMemory {
    Measured {
        live_rss_bytes: u64,
        available_bytes: u64,
        children: usize,
    },
    Unmeasured,
}

/// Read memory available to this process. Linux uses the tighter of the host
/// and cgroup readings. Other platforms are deliberately unmeasured: ADR-0050
/// §9 does not establish an equivalent production instrument for them.
pub(crate) fn read_available_memory() -> Result<u64, String> {
    #[cfg(target_os = "linux")]
    {
        read_available_memory_at(Path::new("/"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("encoder memory admission is not measured on this platform".into())
    }
}

/// Read one child's resident set. The root parameter keeps the parser
/// fixture-testable on hosts without `/proc`.
pub(crate) fn read_child_rss_at(root: &Path, pid: u32) -> Result<u64, String> {
    let path = root.join("proc").join(pid.to_string()).join("status");
    let status = fs::read_to_string(&path)
        .map_err(|e| format!("read child status {}: {e}", path.display()))?;
    let rss_kib = parse_kib_field(&status, "VmRSS:")?
        .ok_or_else(|| format!("child status {} has no VmRSS", path.display()))?;
    let rss_bytes = rss_kib
        .checked_mul(1024)
        .ok_or_else(|| format!("child VmRSS overflows bytes in {}", path.display()))?;
    if rss_bytes == 0 {
        return Err(format!(
            "child status {} reports zero VmRSS",
            path.display()
        ));
    }
    Ok(rss_bytes)
}

/// Read one live child's RSS on Linux. Non-Linux production admission stays
/// [`EncoderMemory::Unmeasured`], while [`read_child_rss_at`] remains portable
/// for fixture tests.
#[cfg(target_os = "linux")]
pub(crate) fn read_child_rss(pid: u32) -> Result<u64, String> {
    read_child_rss_at(Path::new("/"), pid)
}

/// Composes the two production pieces against a fixture root. Production
/// composes them itself from `read_available_memory` and
/// `encoder_memory_from_available`, so this exists only for the tests that
/// need a root they can write.
#[cfg(test)]
pub(crate) fn measure_encoder_memory_at(
    root: &Path,
    live_rss_bytes: Option<u64>,
    children: usize,
) -> EncoderMemory {
    encoder_memory_from_available(live_rss_bytes, children, &read_available_memory_at(root))
}

pub(crate) fn encoder_memory_from_available(
    live_rss_bytes: Option<u64>,
    children: usize,
    available_bytes: &Result<u64, String>,
) -> EncoderMemory {
    let (Some(live_rss_bytes), Ok(available_bytes)) = (live_rss_bytes, available_bytes) else {
        return EncoderMemory::Unmeasured;
    };
    EncoderMemory::Measured {
        live_rss_bytes,
        available_bytes: *available_bytes,
        children,
    }
}

pub(crate) fn read_available_memory_at(root: &Path) -> Result<u64, String> {
    let cgroup_v2_available = read_cgroup_v2_available(root)?;
    let cgroup_v1_available = read_cgroup_v1_available(root)?;
    let meminfo_path = root.join("proc/meminfo");
    let meminfo = fs::read_to_string(&meminfo_path)
        .map_err(|e| format!("read host memory {}: {e}", meminfo_path.display()))?;
    let host_kib = parse_kib_field(&meminfo, "MemAvailable:")?
        .ok_or_else(|| format!("host memory {} has no MemAvailable", meminfo_path.display()))?;
    let mut available = host_kib.checked_mul(1024).ok_or_else(|| {
        format!(
            "host MemAvailable overflows bytes in {}",
            meminfo_path.display()
        )
    })?;

    if let Some(cgroup_available) = cgroup_v2_available {
        available = available.min(cgroup_available);
    }
    if let Some(cgroup_available) = cgroup_v1_available {
        available = available.min(cgroup_available);
    }
    Ok(available)
}

fn read_cgroup_v2_available(root: &Path) -> Result<Option<u64>, String> {
    let base = root.join("sys/fs/cgroup");
    let limit_path = base.join("memory.max");
    let Some(limit_raw) = read_optional(&limit_path)? else {
        return Ok(None);
    };
    if limit_raw.trim() == "max" {
        return Ok(None);
    }
    let limit = parse_bytes(&limit_path, &limit_raw)?;
    let used = read_memory_stat(&base.join("memory.stat"), "anon")?;
    limit
        .checked_sub(used)
        .map(Some)
        .ok_or_else(|| format!("cgroup v2 anon {used} exceeds memory.max {limit}"))
}

fn read_cgroup_v1_available(root: &Path) -> Result<Option<u64>, String> {
    let base = root.join("sys/fs/cgroup/memory");
    let limit_path = base.join("memory.limit_in_bytes");
    let Some(limit_raw) = read_optional(&limit_path)? else {
        return Ok(None);
    };
    let limit = parse_bytes(&limit_path, &limit_raw)?;
    // An unset v1 controller reports page-rounded LONG_MAX. Half of i64::MAX
    // is the boundary because any value in that exabyte range is the sentinel,
    // not a memory limit a Nightjar host can supply.
    if limit > i64::MAX as u64 / 2 {
        return Ok(None);
    }
    let used = read_memory_stat(&base.join("memory.stat"), "rss")?;
    limit
        .checked_sub(used)
        .map(Some)
        .ok_or_else(|| format!("cgroup v1 rss {used} exceeds memory limit {limit}"))
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read memory source {}: {e}", path.display())),
    }
}

fn read_memory_stat(path: &Path, field: &str) -> Result<u64, String> {
    let stat = fs::read_to_string(path)
        .map_err(|e| format!("read cgroup memory stat {}: {e}", path.display()))?;
    for line in stat.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(field) {
            continue;
        }
        let value = fields.next().ok_or_else(|| {
            format!(
                "cgroup memory stat {} has no value for {field}",
                path.display()
            )
        })?;
        if fields.next().is_some() {
            return Err(format!(
                "cgroup memory stat {} has extra values for {field}",
                path.display()
            ));
        }
        return value.parse::<u64>().map_err(|e| {
            format!(
                "parse cgroup memory stat {} field {field}: {e}",
                path.display()
            )
        });
    }
    Err(format!(
        "cgroup memory stat {} has no {field} field",
        path.display()
    ))
}

fn parse_bytes(path: &Path, raw: &str) -> Result<u64, String> {
    raw.trim()
        .parse::<u64>()
        .map_err(|e| format!("parse memory limit {}: {e}", path.display()))
}

fn parse_kib_field(contents: &str, field: &str) -> Result<Option<u64>, String> {
    for line in contents.lines() {
        let Some(raw) = line.strip_prefix(field) else {
            continue;
        };
        let mut parts = raw.split_whitespace();
        let Some(value) = parts.next() else {
            return Err(format!("{field} has no value"));
        };
        if parts.next() != Some("kB") || parts.next().is_some() {
            return Err(format!("{field} does not use one kB value"));
        }
        return value
            .parse::<u64>()
            .map(Some)
            .map_err(|e| format!("parse {field}: {e}"));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::process::Command;
    #[cfg(target_os = "linux")]
    use std::time::{Duration, Instant};

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn cgroup_v2_max_uses_host_memavailable() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "proc/meminfo", "MemAvailable: 4096 kB\n");
        write(dir.path(), "sys/fs/cgroup/memory.max", "max\n");

        assert_eq!(
            measure_encoder_memory_at(dir.path(), Some(1024), 1),
            EncoderMemory::Measured {
                live_rss_bytes: 1024,
                available_bytes: 4 * 1024 * 1024,
                children: 1,
            }
        );
    }

    #[test]
    fn cgroup_v1_unset_sentinel_uses_host_memavailable() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "proc/meminfo", "MemAvailable: 8192 kB\n");
        write(
            dir.path(),
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            "9223372036854771712\n",
        );

        assert_eq!(
            measure_encoder_memory_at(dir.path(), Some(2048), 2),
            EncoderMemory::Measured {
                live_rss_bytes: 2048,
                available_bytes: 8 * 1024 * 1024,
                children: 2,
            }
        );
    }

    #[test]
    fn cgroup_usage_excludes_reclaimable_page_cache() {
        let v2 = tempfile::tempdir().unwrap();
        write(v2.path(), "proc/meminfo", "MemAvailable: 4194304 kB\n");
        write(v2.path(), "sys/fs/cgroup/memory.max", "1073741824\n");
        write(
            v2.path(),
            "sys/fs/cgroup/memory.stat",
            "anon 67108864\nfile 939524096\n",
        );
        write(v2.path(), "sys/fs/cgroup/memory.current", "1006632960\n");

        let v1 = tempfile::tempdir().unwrap();
        write(v1.path(), "proc/meminfo", "MemAvailable: 4194304 kB\n");
        write(
            v1.path(),
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            "1073741824\n",
        );
        write(
            v1.path(),
            "sys/fs/cgroup/memory/memory.stat",
            "rss 67108864\ncache 939524096\n",
        );
        write(
            v1.path(),
            "sys/fs/cgroup/memory/memory.usage_in_bytes",
            "1006632960\n",
        );

        assert_eq!(
            [
                read_available_memory_at(v2.path()).unwrap(),
                read_available_memory_at(v1.path()).unwrap(),
            ],
            [1006632960, 1006632960]
        );
    }

    #[test]
    fn smaller_cgroup_headroom_wins_over_host_memavailable() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "proc/meminfo", "MemAvailable: 4194304 kB\n");
        write(dir.path(), "sys/fs/cgroup/memory.max", "536870912\n");
        write(
            dir.path(),
            "sys/fs/cgroup/memory.stat",
            "anon 67108864\nfile 0\n",
        );

        assert_eq!(read_available_memory_at(dir.path()), Ok(469762048));
    }

    #[test]
    fn failed_read_is_unmeasured() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            measure_encoder_memory_at(dir.path(), Some(0), 1),
            EncoderMemory::Unmeasured
        );
    }

    #[test]
    fn child_rss_reader_uses_the_injected_root() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "proc/42/status",
            "Name:\tffmpeg\nVmRSS:\t123 kB\n",
        );

        assert_eq!(read_child_rss_at(dir.path(), 42), Ok(123 * 1024));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_child_rss_is_positive() {
        let mut child = match Command::new("sleep").arg("30").spawn() {
            Ok(child) => child,
            Err(e) => {
                if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
                    panic!(
                        "NIGHTJAR_TEST_REQUIRE_FFMPEG is set but a child cannot be spawned: {e}"
                    );
                }
                eprintln!("skipping: cannot spawn RSS positive-control child: {e}");
                return;
            }
        };
        // Test-only guess: one second permits one hundred 10 ms polls for the
        // spawned process to appear in procfs; it does not govern admission.
        let deadline = Instant::now() + Duration::from_secs(1);
        let rss = loop {
            if let Ok(rss) = read_child_rss_at(Path::new("/"), child.id()) {
                break Some(rss);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let _ = child.kill();
        let _ = child.wait();
        let Some(rss) = rss else {
            if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
                panic!("NIGHTJAR_TEST_REQUIRE_FFMPEG is set but child RSS could not be measured");
            }
            eprintln!("skipping: child RSS positive control could not be measured");
            return;
        };

        assert!(rss > 0, "a live child must report positive RSS");
    }
}
