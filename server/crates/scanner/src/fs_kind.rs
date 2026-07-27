//! Classify library roots as local vs network for watcher policy.
//!
//! Notify is trustworthy on local disks. On network mounts (SMB/NFS/…) it can
//! arm successfully and still never deliver create events, so those roots must
//! keep polling (ADR-0013).

use std::path::Path;

/// True when `path` lives on a network-backed filesystem.
///
/// On failure to classify, returns `false` (treat as local): a false local
/// means we may drop poll after notify arms; a false network would leave a
/// local disk polling forever — the cheaper mistake for a home server.
pub fn is_network_fs(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_is_network(path)
    }
    #[cfg(target_os = "linux")]
    {
        linux_is_network(path)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        false
    }
}

#[cfg(target_os = "macos")]
fn macos_is_network(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Layout matches Darwin sys/mount.h (arm64/x86_64). Verified sizeof=2168.
    #[repr(C)]
    struct Statfs {
        f_bsize: u32,
        f_iosize: i32,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_fsid: [i32; 2],
        f_owner: u32,
        f_type: u32,
        f_flags: u32,
        f_fssubtype: u32,
        f_fstypename: [u8; 16],
        f_mntonname: [u8; 1024],
        f_mntfromname: [u8; 1024],
        f_flags_ext: u32,
        f_reserved: [u32; 7],
    }

    const MNT_LOCAL: u32 = 0x0000_1000;

    #[allow(non_camel_case_types)]
    type c_char = i8;

    unsafe extern "C" {
        fn statfs(path: *const c_char, buf: *mut Statfs) -> i32;
    }

    debug_assert_eq!(std::mem::size_of::<Statfs>(), 2168);

    let Some(c_path) = CString::new(path.as_os_str().as_bytes()).ok() else {
        return false;
    };
    let mut buf = unsafe { std::mem::zeroed::<Statfs>() };
    let rc = unsafe { statfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return false;
    }
    (buf.f_flags & MNT_LOCAL) == 0
}

#[cfg(target_os = "linux")]
fn linux_is_network(path: &Path) -> bool {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let mut best_len = 0usize;
    let mut best_fstype = None::<&str>;
    for line in mountinfo.lines() {
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let pre_fields: Vec<&str> = pre.split_whitespace().collect();
        // id parent maj:min root mountpoint …
        if pre_fields.len() < 5 {
            continue;
        }
        let mount_point = pre_fields[4];
        let mut post_fields = post.split_whitespace();
        let Some(fstype) = post_fields.next() else {
            continue;
        };
        if !path_is_under(&canon, Path::new(mount_point)) {
            continue;
        }
        let len = mount_point.len();
        if len >= best_len {
            best_len = len;
            best_fstype = Some(fstype);
        }
    }
    best_fstype.map(fstype_is_network).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn path_is_under(path: &Path, root: &Path) -> bool {
    if root == Path::new("/") {
        return true;
    }
    path.starts_with(root)
}

#[cfg(target_os = "linux")]
fn fstype_is_network(fstype: &str) -> bool {
    matches!(
        fstype,
        "cifs" | "smb3" | "smb" | "nfs" | "nfs4" | "afs" | "fuse.sshfs" | "fuse.rclone"
    ) || (fstype.starts_with("fuse.") && (fstype.contains("smb") || fstype.contains("nfs")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_is_local() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !is_network_fs(dir.path()),
            "tempdir should classify as local"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn network_fstypes() {
        assert!(fstype_is_network("cifs"));
        assert!(fstype_is_network("nfs4"));
        assert!(!fstype_is_network("ext4"));
        assert!(!fstype_is_network("tmpfs"));
    }
}
