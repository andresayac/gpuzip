//! 7-Zip, for what gpuzip does not decode itself: encrypted entries, methods other than
//! stored and Deflate, and whole RAR, 7z and tar.* archives.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const TAR_SUFFIXES: [&str; 10] =
    [".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".tar.zst", ".tzst", ".tar.lz", ".tar.lzma"];

pub fn find(explicit: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = explicit.map(Path::to_path_buf).into_iter().collect();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("7z.exe"));
            candidates.push(dir.join("7za.exe"));
        }
    }
    candidates.push(PathBuf::from(r"C:\Program Files\7-Zip\7z.exe"));
    candidates.push(PathBuf::from(r"C:\Program Files (x86)\7-Zip\7z.exe"));
    candidates.into_iter().find(|p| p.is_file())
}

pub fn version(exe: &Path) -> String {
    let out = Command::new(exe).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let line = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("7-Zip");
    line.split(':').next().unwrap_or(line).trim().to_string()
}

fn quiet(verbose: bool) -> Stdio {
    if verbose { Stdio::inherit() } else { Stdio::null() }
}

fn code(status: std::io::Result<std::process::ExitStatus>) -> i32 {
    status.ok().and_then(|s| s.code()).unwrap_or(-1)
}

/// Extract (or test, with no `out`) the named entries of a ZIP file.
pub fn run_entries(exe: &Path, archive: &str, names: &[&str], out: Option<&Path>, password: Option<&str>, verbose: bool) -> i32 {
    let list = std::env::temp_dir().join(format!("gpuzip-{}-{}.txt", std::process::id(), names.len()));
    if std::fs::File::create(&list).and_then(|mut f| f.write_all(names.join("\n").as_bytes())).is_err() {
        return -1;
    }
    let mut cmd = Command::new(exe);
    cmd.arg(if out.is_some() { "x" } else { "t" }).arg(archive).arg("-y").arg("-scsUTF-8").arg(format!("@{}", list.display()));
    if let Some(o) = out {
        cmd.arg(format!("-o{}", o.display()));
    }
    if let Some(p) = password {
        cmd.arg(format!("-p{p}"));
    }
    let rc = code(cmd.stdout(quiet(verbose)).status());
    let _ = std::fs::remove_file(&list);
    rc
}

/// Extract (or test, with no `out`) a whole non-ZIP archive.
pub fn run_archive(exe: &Path, archive: &str, out: Option<&Path>, password: Option<&str>, verbose: bool) -> i32 {
    let verb = if out.is_some() { "x" } else { "t" };
    let out_arg = out.map(|o| format!("-o{}", o.display()));
    let pw = password.map(|p| format!("-p{p}"));
    let lower = archive.to_lowercase();
    if TAR_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        // .tar.gz and friends: unpack the outer layer to a pipe, then the tar from it
        let first = Command::new(exe)
            .args(["x", archive, "-so", "-y"])
            .args(pw.iter())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut first) = first else { return -1 };
        let pipe = first.stdout.take().unwrap();
        let second = Command::new(exe).args([verb, "-si", "-ttar", "-y"]).args(out_arg.iter()).stdin(pipe).stdout(quiet(verbose)).status();
        let a = code(first.wait());
        let b = code(second);
        return if a != 0 { a } else { b };
    }
    code(Command::new(exe).args([verb, archive, "-y"]).args(pw.iter()).args(out_arg.iter()).stdout(quiet(verbose)).status())
}

pub fn list(exe: &Path, archive: &str) -> i32 {
    code(Command::new(exe).args(["l", archive]).status())
}
