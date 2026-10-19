//! One large Deflate stream decoded in parallel on the GPU by gpuinflate (gpuinflate.dll,
//! libgpuinflate.so; built from ../gpuinflate), loaded at run time like nvCOMP so the program
//! also runs without it.
//!
//! The library decodes the stream in segments and hands the output over in order, from a
//! thread of its own, while it decodes the next segment.

use crate::dylib;
use libloading::Library;
use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const ABI_VERSION: u32 = 1;

#[repr(C)]
struct Config {
    size: u32,
    device: i32,
    segment_out_mb: u32,
    segment_in_kb: u32,
}

/// What one call did (gi_stats)
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub in_used: u64,
    pub out_size: u64,
    pub segments: u32,
    pub rounds: u32,
    pub jobs: u64,
    pub seconds: f64,
    pub upload_s: f64,
    pub find_s: f64,
    pub decode_s: f64,
    pub resolve_s: f64,
    pub download_s: f64,
}

pub enum Error {
    /// the stream is not valid Deflate
    Data(String),
    /// the input ends inside the stream
    Truncated(String),
    /// the sink asked to stop
    Aborted,
    /// a GPU problem: decode on the CPU instead
    Gpu(String),
}

type Sink = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> i32;
type SinkFn<'a> = &'a mut (dyn FnMut(&[u8]) -> bool + Send);

struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}

pub struct Gpu {
    _lib: std::mem::ManuallyDrop<Library>,
    ctx: Mutex<Ctx>, // a context serves one call at a time
    pub device: String,
    pub path: String,
    inflate: unsafe extern "C" fn(*mut c_void, *const u8, u64, u64, Option<Sink>, *mut c_void, *mut Stats) -> i32,
    last_error: unsafe extern "C" fn(*const c_void) -> *const c_char,
}

unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

/// gpuinflate's file name here: none where there is no CUDA
fn names() -> Vec<String> {
    if dylib::CUDA { vec![dylib::file_name("gpuinflate")] } else { Vec::new() }
}

/// Places gpuinflate is looked for, in order.
pub fn candidates() -> Vec<PathBuf> {
    dylib::candidates("GPUZIP_INFLATE_DIR", &names())
}

/// Whether gpuinflate is there to load, without loading it
pub fn available() -> bool {
    dylib::present("GPUZIP_INFLATE_DIR", &names())
}

/// The library and its GPU buffers, set up once per process on first use.
pub fn get() -> Result<&'static Gpu, String> {
    static GPU: OnceLock<Result<Gpu, String>> = OnceLock::new();
    GPU.get_or_init(Gpu::load).as_ref().map_err(|e| e.clone())
}

unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
    unsafe {
        lib.get::<T>(name)
            .map(|s| *s)
            .map_err(|e| format!("{} missing: {e}", String::from_utf8_lossy(&name[..name.len() - 1])))
    }
}

unsafe extern "C" fn trampoline(user: *mut c_void, data: *const u8, len: usize) -> i32 {
    let f = unsafe { &mut *(user as *mut SinkFn) };
    let chunk = unsafe { std::slice::from_raw_parts(data, len) };
    // a panic must not unwind into the library
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(chunk))) {
        Ok(true) => 0,
        _ => 1,
    }
}

impl Gpu {
    fn load() -> Result<Gpu, String> {
        unsafe {
            if !dylib::CUDA {
                return Err("no CUDA on this system".into());
            }
            crate::gpu::driver_sees_gpu()?;
            let name = dylib::file_name("gpuinflate");
            let (lib, path) = dylib::load_first(candidates()).ok_or_else(|| {
                format!("{name} not found: put it next to {} or set GPUZIP_INFLATE_DIR", dylib::exe_name())
            })?;
            // never unloaded, even when it cannot be used: unloading its CUDA runtime after a
            // failed start is what may crash (the library lives as long as the process anyway)
            let lib = std::mem::ManuallyDrop::new(lib);
            let abi: unsafe extern "C" fn() -> u32 = sym(&lib, b"gi_abi_version\0")?;
            if abi() != ABI_VERSION {
                return Err(format!("{name} speaks version {} of the interface, this gpuzip {ABI_VERSION}", abi()));
            }
            let create: unsafe extern "C" fn(*const Config, *mut *mut c_void, *mut c_char, usize) -> i32 =
                sym(&lib, b"gi_create\0")?;
            let device_name: unsafe extern "C" fn(*const c_void) -> *const c_char = sym(&lib, b"gi_device_name\0")?;
            let cfg = Config { size: std::mem::size_of::<Config>() as u32, device: 0, segment_out_mb: 0, segment_in_kb: 0 };
            let mut ctx = std::ptr::null_mut();
            let mut err = [0 as c_char; 512];
            if create(&cfg, &mut ctx, err.as_mut_ptr(), err.len()) != 0 {
                return Err(format!("GPU not usable: {}", CStr::from_ptr(err.as_ptr()).to_string_lossy()));
            }
            let device = CStr::from_ptr(device_name(ctx)).to_string_lossy().into_owned();
            Ok(Gpu {
                inflate: sym(&lib, b"gi_inflate\0")?,
                last_error: sym(&lib, b"gi_last_error\0")?,
                _lib: lib,
                ctx: Mutex::new(Ctx(ctx)),
                device,
                path,
            })
        }
    }

    /// Decode the raw Deflate stream at the start of `input` (bytes after its last block are
    /// allowed), passing the output to `sink` in order; `sink` returns false to stop.
    /// `hint`: the expected output size, or 0.
    pub fn inflate(&self, input: &[u8], hint: u64, mut sink: impl FnMut(&[u8]) -> bool + Send) -> Result<Stats, Error> {
        let ctx = self.ctx.lock().unwrap_or_else(|p| p.into_inner());
        let mut stats = Stats::default();
        let mut f: SinkFn = &mut sink;
        let user = &mut f as *mut SinkFn as *mut c_void;
        let r = unsafe {
            (self.inflate)(ctx.0, input.as_ptr(), input.len() as u64, hint, Some(trampoline), user, &mut stats)
        };
        let msg = || unsafe { CStr::from_ptr((self.last_error)(ctx.0)) }.to_string_lossy().into_owned();
        match r {
            0 => Ok(stats),
            1 => Err(Error::Data(msg())),
            2 => Err(Error::Truncated(msg())),
            3 => Err(Error::Aborted),
            _ => Err(Error::Gpu(msg())),
        }
    }
}
