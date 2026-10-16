#!/usr/bin/env python3
"""gpuzip: extract ZIP archives on an NVIDIA GPU with nvCOMP, with CPU fallback.

Each entry in a .zip file is compressed on its own, so the GPU can decode thousands of
Deflate entries at once. A single large entry, though, is decoded by one GPU thread and
runs far slower there than on a CPU core. So the work is split: a GPU helper process takes
batches of the smallest Deflate entries while CPU threads take the largest ones, until the
two meet. Stored, bzip2, LZMA and Zstandard entries are read on the CPU; encrypted,
Deflate64 and PPMd entries, and every non-ZIP archive (.rar, .7z, .tar.gz...), go to 7-Zip.

Usage:
  gpuzip x ARCHIVE... [-o DIR]   extract
  gpuzip t ARCHIVE...            test: decompress and check every CRC, write nothing
  gpuzip l ARCHIVE...            list entries and where each would be decoded
  gpuzip doctor                  check the GPU, nvCOMP, CuPy and 7-Zip setup
"""

import argparse
import collections
import glob
import importlib.util
import json
import mmap
import multiprocessing as mp
import os
import platform
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zipfile
import zlib
from concurrent.futures import ThreadPoolExecutor, wait
from multiprocessing import shared_memory

__version__ = "0.1.0"
MiB = 1 << 20
ALIGN = 16  # nvCOMP wants 4-byte aligned buffers; 16 keeps every copy friendly
MAX_BATCH_ENTRIES = 65536
WRITE_CHUNK = 64  # GPU results are handed to the I/O threads this many files at a time
CPU_METHODS = {zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED, zipfile.ZIP_BZIP2, zipfile.ZIP_LZMA}
if hasattr(zipfile, "ZIP_ZSTANDARD"):  # Python 3.14+
    CPU_METHODS.add(zipfile.ZIP_ZSTANDARD)
METHOD_NAMES = {0: "stored", 8: "deflate", 9: "deflate64", 12: "bzip2", 14: "lzma",
                93: "zstd", 95: "xz", 98: "ppmd", 99: "aes"}
TAR_SUFFIXES = (".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz",
                ".tar.zst", ".tzst", ".tar.lz", ".tar.lzma")
_WIN_BAD = str.maketrans({c: "_" for c in ':<>|"?*'})


def align(n):
    return (n + ALIGN - 1) & ~(ALIGN - 1)


def human(n):
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1000 or unit == "GB":
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1000


def gpu_libs_missing():
    """The pip package to install for the GPU path, or None when everything is there."""
    for module, package in (("numpy", "numpy"), ("cupy", "cupy-cuda13x"), ("nvidia.nvcomp", "nvidia-nvcomp-cu13")):
        try:
            if importlib.util.find_spec(module) is None:
                return package
        except ModuleNotFoundError:
            return package
    return None


def find_7z(explicit=None):
    candidates = [explicit, shutil.which("7z"), shutil.which("7za"),
                  r"C:\Program Files\7-Zip\7z.exe", r"C:\Program Files (x86)\7-Zip\7z.exe"]
    return next((c for c in candidates if c and os.path.isfile(c)), None)


def safe_relpath(name):
    """The entry's path below the output folder, or None if it would escape it."""
    parts = []
    for p in name.replace("\\", "/").split("/"):
        if p in ("", "."):
            continue
        if p == "..":
            return None
        if os.name == "nt":
            p = p.translate(_WIN_BAD).rstrip(". ") or "_"
        parts.append(p)
    return os.path.join(*parts) if parts else None


def set_mtime(path, date_time):
    try:
        t = time.mktime(date_time + (0, 0, -1))
        os.utime(path, (t, t))
    except (OverflowError, ValueError, OSError):
        pass


class Entry:
    __slots__ = ("info", "offset", "route", "path")

    def __init__(self, info):
        self.info, self.offset, self.route, self.path = info, 0, "", None

    size = property(lambda self: self.info.file_size)
    csize = property(lambda self: self.info.compress_size)
    method = property(lambda self: self.info.compress_type)


