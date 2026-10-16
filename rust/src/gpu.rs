//! Decoding batches of raw Deflate streams on an NVIDIA GPU with nvCOMP.
//!
//! The NVIDIA driver (nvcuda.dll) and nvCOMP (nvcomp64_6.dll) are loaded at run time, so the
//! program starts and runs on the CPU on machines without them. nvCOMP links the CUDA runtime
//! statically and works on the driver's primary context, which this module makes current.

use libloading::Library;
use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;

type CuResult = i32;
type CuDevice = i32;
type CuContext = *mut c_void;
type CuStream = *mut c_void;
type DevPtr = u64;

pub const ALIGN: usize = 16; // nvCOMP needs 4-byte aligned buffers; 16 keeps copies friendly
pub const MAX_BATCH_ENTRIES: usize = 65536;

pub fn align(n: usize) -> usize {
    (n + ALIGN - 1) & !(ALIGN - 1)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DecompressOpts {
    backend: i32, // nvcompDecompressBackend_t: 0 lets nvCOMP choose
    sort_before_hw_decompress: i32,
    reserved: [u8; 56],
}

#[repr(C)]
struct NvcompProperties {
    version: u32,
    cudart_version: u32,
}

const OPTS: DecompressOpts = DecompressOpts { backend: 0, sort_before_hw_decompress: 0, reserved: [0; 56] };

/// The driver and nvCOMP entry points this program uses.
pub struct Api {
    _cuda: Library,
    _nvcomp: Library,
    pub nvcomp_path: String,
    ctx: CuContext,
    pub device: String,
    pub nvcomp_version: String,
    cu_ctx_set_current: unsafe extern "C" fn(CuContext) -> CuResult,
    cu_mem_alloc: unsafe extern "C" fn(*mut DevPtr, usize) -> CuResult,
    cu_mem_free: unsafe extern "C" fn(DevPtr) -> CuResult,
    cu_mem_alloc_host: unsafe extern "C" fn(*mut *mut c_void, usize) -> CuResult,
    cu_mem_free_host: unsafe extern "C" fn(*mut c_void) -> CuResult,
    cu_htod_async: unsafe extern "C" fn(DevPtr, *const c_void, usize, CuStream) -> CuResult,
    cu_dtoh_async: unsafe extern "C" fn(*mut c_void, DevPtr, usize, CuStream) -> CuResult,
    cu_stream_create: unsafe extern "C" fn(*mut CuStream, u32) -> CuResult,
    cu_stream_sync: unsafe extern "C" fn(CuStream) -> CuResult,
    cu_stream_destroy: unsafe extern "C" fn(CuStream) -> CuResult,
    cu_error_string: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
    nv_temp_size: unsafe extern "C" fn(usize, usize, DecompressOpts, *mut usize, usize, CuStream) -> i32,
    nv_decompress: unsafe extern "C" fn(
        *const DevPtr, *const usize, *const usize, *mut usize, usize, DevPtr, usize, *const DevPtr,
        DecompressOpts, *mut i32, CuStream,
    ) -> i32,
    nv_status_string: unsafe extern "C" fn(i32) -> *const c_char,
}

// The context and function pointers are process-wide; the driver is thread-safe.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// Places nvcomp64_6.dll is looked for, in order.
pub fn nvcomp_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = std::env::var_os("GPUZIP_NVCOMP_DIR") {
        out.push(PathBuf::from(dir).join("nvcomp64_6.dll"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("nvcomp64_6.dll"));
        }
    }
    out.push(PathBuf::from("nvcomp64_6.dll")); // the system search path (PATH)
    out
}

unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
    unsafe {
        lib.get::<T>(name)
            .map(|s| *s)
            .map_err(|e| format!("{} missing: {e}", String::from_utf8_lossy(&name[..name.len() - 1])))
    }
}

