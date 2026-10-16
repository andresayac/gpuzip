//! zseq: how good a zstd file can be made from the GPU match finder's candidates.
//!
//!   zseq <file> [--k 3] [--window-log 27] [--levels 19,22|none] [--parsers greedy,lazy,optimal]
//!
//! Reads <file> and <file>.cand3 (gpumatch --dump: K (offset, length) pairs per position; --k 1
//! reads <file>.cand with one), parses the data into LZ77 sequences in three ways (greedy; lazy
//! with repeat offsets, as zstd's lazy2 does; and an optimal parse priced in bits, as zstd's
//! btultra does), hands each sequence list to libzstd's ZSTD_compressSequences, which blocks
//! and entropy-codes it into a standard zstd frame, checks the frame decodes to the input, and
//! prints the sizes next to zstd's own levels on the same data with the same window.
//! The entropy coding is zstd's in every case, so the comparison isolates the parse.

use std::ffi::c_void;
use std::time::Instant;

// zstd.h parameter ids (stable numbers)
const C_COMPRESSION_LEVEL: u32 = 100;
const C_WINDOW_LOG: u32 = 101;
const C_MIN_MATCH: u32 = 105;
const C_ENABLE_LDM: u32 = 160;
const C_BLOCK_DELIMITERS: u32 = 1008; // ZSTD_c_experimentalParam11
const C_VALIDATE_SEQUENCES: u32 = 1009; // ZSTD_c_experimentalParam12
const SF_NO_BLOCK_DELIMITERS: i32 = 0;

const MAX_MATCH: usize = 65535; // longer matches are emitted in pieces (the format codes up to 131074)

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Seq {
    offset: u32,
    lit_length: u32,
    match_length: u32,
    rep: u32,
}

#[derive(Clone, Copy, Default)]
struct Cand {
    off: u32,
    len: u32,
}

/// K candidates per position, as gpumatch dumped them
struct Cands {
    k: usize,
    cap: u32,
    v: Vec<Cand>,
}

impl Cands {
    #[inline]
    fn at(&self, i: usize) -> &[Cand] {
        &self.v[i * self.k..(i + 1) * self.k]
    }

    /// candidate `s` at `i`, with a capped length extended (bounded by MAX_MATCH)
    #[inline]
    fn get(&self, d: &[u8], i: usize, s: usize) -> Cand {
        let c = self.v[i * self.k + s];
        if c.len == self.cap { Cand { off: c.off, len: extend(d, i - c.off as usize, i, (d.len() - i).min(MAX_MATCH)) } } else { c }
    }

    /// the longest candidate at `i`
    fn longest(&self, d: &[u8], i: usize) -> Cand {
        let mut best = Cand::default();
        for s in 0..self.k {
            let c = self.get(d, i, s);
            if c.len > best.len {
                best = c;
            }
        }
        best
    }
}

struct Cctx(*mut zstd_sys::ZSTD_CCtx);

impl Cctx {
    fn new(params: &[(u32, i32)]) -> Cctx {
        unsafe {
            let c = zstd_sys::ZSTD_createCCtx();
            for &(p, v) in params {
                // the binding declares the parameter ids as an enum with these same values
                let id: zstd_sys::ZSTD_cParameter = std::mem::transmute(p);
                let r = zstd_sys::ZSTD_CCtx_setParameter(c, id, v);
                assert_eq!(zstd_sys::ZSTD_isError(r), 0, "setParameter {p}={v}: {}", err(r));
            }
            Cctx(c)
        }
    }
}

impl Drop for Cctx {
    fn drop(&mut self) {
        unsafe { zstd_sys::ZSTD_freeCCtx(self.0) };
    }
}

fn err(r: usize) -> String {
    unsafe { std::ffi::CStr::from_ptr(zstd_sys::ZSTD_getErrorName(r)) }.to_string_lossy().into_owned()
}

fn bound(n: usize) -> usize {
    unsafe { zstd_sys::ZSTD_compressBound(n) }
}