def plan(zf, mm):
    """Give every entry a route: deflate (GPU or CPU, decided while running), cpu, 7z,
    dir, unsafe (path escapes the output folder) or dup (a later entry has the same path)."""
    entries, by_path = [], {}
    for info in zf.infolist():
        e = Entry(info)
        e.path = safe_relpath(info.filename)
        if e.path is None:
            e.route = "unsafe"
        elif info.is_dir():
            e.route = "dir"
        elif info.flag_bits & 0x1 or info.compress_type not in CPU_METHODS:
            e.route = "7z"
        else:
            e.route = "deflate" if info.compress_type == zipfile.ZIP_DEFLATED and info.file_size else "cpu"
            h = info.header_offset
            if mm[h:h + 4] != b"PK\x03\x04":
                raise zipfile.BadZipFile(f"bad local header for {info.filename!r}")
            name_len, extra_len = struct.unpack_from("<HH", mm, h + 26)
            e.offset = h + 30 + name_len + extra_len
        if e.route in ("deflate", "cpu", "7z"):
            key = e.path.lower() if os.name == "nt" else e.path
            if key in by_path:
                by_path[key].route = "dup"
            by_path[key] = e
        entries.append(e)
    return entries


def batch_layout(spans):
    """Where each entry of a GPU batch sits, compressed and decoded. Both processes compute
    this, so replies through the pipe stay tiny: two processes writing large messages into
    one pipe at the same moment deadlock."""
    coffs, uoffs, cpos, upos = [], [], 0, 0
    for _, cs, us in spans:
        coffs.append(cpos)
        uoffs.append(upos)
        cpos += align(cs)
        upos += align(us)
    return coffs, uoffs, cpos, upos


def gpu_helper(conn, archive, slot_names, slot_bytes):
    """Decodes batches of raw Deflate streams on the GPU, in its own process: the nvCOMP
    Python binding holds the GIL while it works, which would stall the CPU threads."""
    t0 = time.perf_counter()
    try:
        import numpy as np
        import cupy as cp
        import cupyx
        from nvidia import nvcomp

        rt = cp.cuda.runtime
        codec = nvcomp.Codec(algorithm="Deflate", bitstream_kind=nvcomp.BitstreamKind.RAW)
        shms = [shared_memory.SharedMemory(name=n) for n in slot_names]
        slots = [np.ndarray((slot_bytes,), np.uint8, buffer=s.buf) for s in shms]
        for a in slots:  # pinned memory: copies from the GPU run ~5x faster
            rt.hostRegister(a.ctypes.data, slot_bytes, 0)
        staging = cupyx.empty_pinned((slot_bytes,), np.uint8)
        dev_in, dev_out = cp.empty(slot_bytes, cp.uint8), cp.empty(slot_bytes, cp.uint8)
        nv_in, nv_out = nvcomp.as_array(dev_in), nvcomp.as_array(dev_out)
        f = open(archive, "rb")
        mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        src = np.frombuffer(mm, np.uint8)
        name = cp.cuda.runtime.getDeviceProperties(0)["name"].decode()
    except Exception as ex:
        conn.send(("fail", f"{type(ex).__name__}: {ex}"))
        return
    conn.send(("ready", time.perf_counter() - t0, name))
    while True:
        job = conn.recv()
        if job is None:
            break
        slot, spans = job
        t = time.perf_counter()
        try:
            coffs, uoffs, cpos, upos = batch_layout(spans)
            for (off, cs, _), co in zip(spans, coffs):
                staging[co:co + cs] = src[off:off + cs]
            rt.memcpy(dev_in.data.ptr, staging.ctypes.data, cpos, rt.memcpyHostToDevice)
            res = codec.decode(nvcomp.as_arrays(nv_in, coffs, [s[1] for s in spans]),
                               out=nvcomp.as_arrays(nv_out, uoffs, [s[2] for s in spans]))
            res[0]  # waits for the decode to finish
            rt.memcpy(slots[slot].ctypes.data, dev_out.data.ptr, upos, rt.memcpyDeviceToHost)
            conn.send(("done", slot, time.perf_counter() - t))
        except Exception as ex:
            conn.send(("error", slot, f"{type(ex).__name__}: {ex}"[:500]))
    for a in slots:
        rt.hostUnregister(a.ctypes.data)


