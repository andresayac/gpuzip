//! The shared libraries loaded at run time (the NVIDIA driver, nvCOMP, gpuinflate, gzrescue):
//! their file names on each system and the places they are looked for. Loading them at run
//! time keeps one gpuzip binary working on machines that lack some of them.
//!
//! CUDA has no macOS version, so there the GPU libraries have no names and the GPU paths
//! report themselves unavailable; gzrescue (CPU only) is found like everywhere else.

use libloading::Library;
use std::path::{Path, PathBuf};

/// Whether this system can have an NVIDIA GPU with CUDA at all
pub const CUDA: bool = cfg!(any(windows, target_os = "linux"));

/// A library built from this repository (gpuinflate, gzrescue) as its file is named here:
/// gzrescue.dll on Windows, libgzrescue.so on Linux, libgzrescue.dylib on macOS.
pub fn file_name(stem: &str) -> String {
    format!("{}{stem}{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX)
}

/// This program's own file name, for messages: gpuzip.exe on Windows, gpuzip elsewhere
pub fn exe_name() -> String {
    format!("gpuzip{}", std::env::consts::EXE_SUFFIX)
}

/// The variable that lists the system's library folders: PATH on Windows, LD_LIBRARY_PATH on
/// Linux, DYLD_LIBRARY_PATH on macOS.
fn search_var() -> &'static str {
    if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// Places a library is looked for, in order: the folder named by the environment variable
/// `env_dir`, the folder that holds this program, then each bare name: on Linux and macOS the
/// system searches for it (LD_LIBRARY_PATH and the linker cache), on Windows load_first looks
/// in PATH's folders (never in the current folder, see `harden`).
pub fn candidates(env_dir: &str, names: &[String]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os(env_dir) {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    let mut out: Vec<PathBuf> = dirs.iter().flat_map(|d| names.iter().map(move |n| d.join(n))).collect();
    out.extend(names.iter().map(PathBuf::from));
    out
}

/// The folders of the system's search variable, absolute ones only: a relative one (".", or
/// none at all) is the current folder
fn search_dirs() -> Vec<PathBuf> {
    std::env::var_os(search_var()).map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).collect()).unwrap_or_default()
}

/// Whether one of `names` is there to load, without loading it: in one of the candidate
/// folders or in a folder of the system's search variable.
pub fn present(env_dir: &str, names: &[String]) -> bool {
    let on_path = search_dirs().iter().any(|d| names.iter().any(|n| d.join(n).is_file()));
    on_path || candidates(env_dir, names).iter().any(|p| p.is_absolute() && p.is_file())
}

/// The first candidate that loads, and where it came from. On Windows each is loaded by its
/// full path, a bare name from the first of PATH's folders that holds it: given a bare name,
/// Windows looks in the current folder before PATH, and a DLL left there (a download's, beside
/// the archive) would run inside gpuzip. What a library itself needs (nvCOMP the C++ runtime)
/// is looked for in its own folder, gpuzip's and System32.
pub fn load_first(candidates: Vec<PathBuf>) -> Option<(Library, String)> {
    candidates.into_iter().find_map(|p| {
        let p = if cfg!(windows) && !p.is_absolute() {
            if p.components().count() != 1 {
                return None;
            }
            search_dirs().into_iter().map(|d| d.join(&p)).find(|f| f.is_file())?
        } else {
            p
        };
        unsafe { open(&p) }.ok().map(|l| (l, p.display().to_string()))
    })
}

#[cfg(windows)]
unsafe fn open(p: &Path) -> Result<Library, libloading::Error> {
    use libloading::os::windows::{LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, Library as WinLibrary};
    let full = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    unsafe { WinLibrary::load_with_flags(&full, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) }.map(Library::from)
}

#[cfg(not(windows))]
unsafe fn open(p: &Path) -> Result<Library, libloading::Error> {
    unsafe { Library::new(p) }
}

/// A library of the system's own (the NVIDIA driver): on Windows from System32 alone, where the
/// driver puts it; elsewhere as the system finds it
pub fn system_library(name: &str) -> Result<Library, libloading::Error> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library as WinLibrary};
        unsafe { WinLibrary::load_with_flags(name, LOAD_LIBRARY_SEARCH_SYSTEM32) }.map(Library::from)
    }
    #[cfg(not(windows))]
    unsafe {
        Library::new(name)
    }
}

/// Windows: whatever this process loads by a bare name from here on, the libraries above and
/// what they load in turn (CUDA's runtime loads the driver), is looked for in gpuzip's folder and
/// System32 only, not the current folder nor PATH. Called first thing in main.
pub fn harden() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetDefaultDllDirectories(flags: u32) -> i32;
        }
        const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x1000;
        unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) };
    }
}
