//! Puts a file on the clipboard the way Explorer's "Copy" does (CF_HDROP), so Ctrl+V in
//! Discord attaches the clip itself.

use anyhow::{Context, Result};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::DROPFILES;

pub fn copy_file(path: &Path) -> Result<()> {
    let path = std::fs::canonicalize(path).context("clip not found")?;
    // canonicalize gives a \\?\ path; Discord and Explorer want the plain form.
    let plain = path.to_string_lossy().trim_start_matches(r"\\?\").to_owned();
    let wide: Vec<u16> = std::ffi::OsStr::new(&plain).encode_wide().chain([0, 0]).collect();
    let header = size_of::<DROPFILES>();
    let size = header + wide.len() * 2;
    unsafe {
        let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size)?;
        let p = GlobalLock(mem) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(mem));
            anyhow::bail!("GlobalLock failed");
        }
        let df = p as *mut DROPFILES;
        (*df).pFiles = header as u32;
        (*df).fWide = true.into();
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, p.add(header), wide.len() * 2);
        let _ = GlobalUnlock(mem);

        // Another app may hold the clipboard for a moment; retry briefly.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        if !opened {
            let _ = GlobalFree(Some(mem));
            anyhow::bail!("the clipboard is busy, try again");
        }
        let res = EmptyClipboard().and_then(|_| SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(mem.0))));
        let _ = CloseClipboard();
        if let Err(e) = res {
            let _ = GlobalFree(Some(mem));
            return Err(e).context("SetClipboardData");
        }
        // On success the clipboard owns `mem`.
    }
    Ok(())
}