class Run:
    def __init__(self, args, archive, zf, mm, entries, out_root):
        self.args, self.archive, self.zf, self.mm, self.mv = args, archive, zf, mm, memoryview(mm)
        self.entries, self.out_root = entries, out_root
        self.lock = threading.Lock()
        self.stats = {r: {"files": 0, "bytes": 0} for r in ("gpu", "cpu", "7z")}
        self.failed, self.retry, self.notes, self.info = [], [], [], []  # info: shown with --verbose
        self.gpu = {"device": None, "batches": 0, "busy_seconds": 0.0, "startup_seconds": None}

        deflate = sorted((e for e in entries if e.route == "deflate"), key=lambda e: e.size)
        self.cpu_queue = collections.deque(e for e in entries if e.route == "cpu")
        self.pending = collections.deque(deflate)  # GPU eats from the left, CPU from the right
        self.gpu_max_entry = args.gpu_max_entry_kb * 1024 if args.mode == "auto" else float("inf")
        eligible = sum(align(e.size) for e in deflate if e.size <= self.gpu_max_entry)
        self.slot_bytes = max(MiB, min(args.batch_mb * MiB, align(eligible)))
        self.use_gpu = args.mode == "gpu" or (args.mode == "auto" and eligible >= args.gpu_min_mb * MiB)
        if args.mode == "auto" and not self.use_gpu and deflate:
            self.info.append(f"GPU skipped: only {human(eligible)} of small Deflate data, the CPU is faster below "
                             f"{args.gpu_min_mb} MB")
        if self.use_gpu and (missing := gpu_libs_missing()):
            self.use_gpu = False
            (self.notes if args.mode == "gpu" else self.info).append(
                f"GPU libraries not installed ({missing}); used the CPU. Install them with: pip install \"gpuzip[gpu]\"")
        self.steal = args.mode != "gpu"  # in gpu mode the CPU only takes what the GPU cannot

    # --- output -----------------------------------------------------------------------
    def deliver(self, e, data, route):
        """Check size and CRC, then write the file. False means the data is wrong."""
        if len(data) != e.size or zlib.crc32(data) != e.info.CRC:
            return False
        if self.out_root is not None:
            path = os.path.join(self.out_root, e.path)
            with open(path, "wb") as f:
                f.write(data)
            set_mtime(path, e.info.date_time)
        with self.lock:
            s = self.stats[route]
            s["files"] += 1
            s["bytes"] += e.size
        return True

    def fail(self, e, reason):
        with self.lock:
            self.failed.append({"name": e.info.filename, "reason": reason})

    # --- CPU path ---------------------------------------------------------------------
    def decode_cpu(self, e):
        if e.method == zipfile.ZIP_DEFLATED:
            return zlib.decompress(self.mv[e.offset:e.offset + e.csize], -15, max(e.size, 1))
        if e.method == zipfile.ZIP_STORED:
            return self.mv[e.offset:e.offset + e.size]
        return self.zf.read(e.info)

    def run_cpu(self, e):
        try:
            if not self.deliver(e, self.decode_cpu(e), "cpu"):
                self.fail(e, "CRC mismatch")
        except Exception as ex:
            self.fail(e, f"{type(ex).__name__}: {ex}")

    def take_cpu(self):
        with self.lock:
            if self.cpu_queue:
                return self.cpu_queue.popleft()
            if self.steal and self.pending:
                return self.pending.pop()
        return None

    def cpu_worker(self):
        while (e := self.take_cpu()) is not None:
            self.run_cpu(e)

    # --- GPU path ---------------------------------------------------------------------
    def take_gpu_batch(self):
        with self.lock:
            batch, used_u, used_c, p = [], 0, 0, self.pending
            while p and len(batch) < MAX_BATCH_ENTRIES:
                e = p[0]
                u, c = align(e.size), align(e.csize)
                if e.size > self.gpu_max_entry or used_u + u > self.slot_bytes or used_c + c > self.slot_bytes:
                    break
                batch.append(p.popleft())
                used_u += u
                used_c += c
            return batch

    def write_gpu_batch(self, shm, batch, uoffs, free):
        chunks = [range(i, min(i + WRITE_CHUNK, len(batch))) for i in range(0, len(batch), WRITE_CHUNK)]
        left = [len(chunks)]

        def work(idx):
            try:
                for i in idx:
                    e = batch[i]
                    data = shm.buf[uoffs[i]:uoffs[i] + e.size]
                    try:
                        if not self.deliver(e, data, "gpu"):
                            with self.lock:
                                self.retry.append(e)
                    except Exception as ex:
                        self.fail(e, f"{type(ex).__name__}: {ex}")
                    finally:
                        data.release()
            finally:
                with self.lock:
                    left[0] -= 1
                    last = left[0] == 0
                if last:
                    free.set()

        for c in chunks:
            self.io_pool.submit(work, c)

    def gpu_feeder(self):
        ctx = mp.get_context("spawn")
        shms = [shared_memory.SharedMemory(create=True, size=self.slot_bytes) for _ in range(2)]
        free = [threading.Event(), threading.Event()]
        for ev in free:
            ev.set()
        parent, child = ctx.Pipe()
        t0 = time.perf_counter()
        proc = ctx.Process(target=gpu_helper, daemon=True,
                           args=(child, self.archive, [s.name for s in shms], self.slot_bytes))
        proc.start()
        outstanding = collections.deque()
        try:
            while not parent.poll(0.05):
                if not proc.is_alive():
                    raise EOFError("GPU helper died during startup")
                if self.steal and not self.pending:
                    self.info.append("GPU not used: the CPU took every entry before the GPU was ready")
                    proc.terminate()
                    return
            msg = parent.recv()
            if msg[0] != "ready":
                self.notes.append(f"GPU unavailable ({msg[1]}); used the CPU instead")
                return
            self.gpu["startup_seconds"] = round(time.perf_counter() - t0, 3)
            self.gpu["device"] = msg[2]
            k = 0
            while True:
                batch = self.take_gpu_batch()
                if batch:
                    slot = k % 2
                    k += 1
                    free[slot].wait()
                    free[slot].clear()
                    outstanding.append((slot, batch))
                    parent.send((slot, [(e.offset, e.csize, e.size) for e in batch]))
                if outstanding and (not batch or len(outstanding) == 2):
                    while not parent.poll(1.0):
                        if not proc.is_alive():
                            raise EOFError("GPU helper died")
                    msg = parent.recv()
                    slot, done = outstanding.popleft()
                    if msg[0] == "done":
                        self.gpu["batches"] += 1
                        self.gpu["busy_seconds"] += msg[2]
                        uoffs = batch_layout([(0, e.csize, e.size) for e in done])[1]
                        self.write_gpu_batch(shms[slot], done, uoffs, free[slot])
                    else:
                        self.notes.append(f"GPU batch failed ({msg[2]}); redone on the CPU")
                        with self.lock:
                            self.retry.extend(done)
                        free[slot].set()
                if not batch and not outstanding:
                    break
        except (EOFError, OSError) as ex:
            self.notes.append(f"GPU helper stopped ({type(ex).__name__}); the CPU finished the job")
            for slot, undone in outstanding:  # sent but never decoded: back to the CPU
                with self.lock:
                    self.pending.extend(undone)
                free[slot].set()
        finally:
            try:
                parent.send(None)
            except OSError:
                pass
            proc.join(timeout=10)
            for ev in free:
                ev.wait()
            for s in shms:
                s.close()
                s.unlink()

    # --- 7-Zip path -------------------------------------------------------------------
    def run_7z_entries(self):
        todo = [e for e in self.entries if e.route == "7z"]
        if not todo:
            return
        exe = find_7z(self.args.sevenzip)
        if not exe:
            for e in todo:
                self.fail(e, f"{METHOD_NAMES.get(e.method, e.method)} or encrypted entry needs 7-Zip, not found")
            return
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".txt", delete=False) as f:
            f.write("\n".join(e.info.filename for e in todo))
        cmd = [exe, "x" if self.out_root is not None else "t", self.archive, "-y", "-scsUTF-8", f"@{f.name}"]
        if self.out_root is not None:
            cmd.append(f"-o{self.out_root}")
        if self.args.password:
            cmd.append(f"-p{self.args.password}")
        rc = subprocess.run(cmd, stdout=subprocess.DEVNULL if self.args.quiet or self.args.json else None).returncode
        os.unlink(f.name)
        for e in todo:
            if rc == 0:
                self.stats["7z"]["files"] += 1
                self.stats["7z"]["bytes"] += e.size
            else:
                self.fail(e, f"7-Zip exit code {rc}")

    # --- driver -----------------------------------------------------------------------
    def progress(self, stop):
        total = sum(e.size for e in self.entries if e.route in ("deflate", "cpu")) or 1
        t0 = time.perf_counter()
        while not stop.wait(0.25):
            g, c = self.stats["gpu"], self.stats["cpu"]
            done = g["bytes"] + c["bytes"]
            rate = done / (time.perf_counter() - t0) / 1e9
            sys.stderr.write(f"\r  {100 * done / total:5.1f}% · {g['files'] + c['files']:,} files · {human(done)}"
                             f" · {rate:.2f} GB/s · GPU {g['files']:,} · CPU {c['files']:,}   ")
        sys.stderr.write("\r" + " " * 96 + "\r")

    def run(self):
        t0 = time.perf_counter()
        if self.out_root is not None:
            dirs = {self.out_root}
            for e in self.entries:
                if e.route == "dir":
                    dirs.add(os.path.join(self.out_root, e.path))
                elif e.route in ("deflate", "cpu", "7z"):
                    dirs.add(os.path.dirname(os.path.join(self.out_root, e.path)))
            for d in sorted(dirs):
                os.makedirs(d, exist_ok=True)
        stop = threading.Event()
        if not (self.args.quiet or self.args.json) and sys.stderr.isatty():
            threading.Thread(target=self.progress, args=(stop,), daemon=True).start()
        self.io_pool = ThreadPoolExecutor(self.args.io_threads)
        feeder = threading.Thread(target=self.gpu_feeder) if self.use_gpu else None
        if feeder:
            feeder.start()
        with ThreadPoolExecutor(self.args.threads) as cpu_pool:
            wait([cpu_pool.submit(self.cpu_worker) for _ in range(self.args.threads)])
            if feeder:
                feeder.join()
            self.io_pool.shutdown(wait=True)
            # what the GPU could not take or got wrong (should be nothing) is redone on the CPU
            if self.retry:
                self.notes.append(f"{len(self.retry)} GPU results failed the CRC check and were redone on the CPU")
            rest = list(self.pending) + self.retry
            self.pending.clear()
            wait([cpu_pool.submit(self.run_cpu, e) for e in rest])
        self.run_7z_entries()
        stop.set()
        for e in self.entries:
            if e.route == "unsafe":
                self.fail(e, "path leaves the output folder; skipped")
            elif e.route == "dup":
                self.notes.append(f"duplicate path, kept the last copy: {e.info.filename}")
        self.seconds = time.perf_counter() - t0