impl Api {
    /// Load the driver and nvCOMP, and make device 0's primary context ready.
    pub fn load() -> Result<Api, String> {
        unsafe {
            let cuda = Library::new("nvcuda.dll").map_err(|_| "NVIDIA driver not found (nvcuda.dll)".to_string())?;
            let (nvcomp, nvcomp_path) = nvcomp_candidates()
                .into_iter()
                .find_map(|p| Library::new(&p).ok().map(|l| (l, p.display().to_string())))
                .ok_or("nvCOMP not found (nvcomp64_6.dll): put it next to gpuzip.exe or set GPUZIP_NVCOMP_DIR")?;

            let cu_init: unsafe extern "C" fn(u32) -> CuResult = sym(&cuda, b"cuInit\0")?;
            let cu_device_get: unsafe extern "C" fn(*mut CuDevice, i32) -> CuResult = sym(&cuda, b"cuDeviceGet\0")?;
            let cu_device_name: unsafe extern "C" fn(*mut c_char, i32, CuDevice) -> CuResult =
                sym(&cuda, b"cuDeviceGetName\0")?;
            let cu_retain: unsafe extern "C" fn(*mut CuContext, CuDevice) -> CuResult =
                sym(&cuda, b"cuDevicePrimaryCtxRetain\0")?;
            let nv_props: unsafe extern "C" fn(*mut NvcompProperties) -> i32 = sym(&nvcomp, b"nvcompGetProperties\0")?;

            let mut api = Api {
                cu_ctx_set_current: sym(&cuda, b"cuCtxSetCurrent\0")?,
                cu_mem_alloc: sym(&cuda, b"cuMemAlloc_v2\0")?,
                cu_mem_free: sym(&cuda, b"cuMemFree_v2\0")?,
                cu_mem_alloc_host: sym(&cuda, b"cuMemAllocHost_v2\0")?,
                cu_mem_free_host: sym(&cuda, b"cuMemFreeHost\0")?,
                cu_htod_async: sym(&cuda, b"cuMemcpyHtoDAsync_v2\0")?,
                cu_dtoh_async: sym(&cuda, b"cuMemcpyDtoHAsync_v2\0")?,
                cu_stream_create: sym(&cuda, b"cuStreamCreate\0")?,
                cu_stream_sync: sym(&cuda, b"cuStreamSynchronize\0")?,
                cu_stream_destroy: sym(&cuda, b"cuStreamDestroy_v2\0")?,
                cu_error_string: sym(&cuda, b"cuGetErrorString\0")?,
                nv_temp_size: sym(&nvcomp, b"nvcompBatchedDeflateDecompressGetTempSize\0")?,
                nv_decompress: sym(&nvcomp, b"nvcompBatchedDeflateDecompressAsync\0")?,
                nv_status_string: sym(&nvcomp, b"nvcompGetStatusString\0")?,
                _cuda: cuda,
                _nvcomp: nvcomp,
                nvcomp_path,
                ctx: std::ptr::null_mut(),
                device: String::new(),
                nvcomp_version: String::new(),
            };
            api.check(cu_init(0), "cuInit")?;
            let mut dev = 0;
            api.check(cu_device_get(&mut dev, 0), "cuDeviceGet")?;
            let mut name = [0 as c_char; 256];
            api.check(cu_device_name(name.as_mut_ptr(), 256, dev), "cuDeviceGetName")?;
            api.device = CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned();
            let mut ctx = std::ptr::null_mut();
            api.check(cu_retain(&mut ctx, dev), "cuDevicePrimaryCtxRetain")?;
            api.ctx = ctx;
            let mut props = NvcompProperties { version: 0, cudart_version: 0 };
            if nv_props(&mut props) == 0 {
                let v = props.version;
                api.nvcomp_version = format!("{}.{}.{}", v / 1000, (v % 1000) / 100, v % 100);
            }
            Ok(api)
        }
    }

    fn check(&self, r: CuResult, what: &str) -> Result<(), String> {
        if r == 0 {
            return Ok(());
        }
        let mut s: *const c_char = std::ptr::null();
        unsafe { (self.cu_error_string)(r, &mut s) };
        let msg = if s.is_null() { String::new() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() };
        Err(format!("{what} failed: CUDA error {r} {msg}"))
    }

