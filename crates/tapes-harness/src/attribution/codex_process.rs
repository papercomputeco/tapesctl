//! Process-local helpers for Codex transcript attribution.
//!
//! Codex JSONL transcripts are not PID-indexed, so the daemon cannot do
//! the Claude-style `<pid>.json` lookup. Once the accepted loopback socket
//! identifies the calling PID, this module inspects that process's open
//! files for Codex transcript JSONL paths.

use std::path::{Path, PathBuf};

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
fn session_open_by_pid(pid: i32, path: &Path) -> bool {
    let Ok(target) = path.canonicalize() else {
        return false;
    };
    open_files_by_pid(pid)
        .into_iter()
        .any(|link| link == target)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn open_jsonl_sessions_by_pid(pid: i32) -> Vec<PathBuf> {
    open_files_by_pid(pid)
        .into_iter()
        .filter(|path| is_codex_jsonl_session_path(path))
        .collect()
}

#[cfg(target_os = "linux")]
fn open_files_by_pid(pid: i32) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            std::fs::read_link(entry.path())
                .ok()
                .and_then(|link| link.canonicalize().ok())
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn open_files_by_pid(pid: i32) -> Vec<PathBuf> {
    use std::ffi::CStr;
    use std::mem::{MaybeUninit, size_of};
    use std::os::raw::c_char;

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: libc::off_t,
        fi_type: i32,
        fi_guardflags: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct VnodeFdInfoWithPath {
        pfi: ProcFileInfo,
        pvip: libc::vnode_info_path,
    }

    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

    let bytes =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    let Ok(required_bytes) = usize::try_from(bytes) else {
        return Vec::new();
    };
    let fd_info_size = size_of::<libc::proc_fdinfo>();
    let required_fds = required_bytes.div_ceil(fd_info_size);
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        required_fds.saturating_add(64)
    ];
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            i32::try_from(fds.len() * fd_info_size).unwrap_or(i32::MAX),
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    let Ok(bytes) = usize::try_from(bytes) else {
        return Vec::new();
    };
    fds.truncate(bytes / fd_info_size);

    fds.into_iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .filter_map(|fd| {
            let mut info = MaybeUninit::<VnodeFdInfoWithPath>::zeroed();
            let bytes = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    info.as_mut_ptr().cast(),
                    i32::try_from(size_of::<VnodeFdInfoWithPath>()).unwrap_or(i32::MAX),
                )
            };
            if bytes < i32::try_from(size_of::<VnodeFdInfoWithPath>()).unwrap_or(i32::MAX) {
                return None;
            }
            let info = unsafe { info.assume_init() };
            let cstr = unsafe { CStr::from_ptr(info.pvip.vip_path.as_ptr().cast::<c_char>()) };
            cstr.to_str()
                .ok()
                .and_then(|p| Path::new(p).canonicalize().ok())
        })
        .collect()
}

#[cfg(all(test, not(any(target_os = "linux", target_os = "macos"))))]
fn session_open_by_pid(_pid: i32, _path: &Path) -> bool {
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn open_jsonl_sessions_by_pid(_pid: i32) -> Vec<PathBuf> {
    Vec::new()
}

fn is_codex_jsonl_session_path(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        && path
            .components()
            .any(|component| component.as_os_str() == "sessions")
}

#[cfg(test)]
mod tests {
    use super::{open_jsonl_sessions_by_pid, session_open_by_pid};

    #[test]
    fn current_process_reports_open_file() -> std::io::Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;
        let _held = std::fs::File::open(tmp.path())?;

        assert!(session_open_by_pid(std::process::id() as i32, tmp.path()));
        Ok(())
    }

    #[test]
    fn current_process_reports_missing_file_as_closed() -> std::io::Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;

        assert!(!session_open_by_pid(
            std::process::id() as i32,
            &tmp.path().with_extension("missing"),
        ));
        Ok(())
    }

    #[test]
    fn current_process_reports_open_session_jsonl() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let sessions = dir
            .path()
            .join("sessions")
            .join("2026")
            .join("06")
            .join("18");
        std::fs::create_dir_all(&sessions)?;
        let path = sessions.join("rollout-test.jsonl");
        std::fs::write(&path, "{}\n")?;
        let _held = std::fs::File::open(&path)?;

        let paths = open_jsonl_sessions_by_pid(std::process::id() as i32);
        let canonical_path = path.canonicalize()?;
        assert!(paths.iter().any(|p| p == &canonical_path));
        Ok(())
    }
}