def report(args, archive, fmt, seconds, stats, failed, notes, info=(), gpu=None, retried=0):
    files = sum(s["files"] for s in stats.values())
    size = sum(s["bytes"] for s in stats.values())
    out = {"archive": archive, "command": args.cmd, "mode": args.mode, "format": fmt,
           "files": files, "bytes": size, "seconds": round(seconds, 4),
           "gbps": round(size / seconds / 1e9, 3) if seconds else None,
           "routes": stats, "gpu": gpu, "retried": retried, "failed": failed, "notes": notes, "info": list(info)}
    if args.json:
        print(json.dumps(out))
        return
    if args.quiet and not failed:
        return
    verb = "Extracted" if args.cmd == "x" else "Tested"
    if fmt != "zip":
        print(f"{verb} {os.path.basename(archive)} with 7-Zip on the CPU in {seconds:.2f} s · {len(failed)} failed")
    else:
        print(f"{verb} {files:,} files ({human(size)}) in {seconds:.2f} s"
              f" · {size / seconds / 1e9 if seconds else 0:.2f} GB/s"
              f" · {stats['gpu']['files']:,} on GPU, {stats['cpu']['files']:,} on CPU, {stats['7z']['files']:,} with 7-Zip"
              f" · {len(failed)} failed")
    if args.verbose and gpu and gpu.get("device"):
        print(f"  GPU: {gpu['device']} · {gpu['batches']} batches · busy {gpu['busy_seconds']:.2f} s"
              f" · ready after {gpu['startup_seconds']:.2f} s")
    for n in list(notes) + (list(info) if args.verbose else []):
        print(f"  Note: {n}")
    for f in failed[:20]:
        print(f"  Failed: {f['name']}: {f['reason']}")
    if len(failed) > 20:
        print(f"  ... and {len(failed) - 20} more failed")