    fn nv_check(&self, r: i32, what: &str) -> Result<(), String> {
        if r == 0 {
            return Ok(());
        }
        let s = unsafe { (self.nv_status_string)(r) };
        let msg = if s.is_null() { String::new() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() };
        Err(format!("{what} failed: nvCOMP status {r} {msg}"))
    }

    /// Make the GPU usable from the calling thread.
    pub fn bind_thread(&self) -> Result<(), String> {
        self.check(unsafe { (self.cu_ctx_set_current)(self.ctx) }, "cuCtxSetCurrent")
    }
}

/// One set of buffers: while the writers read one slot's results, the GPU fills the other.
pub struct Slot {
    stream: CuStream,
    pub h_in: *mut u8,
    pub h_out: *mut u8,
    h_meta: *mut u8,
    d_in: DevPtr,
    d_out: DevPtr,
    d_meta: DevPtr,
    d_temp: DevPtr,
    temp_bytes: usize,
}

unsafe impl Send for Slot {}

/// What one decoded batch looks like: where each entry sits in the slot's output, and
/// whether nvCOMP reported it as decoded to the expected size.
pub struct BatchResult {
    pub out_offsets: Vec<usize>,
    pub ok: Vec<bool>,
}

pub struct Decoder<'a> {
    api: &'a Api,
    pub slots: Vec<Slot>,
    slot_bytes: usize,
    max_entry: usize,
}

// metadata per entry: compressed pointer, compressed size, buffer size, output pointer, actual size
const META_U64S: usize = 5;