/// zstd's own compression of `data` with the given level and window
fn zstd_level(data: &[u8], level: i32, window_log: i32, extra: &[(u32, i32)]) -> (usize, f64) {
    let mut params = vec![(C_COMPRESSION_LEVEL, level), (C_WINDOW_LOG, window_log)];
    params.extend_from_slice(extra);
    let c = Cctx::new(&params);
    let mut out = vec![0u8; bound(data.len())];
    let t = Instant::now();
    let r = unsafe { zstd_sys::ZSTD_compress2(c.0, out.as_mut_ptr() as *mut c_void, out.len(), data.as_ptr() as *const c_void, data.len()) };
    assert_eq!(unsafe { zstd_sys::ZSTD_isError(r) }, 0, "compress2: {}", err(r));
    (r, t.elapsed().as_secs_f64())
}

/// Entropy-code our sequences with libzstd, verify, and return the frame's size. The bytes
/// after the last sequence are literals (a literal-only sequence is not allowed here).
fn compress_sequences(data: &[u8], seqs: &[Seq], level: i32, window_log: i32) -> usize {
    let c = Cctx::new(&[
        (C_COMPRESSION_LEVEL, level),
        (C_WINDOW_LOG, window_log),
        (C_MIN_MATCH, 3),
        (C_BLOCK_DELIMITERS, SF_NO_BLOCK_DELIMITERS),
        (C_VALIDATE_SEQUENCES, 1),
    ]);
    let mut out = vec![0u8; bound(data.len())];
    let r = unsafe {
        zstd_sys::ZSTD_compressSequences(
            c.0,
            out.as_mut_ptr() as *mut c_void,
            out.len(),
            seqs.as_ptr() as *const zstd_sys::ZSTD_Sequence,
            seqs.len(),
            data.as_ptr() as *const c_void,
            data.len(),
        )
    };
    assert_eq!(unsafe { zstd_sys::ZSTD_isError(r) }, 0, "compressSequences: {}", err(r));
    out.truncate(r);
    let mut back = vec![0u8; data.len()];
    let d = unsafe { zstd_sys::ZSTD_decompress(back.as_mut_ptr() as *mut c_void, back.len(), out.as_ptr() as *const c_void, out.len()) };
    assert_eq!(unsafe { zstd_sys::ZSTD_isError(d) }, 0, "decompress: {}", err(d));
    assert!(d == data.len() && back == data, "the frame does not decode to the input");
    r
}

#[inline]
fn extend(d: &[u8], a: usize, b: usize, lim: usize) -> u32 {
    let mut n = 0;
    while n + 8 <= lim {
        let x = u64::from_le_bytes(d[a + n..a + n + 8].try_into().unwrap());
        let y = u64::from_le_bytes(d[b + n..b + n + 8].try_into().unwrap());
        if x != y {
            return (n + ((x ^ y).trailing_zeros() / 8) as usize) as u32;
        }
        n += 8;
    }
    while n < lim && d[a + n] == d[b + n] {
        n += 1;
    }
    n as u32
}

/// Take the longest match of 3+ as it comes
fn parse_greedy(d: &[u8], cands: &Cands) -> Vec<Seq> {
    let mut seqs = Vec::new();
    let (mut i, mut lit) = (0usize, 0u32);
    while i < d.len() {
        let c = cands.longest(d, i);
        if c.len >= 3 {
            seqs.push(Seq { offset: c.off, lit_length: lit, match_length: c.len, rep: 0 });
            lit = 0;
            i += c.len as usize;
        } else {
            lit += 1;
            i += 1;
        }
    }
    seqs
}

/// zstd's lazy idea: before taking a match, look whether the next position (or the one after)
/// has a longer one; prefer a repeat offset when it is nearly as long (it costs few bits).
fn parse_lazy(d: &[u8], cands: &Cands) -> Vec<Seq> {
    let mut seqs = Vec::new();
    let (mut i, mut lit) = (0usize, 0u32);
    let mut reps = [1u32, 4, 8];
    let n = d.len();
    let best = |i: usize, reps: &[u32; 3]| -> Cand {
        let mut c = cands.longest(d, i);
        for &r in reps {
            if (r as usize) <= i {
                let l = extend(d, i - r as usize, i, (n - i).min(MAX_MATCH));
                if l >= 3 && l + 1 >= c.len {
                    c = Cand { off: r, len: l };
                    break;
                }
            }
        }
        c
    };
    while i < n {
        let mut c = best(i, &reps);
        if c.len < 3 {
            lit += 1;
            i += 1;
            continue;
        }
        let mut skip = 0;
        for la in 1..=2usize {
            if i + la >= n {
                break;
            }
            let c2 = best(i + la, &reps);
            if c2.len >= c.len + la as u32 + 1 {
                c = c2;
                skip = la;
            }
        }
        lit += skip as u32;
        i += skip;
        seqs.push(Seq { offset: c.off, lit_length: lit, match_length: c.len, rep: 0 });
        lit = 0;
        i += c.len as usize;
        reps = push_rep(&reps, c.off);
    }
    seqs
}

