//! A file an extraction writes, which takes its own name only once it is whole: an extraction
//! stopped by Ctrl-C, a kill or a crash must not leave half a file under the name of a good one.
//!
//! On Windows the file is created under its name but marked to be deleted when its handle
//! closes, and unmarked once whole: if the process dies, Windows closes the handle and the file
//! goes. Writing it under another name and renaming it instead cost 260-320 µs a file on this
//! machine's NTFS (20,000 files of 6 KB: 60-116% longer); the mark costs 0-7 µs. Elsewhere a
//! rename is cheap: the file is written as NAME.gpuzip-part and renamed, and a process that dies
//! leaves that name, never the real one.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct OutFile {
    file: Option<File>,
    path: PathBuf,
    #[cfg(windows)]
    marked: bool, // false where the file system refuses the mark: then removed here if not whole
    #[cfg(not(windows))]
    tmp: PathBuf,
    done: bool,
}

impl OutFile {
    pub fn create(path: &Path) -> io::Result<OutFile> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const DELETE: u32 = 0x0001_0000;
            let file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).access_mode(GENERIC_WRITE | DELETE).open(path)?;
            let marked = set_delete_on_close(&file, true);
            Ok(OutFile { file: Some(file), path: path.to_path_buf(), marked, done: false })
        }
        #[cfg(not(windows))]
        {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(".gpuzip-part");
            let tmp = path.with_file_name(name);
            // a new file, never one already there (a link an archive planted would be followed)
            let open = || std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp);
            let file = match open() {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    std::fs::remove_file(&tmp)?;
                    open()?
                }
                r => r?,
            };
            Ok(OutFile { file: Some(file), path: path.to_path_buf(), tmp, done: false })
        }
    }

    /// The file being written (to set its permissions before it is finished)
    #[cfg(unix)]
    pub fn file(&self) -> &File {
        self.file.as_ref().unwrap()
    }

    /// The file is whole: it takes its name (and `mtime`, when given)
    pub fn finish(mut self, mtime: Option<SystemTime>) -> io::Result<()> {
        let file = self.file.as_ref().unwrap();
        #[cfg(windows)]
        if self.marked && !set_delete_on_close(file, false) {
            return Err(io::Error::last_os_error()); // still marked: it goes when dropped
        }
        if let Some(t) = mtime {
            let _ = file.set_modified(t);
        }
        #[cfg(not(windows))]
        {
            drop(self.file.take());
            std::fs::rename(&self.tmp, &self.path)?;
        }
        self.done = true;
        Ok(())
    }
}

impl Write for OutFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.as_mut().unwrap().write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file.as_mut().unwrap().write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().unwrap().flush()
    }
}

impl Drop for OutFile {
    // not whole (an error, a wrong CRC): nothing stays under its name
    fn drop(&mut self) {
        if self.done {
            return;
        }
        drop(self.file.take());
        #[cfg(windows)]
        if !self.marked {
            let _ = std::fs::remove_file(&self.path);
        }
        #[cfg(not(windows))]
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// Mark (or unmark) the file to be deleted when its last handle closes (FileDispositionInfo;
/// the handle needs DELETE access). Whether it worked.
#[cfg(windows)]
fn set_delete_on_close(file: &File, delete: bool) -> bool {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileInformationByHandle(file: *mut std::ffi::c_void, class: i32, info: *const u8, size: u32) -> i32;
    }
    const FILE_DISPOSITION_INFO: i32 = 4;
    let flag = delete as u8; // FILE_DISPOSITION_INFO is one BOOLEAN
    // SAFETY: a valid handle, and a 1-byte buffer whose size is passed
    unsafe { SetFileInformationByHandle(file.as_raw_handle(), FILE_DISPOSITION_INFO, &flag, 1) != 0 }
}