impl<'a> Decoder<'a> {
    /// `slots` buffer sets of `slot_bytes` each, on the calling thread's context. Each set is
    /// allocated on its first use: pinned memory is slow to get, and often no batch comes.
    pub fn new(api: &'a Api, slot_bytes: usize, max_entry: usize, slots: usize) -> Result<Decoder<'a>, String> {
        api.bind_thread()?;
        let empty = || Slot {
            stream: std::ptr::null_mut(),
            h_in: std::ptr::null_mut(),
            h_out: std::ptr::null_mut(),
            h_meta: std::ptr::null_mut(),
            d_in: 0,
            d_out: 0,
            d_meta: 0,
            d_temp: 0,
            temp_bytes: 0,
        };
        Ok(Decoder { api, slots: (0..slots).map(|_| empty()).collect(), slot_bytes, max_entry })
    }

    fn ensure(&mut self, si: usize) -> Result<(), String> {
        let (api, slot_bytes, max_entry) = (self.api, self.slot_bytes, self.max_entry);
        let s = &mut self.slots[si];
        if !s.stream.is_null() {
            return Ok(());
        }
        let meta_bytes = MAX_BATCH_ENTRIES * (META_U64S * 8 + 4);
        unsafe {
            api.check((api.cu_stream_create)(&mut s.stream, 1), "cuStreamCreate")?;
            for (p, n) in [(&mut s.h_in, slot_bytes), (&mut s.h_out, slot_bytes), (&mut s.h_meta, meta_bytes)] {
                let mut ptr: *mut c_void = std::ptr::null_mut();
                api.check((api.cu_mem_alloc_host)(&mut ptr, n), "cuMemAllocHost (pinned memory)")?;
                *p = ptr as *mut u8;
            }
            for (p, n) in [(&mut s.d_in, slot_bytes), (&mut s.d_out, slot_bytes), (&mut s.d_meta, meta_bytes)] {
                api.check((api.cu_mem_alloc)(p, n), "cuMemAlloc")?;
            }
            api.nv_check(
                (api.nv_temp_size)(MAX_BATCH_ENTRIES, max_entry.max(1), OPTS, &mut s.temp_bytes, slot_bytes, s.stream),
                "nvcompBatchedDeflateDecompressGetTempSize",
            )?;
            if s.temp_bytes > 0 {
                api.check((api.cu_mem_alloc)(&mut s.d_temp, s.temp_bytes), "cuMemAlloc (temp)")?;
            }
        }
        Ok(())
    }

    /// Decode `spans` (data offset in `archive`, compressed size, size) into slot `si`.
    /// Their aligned sizes must fit the slot. Blocks until the results are in host memory.
    pub fn decode(&mut self, si: usize, archive: &[u8], spans: &[(u64, u64, u64)]) -> Result<BatchResult, String> {
        self.ensure(si)?;
        let api = self.api;
        let s = &self.slots[si];
        let n = spans.len();
        assert!(n > 0 && n <= MAX_BATCH_ENTRIES);
        let (mut cpos, mut upos) = (0usize, 0usize);
        let mut out_offsets = Vec::with_capacity(n);
        unsafe {
            let meta = std::slice::from_raw_parts_mut(s.h_meta as *mut u64, n * META_U64S);
            let (cptrs, rest) = meta.split_at_mut(n);
            let (csizes, rest) = rest.split_at_mut(n);
            let (bsizes, rest) = rest.split_at_mut(n);
            let (optrs, _actual) = rest.split_at_mut(n);
            for (i, &(off, cs, us)) in spans.iter().enumerate() {
                let (off, cs, us) = (off as usize, cs as usize, us as usize);
                if cpos + cs > self.slot_bytes || upos + us > self.slot_bytes {
                    return Err("batch does not fit the GPU buffers".into());
                }
                std::ptr::copy_nonoverlapping(archive.as_ptr().add(off), s.h_in.add(cpos), cs);
                cptrs[i] = s.d_in + cpos as u64;
                csizes[i] = cs as u64;
                bsizes[i] = us as u64;
                optrs[i] = s.d_out + upos as u64;
                out_offsets.push(upos);
                cpos = align(cpos + cs);
                upos = align(upos + us);
            }
            let d_cptrs = s.d_meta;
            let d_csizes = d_cptrs + 8 * n as u64;
            let d_bsizes = d_csizes + 8 * n as u64;
            let d_optrs = d_bsizes + 8 * n as u64;
            let d_actual = d_optrs + 8 * n as u64;
            let d_status = d_actual + 8 * n as u64;
            api.check((api.cu_htod_async)(s.d_in, s.h_in as *const c_void, cpos, s.stream), "copy to GPU")?;
            api.check((api.cu_htod_async)(s.d_meta, s.h_meta as *const c_void, 4 * 8 * n, s.stream), "copy to GPU")?;
            api.nv_check(
                (api.nv_decompress)(
                    d_cptrs as *const DevPtr,
                    d_csizes as *const usize,
                    d_bsizes as *const usize,
                    d_actual as *mut usize,
                    n,
                    s.d_temp,
                    s.temp_bytes,
                    d_optrs as *const DevPtr,
                    OPTS,
                    d_status as *mut i32,
                    s.stream,
                ),
                "nvcompBatchedDeflateDecompressAsync",
            )?;
            api.check((api.cu_dtoh_async)(s.h_out as *mut c_void, s.d_out, upos, s.stream), "copy from GPU")?;
            let h_actual = s.h_meta.add(4 * 8 * n);
            api.check((api.cu_dtoh_async)(h_actual as *mut c_void, d_actual, 8 * n + 4 * n, s.stream), "copy from GPU")?;
            api.check((api.cu_stream_sync)(s.stream), "cuStreamSynchronize")?;
            let actual = std::slice::from_raw_parts(h_actual as *const u64, n);
            let status = std::slice::from_raw_parts(h_actual.add(8 * n) as *const i32, n);
            let ok = (0..n).map(|i| status[i] == 0 && actual[i] == spans[i].2).collect();
            Ok(BatchResult { out_offsets, ok })
        }
    }
}

impl Drop for Decoder<'_> {
    fn drop(&mut self) {
        let api = self.api;
        let _ = api.bind_thread();
        for s in &self.slots {
            unsafe {
                if !s.stream.is_null() {
                    (api.cu_stream_sync)(s.stream);
                }
                for p in [s.h_in, s.h_out, s.h_meta] {
                    if !p.is_null() {
                        (api.cu_mem_free_host)(p as *mut c_void);
                    }
                }
                for p in [s.d_in, s.d_out, s.d_meta, s.d_temp] {
                    if p != 0 {
                        (api.cu_mem_free)(p);
                    }
                }
                if !s.stream.is_null() {
                    (api.cu_stream_destroy)(s.stream);
                }
            }
        }
    }
}
