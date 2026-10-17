//! The shared libraries loaded at run time (the NVIDIA driver, nvCOMP, gpuinflate, gzrescue):
//! their file names on each system and the places they are looked for. Loading them at run
//! time keeps one gpuzip binary working on machines that lack some of them.
//!
//! CUDA has no macOS version, so there the GPU libraries have no names and the GPU paths
//! report themselves unavailable; gzrescue (CPU only) is found like everywhere else.

use libloading::Library;
use std::path::PathBuf;

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
/// `env_dir`, the folder that holds this program, then each bare name, which the system
/// searches for itself (PATH on Windows; LD_LIBRARY_PATH and the linker cache on Linux).
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

/// Whether one of `names` is there to load, without loading it: in one of the candidate
/// folders or in a folder of the system's search variable.
pub fn present(env_dir: &str, names: &[String]) -> bool {
    let on_path = std::env::var_os(search_var())
        .map(|p| std::env::split_paths(&p).any(|d| names.iter().any(|n| d.join(n).is_file())))
        .unwrap_or(false);
    on_path || candidates(env_dir, names).iter().any(|p| p.is_file())
}

/// The first candidate that loads, and where it came from
pub fn load_first(candidates: Vec<PathBuf>) -> Option<(Library, String)> {
    candidates.into_iter().find_map(|p| unsafe { Library::new(&p) }.ok().map(|l| (l, p.display().to_string())))
}