// ---- an optimal parse, as zstd's btultra does: a shortest path over positions where every
// edge is a literal or a match (candidate or repeat offset), priced in bits from code
// statistics; two passes, the second priced with the first's statistics.

const LL_CODE: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20, 20, 20, 21, 21, 21, 21,
    22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23, 23, 23, 23, 23, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24,
];
const LL_BITS: [u8; 36] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
const ML_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8,
    9, 10, 11, 12, 13, 14, 15, 16,
];

#[inline]
fn highbit(v: u32) -> usize {
    31 - v.leading_zeros() as usize
}

#[inline]
fn ll_code(ll: u32) -> usize {
    if ll > 63 { (highbit(ll) + 19).min(35) } else { LL_CODE[ll as usize] as usize }
}

#[inline]
fn ml_code(ml: u32) -> usize {
    let b = ml - 3;
    if b > 127 {
        (highbit(b) + 36).min(52)
    } else if b < 32 {
        b as usize
    } else {
        // 32,32,33,33,34,34,35,35, 36x4, 37x4, 38x8, 39x8, 40x16, 41x16, 42x32
        match b {
            32..=39 => 32 + (b as usize - 32) / 2,
            40..=47 => 36 + (b as usize - 40) / 4,
            48..=63 => 38 + (b as usize - 48) / 8,
            64..=95 => 40 + (b as usize - 64) / 16,
            _ => 42,
        }
    }
}

struct Prices {
    lit: [f32; 256],
    ll: [f32; 36],
    ml: [f32; 53],
    off: [f32; 32],
}

impl Prices {
    fn flat(d: &[u8]) -> Prices {
        let mut h = [1u64; 256];
        for &b in d.iter().step_by(16) {
            h[b as usize] += 1;
        }
        let total: u64 = h.iter().sum();
        let mut p = Prices { lit: [8.0; 256], ll: [4.0; 36], ml: [4.0; 53], off: [5.0; 32] };
        for b in 0..256 {
            p.lit[b] = (-(h[b] as f64 / total as f64).log2()).clamp(1.0, 11.0) as f32;
        }
        p
    }

    /// prices from how often each code was used in a parse
    fn from_stats(s: &Stats) -> Prices {
        fn table<const N: usize>(h: &[u64; N], floor: f32, cap: f32) -> [f32; N] {
            let total: u64 = h.iter().sum::<u64>() + N as u64;
            let mut out = [0f32; N];
            for i in 0..N {
                out[i] = (-((h[i] + 1) as f64 / total as f64).log2()).clamp(floor as f64, cap as f64) as f32;
            }
            out
        }
        Prices { lit: table(&s.lit, 1.0, 11.0), ll: table(&s.ll, 0.5, 14.0), ml: table(&s.ml, 0.5, 14.0), off: table(&s.off, 0.5, 14.0) }
    }

    #[inline]
    fn seq(&self, ll: u32, ml: u32, off_base: u32) -> f32 {
        let (lc, mc, oc) = (ll_code(ll), ml_code(ml), highbit(off_base));
        self.ll[lc] + LL_BITS[lc] as f32 + self.ml[mc] + ML_BITS[mc] as f32 + self.off[oc] + oc as f32
    }
}

struct Stats {
    lit: [u64; 256],
    ll: [u64; 36],
    ml: [u64; 53],
    off: [u64; 32],
}

impl Default for Stats {
    fn default() -> Stats {
        Stats { lit: [0; 256], ll: [0; 36], ml: [0; 53], off: [0; 32] }
    }
}

