//! `gpuzip doctor --get-nvcomp`: NVIDIA's nvCOMP, which the packages do not hold (it is NVIDIA's
//! library), fetched from PyPI as pip would fetch it. The wheel's size and SHA-256 are checked
//! against what PyPI gives for it, and its library and NVIDIA's license are taken out of it into a
//! folder gpuzip looks in: GPUZIP_NVCOMP_DIR's when set, else the user's own (`user_dir`), so that
//! no administrator is needed and an update of gpuzip's folder leaves it in place.

use crate::ui::say;
use crate::{dylib, gpu, zip};
use sha2::{Digest, Sha256};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PACKAGE: &str = "nvidia-libnvcomp-cu13";
/// PyPI's JSON index; GPUZIP_PYPI_URL stands in for it in tests (plain http only on this machine)
const INDEX: &str = "https://pypi.org/pypi";
/// A wheel larger than this is not taken (nvCOMP 6's are 22-33 MB)
const MAX_WHEEL: u64 = 256 << 20;

/// The wheel chosen: its file name, where it is, its size and SHA-256 (hex), and its version
#[derive(Debug, PartialEq)]
pub struct Wheel {
    pub filename: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub version: String,
}

/// This system's wheel tag, as PyPI's file names carry it: none where nvCOMP has no build
fn platform() -> Option<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some(("win_amd64", "")),
        ("linux", "x86_64") => Some(("manylinux", "x86_64")),
        ("linux", "aarch64") => Some(("manylinux", "aarch64")),
        _ => None,
    }
}

/// The newest release of nvCOMP's major version gpuzip speaks with a wheel for this system, from
/// PyPI's JSON for the package (releases yanked are skipped)
pub fn choose(index: &serde_json::Value, major: u32, tag: (&str, &str)) -> Option<Wheel> {
    let number = |v: &str| v.split('.').map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<u64>>();
    let releases = index.get("releases")?.as_object()?;
    let mut versions: Vec<&String> = releases.keys().filter(|v| number(v).first() == Some(&(major as u64))).collect();
    versions.sort_by_key(|v| std::cmp::Reverse(number(v)));
    versions.into_iter().find_map(|v| {
        releases[v].as_array()?.iter().find_map(|f| {
            let filename = f.get("filename")?.as_str()?;
            let fits = filename.ends_with(".whl") && filename.contains(tag.0) && filename.contains(tag.1);
            if !fits || f.get("yanked").and_then(|y| y.as_bool()) == Some(true) {
                return None;
            }
            Some(Wheel {
                filename: filename.to_string(),
                url: f.get("url")?.as_str()?.to_string(),
                size: f.get("size")?.as_u64()?,
                sha256: f.get("digests")?.get("sha256")?.as_str()?.to_ascii_lowercase(),
                version: v.clone(),
            })
        })
    })
}