def archive_stem(archive):
    base = os.path.basename(archive)
    for suf in TAR_SUFFIXES + (".zip", ".rar", ".7z", ".tar", ".gz", ".xz", ".bz2", ".zst"):
        if base.lower().endswith(suf):
            return base[:-len(suf)] or "out"
    return os.path.splitext(base)[0] + "_out"


def run_7z_archive(args, archive, out_root):
    """Non-ZIP archives (RAR, 7z, tar.*...) are extracted by 7-Zip on the CPU."""
    exe = find_7z(args.sevenzip)
    if not exe:
        print(f"{archive}: this format needs 7-Zip (https://www.7-zip.org); "
              "pass --sevenzip PATH if it is installed somewhere else", file=sys.stderr)
        return 1
    quiet = subprocess.DEVNULL if args.quiet or args.json or not args.verbose else None
    pw = [f"-p{args.password}"] if args.password else []
    out = [f"-o{out_root}"] if out_root else []
    verb = "x" if out_root else "t"
    t0 = time.perf_counter()
    if archive.lower().endswith(TAR_SUFFIXES):
        first = subprocess.Popen([exe, "x", archive, "-so", "-y", *pw], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        second = subprocess.run([exe, verb, "-si", "-ttar", "-y", *out], stdin=first.stdout, stdout=quiet)
        first.stdout.close()
        rc = first.wait() or second.returncode
    else:
        rc = subprocess.run([exe, verb, archive, "-y", *pw, *out], stdout=quiet).returncode
    seconds = time.perf_counter() - t0
    stats = {r: {"files": 0, "bytes": 0} for r in ("gpu", "cpu", "7z")}
    failed = [{"name": archive, "reason": f"7-Zip exit code {rc}"}] if rc else []
    ext = os.path.basename(archive).lower().split(".", 1)[-1]
    report(args, archive, ext, seconds, stats, failed, [], ["not a ZIP file: handed to 7-Zip on the CPU"])
    return 2 if rc else 0


def list_entries(args, zf, entries):
    gpu_max = args.gpu_max_entry_kb * 1024 if args.mode == "auto" else float("inf")
    counts = collections.Counter()
    print(f"{'method':>9} {'size':>12} {'packed':>12}  route  name")
    for e in entries:
        route = e.route
        if route == "deflate":
            route = "cpu" if args.mode == "cpu" or e.size > gpu_max else "gpu"
        counts[route] += 1
        print(f"{METHOD_NAMES.get(e.method, e.method):>9} {e.size:>12,} {e.csize:>12,}  {route:<5}  {e.info.filename}")
    print("  " + " · ".join(f"{n:,} {r}" for r, n in counts.most_common()))


def process_archive(args, path, out_root):
    archive = os.path.abspath(path)
    if not os.path.isfile(archive):
        print(f"No such file: {path}", file=sys.stderr)
        return 1
    if not zipfile.is_zipfile(archive):
        if args.cmd == "l":
            exe = find_7z(args.sevenzip)
            if not exe:
                print(f"{path}: listing this format needs 7-Zip", file=sys.stderr)
                return 1
            return subprocess.run([exe, "l", archive]).returncode
        return run_7z_archive(args, archive, out_root)

    with open(archive, "rb") as fh, zipfile.ZipFile(archive) as zf:
        mm = mmap.mmap(fh.fileno(), 0, access=mmap.ACCESS_READ)
        entries = plan(zf, mm)
        if args.cmd == "l":
            list_entries(args, zf, entries)
            return 0
        run = Run(args, archive, zf, mm, entries, out_root)
        run.run()
        run.mv.release()
        report(args, archive, "zip", run.seconds, run.stats, run.failed, run.notes, run.info,
               run.gpu if run.use_gpu else None, len(run.retry))
        try:
            mm.close()
        except BufferError:
            pass
    return 2 if run.failed else 0


def doctor(args):
    """Report what gpuzip can use on this machine, with a real decode on the GPU."""
    zlib_name = f"zlib-ng {zlib.ZLIBNG_VERSION}" if hasattr(zlib, "ZLIBNG_VERSION") else f"zlib {zlib.ZLIB_VERSION}"
    print(f"gpuzip {__version__} · Python {platform.python_version()} · {os.cpu_count()} CPU threads · {zlib_name}")
    exe = find_7z(args.sevenzip)
    if exe:
        version = subprocess.run([exe], capture_output=True, text=True).stdout.strip().splitlines()[0].split(":")[0]
        print(f"7-Zip:  {version.strip()} at {exe}")
    else:
        print("7-Zip:  not found; needed for RAR, 7z, tar.* and encrypted entries (https://www.7-zip.org)")
    missing = gpu_libs_missing()
    if missing:
        print(f"GPU:    libraries not installed ({missing}); ZIP files are extracted on the CPU.\n"
              f"        To use an NVIDIA GPU: pip install \"gpuzip[gpu]\"")
        return 0
    try:
        from importlib.metadata import version
        import numpy as np
        import cupy as cp
        from nvidia import nvcomp

        name = cp.cuda.runtime.getDeviceProperties(0)["name"].decode()
        data = b"gpuzip doctor checks a decode on the GPU. " * 40_000
        c = zlib.compressobj(6, zlib.DEFLATED, -15)
        comp = c.compress(data) + c.flush()
        codec = nvcomp.Codec(algorithm="Deflate", bitstream_kind=nvcomp.BitstreamKind.RAW)
        out = codec.decode(nvcomp.as_array(np.frombuffer(comp, np.uint8)).cuda())
        good = bytes(np.asarray(out.cpu())) == data
        print(f"GPU:    {name} · nvCOMP {version('nvidia-nvcomp-cu13')} · CuPy {cp.__version__}"
              f" · test decode {'ok' if good else 'WRONG OUTPUT'}")
        return 0 if good else 1
    except Exception as ex:
        print(f"GPU:    not usable ({type(ex).__name__}: {ex}); ZIP files are extracted on the CPU")
        return 1


EPILOG = r"""examples:
  gpuzip x photos.zip               extract into .\photos
  gpuzip x *.zip -o D:\out          extract each archive into D:\out\<name>
  gpuzip x backup.rar               RAR, 7z and tar.* go to 7-Zip
  gpuzip t backup.zip               check every file's CRC, write nothing
  gpuzip l backup.zip               list entries and where each is decoded
  gpuzip doctor                     check the GPU, nvCOMP and 7-Zip

exit codes: 0 ok · 1 missing file or setup problem · 2 some entries failed · 130 cancelled"""


def build_parser():
    common = argparse.ArgumentParser(add_help=False)
    o = common.add_argument_group("options")
    o.add_argument("--mode", choices=["auto", "gpu", "cpu"], default="auto",
                   help="auto: GPU and CPU share the work (default) · gpu: all Deflate on the GPU · cpu: no GPU")
    o.add_argument("--threads", type=int, default=os.cpu_count() or 4, help="CPU decode threads (default: all)")
    o.add_argument("--io-threads", type=int, default=min(32, os.cpu_count() or 4), help="threads writing GPU results")
    o.add_argument("--batch-mb", type=int, default=256, help="max uncompressed MB per GPU batch (default 256)")
    o.add_argument("--gpu-max-entry-kb", type=int, default=1024, help="auto: larger entries stay on the CPU (1024)")
    o.add_argument("--gpu-min-mb", type=int, default=64, help="auto: skip the GPU below this much small-entry data (64)")
    o.add_argument("--password", help="password for encrypted entries (passed to 7-Zip)")
    o.add_argument("--sevenzip", metavar="PATH", help="path to 7z.exe, if not in the usual place")
    o.add_argument("--json", action="store_true", help="print one JSON report per archive")
    o.add_argument("-v", "--verbose", action="store_true", help="also show GPU details and why work went where")
    o.add_argument("-q", "--quiet", action="store_true", help="print only failures")

    ap = argparse.ArgumentParser(prog="gpuzip", epilog=EPILOG, formatter_class=argparse.RawDescriptionHelpFormatter,
                                 description="Extract ZIP archives on an NVIDIA GPU with nvCOMP, with CPU fallback. "
                                             "RAR, 7z and tar.* archives go to 7-Zip.")
    ap.add_argument("--version", action="version", version=f"gpuzip {__version__}")
    sub = ap.add_subparsers(metavar="COMMAND", required=True)
    for name, alias, text in (("x", "extract", "extract archives"),
                              ("t", "test", "decompress and check every CRC, write nothing"),
                              ("l", "list", "list entries and where each would be decoded")):
        p = sub.add_parser(name, aliases=[alias], parents=[common], help=text,
                           description=text[0].upper() + text[1:] + ".")
        p.add_argument("archives", nargs="+", metavar="ARCHIVE", help="archive files; wildcards like *.zip work")
        if name == "x":
            p.add_argument("-o", "--output", metavar="DIR",
                           help="output folder (default: the archive's name, in the current folder); "
                                "with several archives, each goes into DIR\\<name>")
        p.set_defaults(cmd=name)
    d = sub.add_parser("doctor", parents=[common], help="check the GPU, nvCOMP, CuPy and 7-Zip setup",
                       description="Check the GPU, nvCOMP, CuPy and 7-Zip setup.")
    d.set_defaults(cmd="doctor")
    return ap


def expand(patterns):
    """Windows shells pass *.zip through unexpanded, so expand wildcards here."""
    out = []
    for p in patterns:
        matches = sorted(glob.glob(p)) if glob.has_magic(p) else []
        out.extend(matches or [p])
    return out


def main(argv=None):
    args = build_parser().parse_args(argv)
    if args.cmd == "doctor":
        return doctor(args)
    archives = expand(args.archives)
    rc = 0
    try:
        for path in archives:
            out_root = None
            if args.cmd == "x":
                if args.output and len(archives) > 1:
                    out_root = os.path.abspath(os.path.join(args.output, archive_stem(path)))
                else:
                    out_root = os.path.abspath(args.output or archive_stem(path))
            if len(archives) > 1 and not (args.quiet or args.json):
                print(f"{path}:")
            rc = max(rc, process_archive(args, path, out_root))
    except KeyboardInterrupt:
        print("\nCancelled", file=sys.stderr)
        return 130
    return rc


if __name__ == "__main__":
    sys.exit(main())