#[inline]
fn off_base(off: u32, reps: &[u32; 3]) -> u32 {
    if off == reps[0] {
        1
    } else if off == reps[1] {
        2
    } else if off == reps[2] {
        3
    } else {
        off + 3
    }
}

#[inline]
fn push_rep(reps: &[u32; 3], off: u32) -> [u32; 3] {
    if off == reps[0] {
        *reps
    } else if off == reps[1] {
        [reps[1], reps[0], reps[2]]
    } else {
        [off, reps[0], reps[1]]
    }
}

#[derive(Clone, Copy)]
struct Node {
    cost: f32,
    from: u32,   // position this one was reached from
    off: u32,    // 0: by a literal
    len: u32,
    litrun: u32, // literals pending on the best path into this position
    reps: [u32; 3],
}

const SEG: usize = 1 << 20; // the parse's dynamic programme runs over segments this long
const SUFFICIENT: u32 = 64; // a match this long is taken whole, shorter ones at every length

fn parse_optimal_pass(d: &[u8], cands: &Cands, prices: &Prices, stats: &mut Stats) -> Vec<Seq> {
    let n = d.len();
    let mut seqs = Vec::new();
    let mut reps0 = [1u32, 4, 8];
    let mut lit_carry = 0u32; // literals at a segment's end join the next segment's first sequence
    let inf = f32::INFINITY;
    let mut nodes = vec![Node { cost: inf, from: 0, off: 0, len: 0, litrun: 0, reps: reps0 }; SEG + 1];
    let mut seg_start = 0;
    while seg_start < n {
        let seg_len = SEG.min(n - seg_start);
        for x in nodes[..=seg_len].iter_mut() {
            x.cost = inf;
        }
        nodes[0] = Node { cost: 0.0, from: 0, off: 0, len: 0, litrun: lit_carry, reps: reps0 };
        for i in 0..seg_len {
            let cur = nodes[i];
            if cur.cost == inf {
                continue;
            }
            let pos = seg_start + i;
            // literal
            let c = cur.cost + prices.lit[d[pos] as usize];
            if c < nodes[i + 1].cost {
                nodes[i + 1] = Node { cost: c, from: i as u32, off: 0, len: 0, litrun: cur.litrun + 1, reps: cur.reps };
            }
            let room = ((seg_len - i) as u32).min(MAX_MATCH as u32);
            if room < 3 {
                continue;
            }
            let mut try_match = |off: u32, maxlen: u32, nodes: &mut Vec<Node>| {
                let maxlen = maxlen.min(room);
                if maxlen < 3 {
                    return;
                }
                let ob = off_base(off, &cur.reps);
                let reps = push_rep(&cur.reps, off);
                let mut l = if maxlen >= SUFFICIENT { maxlen } else { 3 };
                loop {
                    let c = cur.cost + prices.seq(cur.litrun, l, ob);
                    let t = &mut nodes[i + l as usize];
                    if c < t.cost {
                        *t = Node { cost: c, from: i as u32, off, len: l, litrun: 0, reps };
                    }
                    if l == maxlen {
                        break;
                    }
                    l += 1;
                }
            };
            // repeat offsets first: they are the cheapest to encode
            for &r in &cur.reps {
                if (r as usize) <= pos {
                    let l = extend(d, pos - r as usize, pos, room as usize);
                    try_match(r, l, &mut nodes);
                }
            }
            for s in 0..cands.k {
                let c = cands.get(d, pos, s);
                if c.len >= 3 {
                    try_match(c.off, c.len, &mut nodes);
                }
            }
        }
        // walk back from the segment's end, then emit forwards
        let mut path = Vec::new();
        let mut i = seg_len;
        while i > 0 {
            let x = nodes[i];
            if x.off != 0 {
                path.push((x.off, x.len, nodes[x.from as usize].litrun));
            }
            i = x.from as usize;
        }
        let end = nodes[seg_len];
        for &(off, len, litrun) in path.iter().rev() {
            seqs.push(Seq { offset: off, lit_length: litrun, match_length: len, rep: 0 });
        }
        lit_carry = end.litrun;
        reps0 = end.reps;
        seg_start += seg_len;
    }
    // statistics for the next pass's prices
    let mut pos = 0usize;
    let mut reps = [1u32, 4, 8];
    for s in &seqs {
        for &b in &d[pos..pos + s.lit_length as usize] {
            stats.lit[b as usize] += 1;
        }
        pos += s.lit_length as usize;
        stats.ll[ll_code(s.lit_length)] += 1;
        stats.ml[ml_code(s.match_length)] += 1;
        stats.off[highbit(off_base(s.offset, &reps))] += 1;
        reps = push_rep(&reps, s.offset);
        pos += s.match_length as usize;
    }
    seqs
}

