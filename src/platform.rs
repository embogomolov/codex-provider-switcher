//! Only OS boundaries live here; catalog, planning, copying and transactions are shared.
use crate::engine::Result;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

#[cfg(windows)]
#[path = "platform/windows.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "platform/macos.rs"]
mod os;
#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("Supported platforms: Windows and macOS");
pub use os::*;

pub(crate) fn is_codex_executable(name: &[u8]) -> bool {
    let name = if name.len() >= 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".exe") {
        &name[..name.len() - 4]
    } else {
        name
    };
    [
        b"codex".as_slice(),
        b"codex-exec",
        b"codex-tui",
        b"codex-app-server",
        b"codex-aarch64-apple-darwin",
        b"codex-x86_64-apple-darwin",
    ]
    .iter()
    .any(|expected| name.eq_ignore_ascii_case(expected))
}

pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::home_dir()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.join(".codex"))
        })
}

pub fn private_file(options: &mut OpenOptions, path: &Path) -> Result<File> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

pub fn staging_file(path: &Path, read: bool) -> Result<File> {
    private_file(
        OpenOptions::new().create_new(true).write(true).read(read),
        path,
    )
}

pub fn finish_staging(source: &File, target: &File) -> Result<()> {
    #[cfg(target_os = "macos")]
    target.set_permissions(source.metadata()?.permissions())?;
    let _ = source;
    target.sync_all()?;
    Ok(())
}

pub fn inherit_permissions(source: &Path, target: &File) -> Result<()> {
    #[cfg(target_os = "macos")]
    match std::fs::metadata(source) {
        Ok(metadata) => target.set_permissions(metadata.permissions())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let _ = (source, target);
    Ok(())
}

pub fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    File::open(path.parent().ok_or("Missing parent directory")?)?.sync_all()?;
    let _ = path;
    Ok(())
}

pub fn configure_db(db: &rusqlite::Connection) -> Result<()> {
    #[cfg(target_os = "macos")]
    db.execute_batch("PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON;")?;
    let _ = db;
    Ok(())
}
