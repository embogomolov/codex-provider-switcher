use crate::{app::Stamp, engine::Result};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

pub fn codex_pids() -> Result<Vec<u32>> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::*,
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry);
        let mut pids = vec![];
        while more != 0 {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            if super::is_codex_executable(
                String::from_utf16_lossy(&entry.szExeFile[..end]).as_bytes(),
            ) {
                pids.push(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry);
        }
        let error = GetLastError();
        CloseHandle(snapshot);
        if error != ERROR_NO_MORE_FILES {
            return Err(std::io::Error::from_raw_os_error(error as i32).into());
        }
        Ok(pids)
    }
}

pub fn replace(src: &Path, dst: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let a: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            a.as_ptr(),
            b.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn stamp_file(f: &File) -> Result<Stamp> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let ns = |v: windows_sys::Win32::Foundation::FILETIME| -> Result<i64> {
        let ticks = ((v.dwHighDateTime as u64) << 32) | v.dwLowDateTime as u64;
        Ok(((ticks as i128 - 116_444_736_000_000_000) * 100).try_into()?)
    };
    Ok(Stamp {
        size: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
        mtime: ns(info.ftLastWriteTime)?,
        atime: ns(info.ftLastAccessTime)?,
        links: info.nNumberOfLinks as u64,
    })
}
pub fn available_space(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0;
    if unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(available)
}

pub fn volume(path: &Path) -> Result<PathBuf> {
    Ok(path.ancestors().last().ok_or("Invalid volume")?.to_owned())
}