fn parse_optimal(d: &[u8], cands: &Cands) -> Vec<Seq> {
    let mut stats = Stats::default();
    parse_optimal_pass(d, cands, &Prices::flat(d), &mut stats);
    let prices = Prices::from_stats(&stats);
    let mut stats2 = Stats::default();
    parse_optimal_pass(d, cands, &prices, &mut stats2)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: zseq <file> [--k 3] [--window-log 27] [--levels 19,22|none] [--parsers greedy,lazy,optimal] [--cap 64]");
        std::process::exit(2);
    }
    let (mut window_log, mut k, mut cap) = (27, 3usize, 64u32);
    let mut levels = vec![19, 22];
    let mut parsers = vec!["greedy".to_string(), "lazy".to_string(), "optimal".to_string()];
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--window-log" => window_log = args[i + 1].parse().unwrap(),
            "--k" => k = args[i + 1].parse().unwrap(),
            "--cap" => cap = args[i + 1].parse().unwrap(),
            "--levels" => levels = args[i + 1].split(',').filter(|s| *s != "none").map(|s| s.parse().unwrap()).collect(),
            "--parsers" => parsers = args[i + 1].split(',').map(|s| s.to_string()).collect(),
            _ => {}
        }
        i += 2;
    }
    let data = std::fs::read(&args[1]).expect("read input");
    let cand_file = if k == 1 { format!("{}.cand", args[1]) } else { format!("{}.cand{k}", args[1]) };
    let raw = std::fs::read(&cand_file).expect("read the candidates (run gpumatch --dump)");
    assert_eq!(raw.len(), data.len() * 8 * k, "{cand_file} does not match the input");
    let v: Vec<Cand> = raw
        .chunks_exact(8)
        .map(|c| Cand { off: u32::from_le_bytes(c[..4].try_into().unwrap()), len: u32::from_le_bytes(c[4..].try_into().unwrap()) })
        .collect();
    drop(raw);
    let cands = Cands { k, cap, v };
    let mb = data.len() as f64 / 1e6;
    println!("{}: {:.0} MB, window 2^{window_log}, {k} candidates per position", args[1], mb);
    for &lvl in &levels {
        let (n, s) = zstd_level(&data, lvl, window_log, &[(C_ENABLE_LDM, 1)]);
        println!("  zstd -{lvl:<2} + LDM            {:>11} bytes  ratio {:6.3}  {:6.1} s ({:.1} MB/s)", n, data.len() as f64 / n as f64, s, mb / s);
    }
    let all: [(&str, fn(&[u8], &Cands) -> Vec<Seq>); 3] = [("greedy", parse_greedy), ("lazy", parse_lazy), ("optimal", parse_optimal)];
    for (name, f) in all {
        if !parsers.iter().any(|p| p == name) {
            continue;
        }
        let t = Instant::now();
        let seqs = f(&data, &cands);
        let tp = t.elapsed().as_secs_f64();
        let covered: u64 = seqs.iter().map(|s| (s.lit_length + s.match_length) as u64).sum();
        let lits: u64 = seqs.iter().map(|s| s.lit_length as u64).sum::<u64>() + (data.len() as u64 - covered);
        let t = Instant::now();
        let n = compress_sequences(&data, &seqs, 19, window_log);
        println!(
            "  GPU candidates, {name:<9}{:>11} bytes  ratio {:6.3}  parse {:.1} s, zstd entropy coding {:.1} s; {} sequences, {:.1}% literals",
            n,
            data.len() as f64 / n as f64,
            tp,
            t.elapsed().as_secs_f64(),
            seqs.len(),
            100.0 * lits as f64 / data.len() as f64
        );
    }
}