/// A member of the wheel (a ZIP) whose name ends with `/name`, decoded and its CRC checked
pub fn member(wheel: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let entries = zip::read_central_directory(wheel, false).map_err(|e| format!("the wheel is not a ZIP: {e}"))?;
    let e = entries
        .iter()
        .find(|e| e.name.rsplit('/').next() == Some(name))
        .ok_or_else(|| format!("the wheel holds no {name}"))?;
    let h = e.data_offset as usize;
    let field = |o: usize| wheel.get(h + o..h + o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let (nlen, xlen) = (field(26).ok_or("a local header lies past the end")?, field(28).ok_or("a local header lies past the end")?);
    let start = h + 30 + nlen + xlen;
    let data = wheel.get(start..start + e.csize as usize).ok_or("a member's data lies past the end")?;
    let out = match e.method {
        0 => data.to_vec(),
        8 => {
            let mut out = vec![0u8; e.size as usize];
            let n = libdeflater::Decompressor::new()
                .deflate_decompress(data, &mut out)
                .map_err(|x| format!("{name}: {x:?}"))?;
            out.truncate(n);
            out
        }
        m => return Err(format!("{name} is compressed with method {m}")),
    };
    if crc32fast::hash(&out) != e.crc || out.len() as u64 != e.size {
        return Err(format!("{name} in the wheel fails its CRC"));
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the library goes: the folder GPUZIP_NVCOMP_DIR names, else the user's own
fn destination() -> Option<PathBuf> {
    std::env::var_os("GPUZIP_NVCOMP_DIR").map(PathBuf::from).or_else(dylib::user_dir)
}

/// The index to ask, and whether plain http may be used: only for a test's own server
fn index_url() -> (String, bool) {
    match std::env::var("GPUZIP_PYPI_URL") {
        Ok(u) => {
            let local = u.starts_with("http://127.0.0.1:") || u.starts_with("http://localhost:");
            (u.trim_end_matches('/').to_string(), local)
        }
        Err(_) => (INDEX.to_string(), false),
    }
}

/// Fetch, check and install nvCOMP; what happened is said on the way. Returns the exit code.
pub fn get() -> i32 {
    match fetch() {
        Ok(path) => {
            say!("nvCOMP:  installed as {}", path.display());
            0
        }
        Err(e) => {
            eprintln!("nvCOMP: {e}");
            1
        }
    }
}

fn fetch() -> Result<PathBuf, String> {
    let tag = platform().ok_or("NVIDIA builds nvCOMP for Windows and Linux (x86-64, Arm) only")?;
    let names = gpu::nvcomp_names();
    let lib_name = names.first().ok_or("no CUDA on this system")?.clone();
    if let Err(why) = gpu::driver_sees_gpu() {
        say!("Note: {why}; nvCOMP is fetched all the same, and used once there is an NVIDIA GPU");
    }
    let dir = destination().ok_or("no folder to put it in: set GPUZIP_NVCOMP_DIR")?;
    let (index, plain_ok) = index_url();
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .https_only(!plain_ok)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .user_agent(concat!("gpuzip/", env!("CARGO_PKG_VERSION")))
            .build(),
    );
    let url = format!("{index}/{PACKAGE}/json");
    let text = agent
        .get(&url)
        .call()
        .map_err(|e| format!("cannot read PyPI's index ({url}): {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("cannot read PyPI's index ({url}): {e}"))?;
    let listing: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("PyPI's index for {PACKAGE} is not what was expected: {e}"))?;
    let wheel = choose(&listing, gpu::NVCOMP_MAJOR, tag)
        .ok_or_else(|| format!("PyPI has no {PACKAGE} {}.x for this system", gpu::NVCOMP_MAJOR))?;
    if wheel.size > MAX_WHEEL {
        return Err(format!("{} is {} bytes: more than a wheel of nvCOMP should be", wheel.filename, wheel.size));
    }
    say!("Fetching {} ({}, nvCOMP {}) from PyPI, NVIDIA's license with it", wheel.filename, crate::human(wheel.size), wheel.version);
    let mut response = agent.get(&wheel.url).call().map_err(|e| format!("cannot download {}: {e}", wheel.url))?;
    let mut reader = response.body_mut().with_config().limit(MAX_WHEEL).reader();
    let mut data = Vec::with_capacity(wheel.size as usize);
    let mut buf = vec![0u8; 1 << 20];
    let show = std::io::stderr().is_terminal();
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("the download broke off: {e}"))?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if show {
            eprint!("\r  {} of {}", crate::human(data.len() as u64), crate::human(wheel.size));
        }
    }
    if show {
        eprintln!();
    }
    if data.len() as u64 != wheel.size {
        return Err(format!("{} came as {} bytes, not the {} PyPI gives", wheel.filename, data.len(), wheel.size));
    }
    let digest = hex(&Sha256::digest(&data));
    if digest != wheel.sha256 {
        return Err(format!("{}'s SHA-256 is {digest}, not the {} PyPI gives: not installed", wheel.filename, wheel.sha256));
    }
    let library = member(&data, &lib_name)?;
    let license = member(&data, "LICENSE").ok();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(&lib_name);
    write(&path, &library)?;
    if let Some(l) = license {
        write(&dir.join("nvcomp-LICENSE.txt"), &l)?;
    }
    Ok(path)
}

/// Written whole or not at all (crate::outfile), over an older copy
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut f = crate::outfile::OutFile::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    f.finish(None).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> serde_json::Value {
        let file = |name: &str, yanked: bool| {
            serde_json::json!({"filename": name, "url": format!("https://files.example/{name}"), "size": 10,
                               "digests": {"sha256": "AB"}, "yanked": yanked})
        };
        serde_json::json!({"releases": {
            "5.3.0.16": [file("nvidia_libnvcomp_cu13-5.3.0.16-py3-none-win_amd64.whl", false)],
            "6.0.0.12": [file("nvidia_libnvcomp_cu13-6.0.0.12-py3-none-manylinux_2_24_x86_64.whl", false),
                         file("nvidia_libnvcomp_cu13-6.0.0.12-py3-none-win_amd64.whl", false)],
            "6.1.0.1": [file("nvidia_libnvcomp_cu13-6.1.0.1-py3-none-win_amd64.whl", true)],
            "6.0.10.1": [file("nvidia_libnvcomp_cu13-6.0.10.1-py3-none-manylinux_2_28_aarch64.whl", false)],
            "7.0.0.1": [file("nvidia_libnvcomp_cu13-7.0.0.1-py3-none-win_amd64.whl", false)],
        }})
    }

    #[test]
    fn the_newest_wheel_of_the_major_version_for_this_system() {
        let w = choose(&index(), 6, ("win_amd64", "")).unwrap();
        assert_eq!((w.version.as_str(), w.sha256.as_str()), ("6.0.0.12", "ab")); // 6.1 is yanked, 7 is another major
        assert_eq!(choose(&index(), 6, ("manylinux", "aarch64")).unwrap().version, "6.0.10.1"); // 6.0.10 > 6.0.0
        assert_eq!(choose(&index(), 6, ("manylinux", "x86_64")).unwrap().version, "6.0.0.12");
        assert!(choose(&index(), 8, ("win_amd64", "")).is_none());
    }
}
