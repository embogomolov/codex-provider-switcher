use crate::{app::Stamp, engine::Result};
use std::{
    ffi::{CString, OsStr},
    fs::{self, File},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

pub fn codex_pids() -> Result<Vec<u32>> {
    // libproc reports a PID count here, but takes bytes for the second argument.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut pids = vec![0 as libc::pid_t; count as usize + 256];
    for _ in 0..4 {
        let bytes = pids
            .len()
            .checked_mul(std::mem::size_of::<libc::pid_t>())
            .ok_or("Process list overflow")?;
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes.try_into()?) };
        if count <= 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if count as usize >= pids.len() {
            pids.resize(pids.len().checked_mul(2).ok_or("Process list overflow")?, 0);
            continue;
        }
        pids.truncate(count as usize);
        let mut found = Vec::new();
        for pid in pids.into_iter().filter(|p| *p > 0) {
            let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of_val(&info) as libc::c_int;
            // SHORTBSDINFO is available across UIDs, unlike proc_name/TBSDINFO.
            let n = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDT_SHORTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdshortinfo).cast(),
                    size,
                )
            };
            if n != size {
                let error = std::io::Error::last_os_error();
                if n == 0 && error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(format!(
                    "Cannot inspect process {pid}: {error} (returned {n}/{size} bytes)"
                )
                .into());
            }
            let name = info.pbsi_comm.map(|c| c as u8);
            if is_codex(nul_bytes(&name)) {
                found.push(pid as u32);
            } else if name[..6].eq_ignore_ascii_case(b"codex-") {
                // p_comm has only 15 useful bytes. Resolve full names only for
                // candidates, e.g. codex-app-server and release archive binaries.
                let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
                let n =
                    unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
                if n <= 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::ESRCH) {
                        continue;
                    }
                    return Err(format!("Cannot inspect executable for process {pid}: {e}").into());
                }
                let basename = nul_bytes(&path)
                    .rsplit(|&c| c == b'/')
                    .next()
                    .unwrap_or_default();
                if super::is_codex_executable(basename) {
                    found.push(pid as u32);
                }
            }
        }
        return Ok(found);
    }
    Err("Process list kept growing; cannot confirm Codex is closed".into())
}

fn nul_bytes(value: &[u8]) -> &[u8] {
    &value[..value.iter().position(|&c| c == 0).unwrap_or(value.len())]
}

fn is_codex(name: &[u8]) -> bool {
    super::is_codex_executable(name)
        || name
            .get(..12)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"codex helper"))
}

pub fn stamp_file(file: &File) -> Result<Stamp> {
    let m = file.metadata()?;
    let ns = |s: i64, n: i64| -> Result<i64> {
        Ok((i128::from(s) * 1_000_000_000 + i128::from(n)).try_into()?)
    };
    Ok(Stamp {
        size: m.len(),
        mtime: ns(m.mtime(), m.mtime_nsec())?,
        atime: ns(m.atime(), m.atime_nsec())?,
        links: m.nlink(),
    })
}

fn filesystem(path: &Path) -> Result<libc::statfs> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stats) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stats)
}

pub fn volume(path: &Path) -> Result<PathBuf> {
    let stats = filesystem(path)?;
    let mount = stats.f_mntonname.map(|c| c as u8);
    let path = PathBuf::from(OsStr::from_bytes(nul_bytes(&mount)));
    if !path.is_absolute() {
        return Err("Invalid filesystem mount point".into());
    }
    Ok(path)
}

pub fn available_space(path: &Path) -> Result<u64> {
    let stats = filesystem(path)?;
    stats
        .f_bavail
        .checked_mul(u64::from(stats.f_bsize))
        .ok_or_else(|| "Filesystem capacity overflow".into())
}

pub fn replace(src: &Path, dst: &Path) -> Result<()> {
    fs::rename(src, dst)?;
    super::sync_parent(dst)?;
    if src.parent() != dst.parent() {
        super::sync_parent(src)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_names() {
        for name in [b"codex".as_slice(), b"Codex", b"Codex Helper (Re"] {
            assert!(is_codex(name));
        }
        for name in [
            b"codex-provider".as_slice(),
            b"provider-migrate",
            b"Terminal",
        ] {
            assert!(!is_codex(name));
        }
        assert_eq!(nul_bytes(&[67, 0, 88]), b"C");
    }
}
