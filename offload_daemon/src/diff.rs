// Copyright © 2026 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Incremental checkpoints from CH's dirty log.
//!
//! With `send-migration ... dirty_log=keep|consume`, CH reports the guest
//! pages written since the previous checkpoint (`Command::DirtyLog`). A diff
//! checkpoint then looks at those pages only. Each dirty page is compared
//! against a *reference* copy of the guest as of the previous checkpoint (a
//! sparse per-slot file the full checkpoint initialises), because many pages
//! are written without changing (false dirty). The pages that did change are
//! copied both into the reference and into a contiguous gather buffer, which
//! is compressed in `chunk_size` pieces like a full checkpoint. The diff
//! manifest records, in gather order, which slot pages the buffer holds, and
//! the directory names its parent checkpoint; restore applies the chain.
//!
//! Comparison and copy run on the CPU (`memcmp`, two `memcpy`) or on DSA as
//! batches of COMPARE and DUALCAST descriptors (one descriptor per 4 KiB page,
//! `batch` pages per batch descriptor, `depth` batches in flight); any DSA
//! operation that does not complete successfully is redone on the CPU.

// Diff checkpoints are written only by the async QPL path; restore needs
// the rest. Without QPL the snapshot half is compiled but unused.
#![cfg_attr(not(feature = "qpl"), allow(dead_code))]

use std::fmt;
#[cfg(feature = "qpl")]
use std::fs::OpenOptions;
use std::fs::{self, File};
#[cfg(all(feature = "qpl", feature = "dto"))]
use std::hint;
use std::io;
#[cfg(feature = "qpl")]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
#[cfg(feature = "qpl")]
use std::ptr;
use std::str::FromStr;
#[cfg(feature = "qpl")]
use std::thread;
use std::time::Duration;
#[cfg(feature = "qpl")]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::compression::{ChunkRecord, Codec, CodecWorker, Destination, Error as CompressionError};
#[cfg(feature = "qpl")]
use crate::crc::is_zero;
use crate::dedup::{self, DedupImage, DedupRun};
#[cfg(feature = "dto")]
use crate::dto::{Batch, BatchPoll, Submit, ZeroBuffer};
#[cfg(feature = "qpl")]
use crate::mapping::{Mapping, data_extents};
#[cfg(feature = "qpl")]
use crate::qpl::{Error as QplError, ExecutionPath, HuffmanMode, JobPool};

pub(crate) const PAGE: usize = 4096;
pub(crate) const DIFF_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffCompare {
    /// Trust the dirty log: every dirty page is stored.
    None,
    Cpu,
    Dsa,
}

impl fmt::Display for DiffCompare {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::None => "none",
            Self::Cpu => "cpu",
            Self::Dsa => "dsa",
        })
    }
}

#[derive(Debug, Error)]
#[error("Unknown diff compare mode {0:?} (expected none, cpu or dsa)")]
pub(crate) struct ParseDiffCompareError(String);

impl FromStr for DiffCompare {
    type Err = ParseDiffCompareError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "cpu" => Ok(Self::Cpu),
            "dsa" => Ok(Self::Dsa),
            _ => Err(ParseDiffCompareError(value.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(all(feature = "qpl", feature = "dto")), allow(dead_code))]
pub(crate) struct DiffOptions {
    pub compare: DiffCompare,
    /// Pages per DSA batch descriptor.
    pub batch: usize,
    /// DSA batch descriptors in flight.
    pub depth: usize,
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("Diff checkpoint I/O")]
    Io(#[from] io::Error),
    #[error("Diff compression")]
    Compression(#[from] CompressionError),
    #[cfg(feature = "qpl")]
    #[error("Diff QPL")]
    Qpl(#[from] QplError),
    #[error("Diff manifest")]
    Json(#[from] serde_json::Error),
    #[error("Invalid diff checkpoint: {0}")]
    Invalid(String),
    #[cfg_attr(feature = "dto", allow(dead_code))]
    #[error("DSA diff comparison requires the dto feature")]
    DsaUnavailable,
}

/// `diff.json` in a diff checkpoint directory.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DiffMarker {
    pub parent: PathBuf,
}

/// `memory-<slot>.diff.json`.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DiffManifest {
    pub version: u32,
    pub codec: Codec,
    pub chunk_size: u32,
    pub slot_size: u64,
    /// (slot byte offset, pages) runs, in gather-buffer order.
    pub runs: Vec<(u64, u32)>,
    /// Compressed pieces of the gather buffer.
    pub chunks: Vec<ChunkRecord>,
    pub dirty_pages: u64,
    pub changed_pages: u64,
    /// Changed pages stored as references to blocks of a disk image.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dedup: Vec<DedupRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_image: Option<DedupImage>,
}

pub(crate) fn marker_path(dir: &Path) -> PathBuf {
    dir.join("diff.json")
}

pub(crate) fn data_path(dir: &Path, slot: u32) -> PathBuf {
    dir.join(format!("memory-{slot}.diff"))
}

pub(crate) fn manifest_path(dir: &Path, slot: u32) -> PathBuf {
    dir.join(format!("memory-{slot}.diff.json"))
}

pub(crate) fn reference_path(dir: &Path, slot: u32) -> PathBuf {
    dir.join(format!("memory-{slot}.ref"))
}

/// Checkpoint directories from `dir` back to its full base: `[dir, parent,
/// ..., base]`.
pub(crate) fn chain(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut out = vec![dir.to_path_buf()];
    let mut current = dir.to_path_buf();
    while marker_path(&current).exists() {
        let marker: DiffMarker = serde_json::from_slice(&fs::read(marker_path(&current))?)?;
        let parent = if marker.parent.is_absolute() {
            marker.parent
        } else {
            current.join(marker.parent)
        };
        if out.len() > 4096 || out.contains(&parent) {
            return Err(Error::Invalid(format!(
                "diff chain loops at {}",
                parent.display()
            )));
        }
        out.push(parent.clone());
        current = parent;
    }
    Ok(out)
}

/// Slot-relative, page-aligned byte ranges of the dirty `(gpa, length)`
/// ranges that fall inside the slot `[slot_gpa, slot_gpa + slot_size)`.
pub(crate) fn slot_dirty_ranges(
    ranges: &[(u64, u64)],
    slot_gpa: u64,
    slot_size: u64,
) -> Vec<(usize, usize)> {
    let page = PAGE as u64;
    let mut out: Vec<(usize, usize)> = ranges
        .iter()
        .filter_map(|&(gpa, length)| {
            let start = gpa.max(slot_gpa);
            let end = (gpa + length).min(slot_gpa + slot_size);
            (start < end).then(|| {
                let s = (start - slot_gpa) / page * page;
                let e = (end - slot_gpa).div_ceil(page) * page;
                (s as usize, e.min(slot_size) as usize)
            })
        })
        .collect();
    out.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(out.len());
    for (s, e) in out {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

pub(crate) fn runs_of(pages: &[usize]) -> Vec<(u64, u32)> {
    let mut runs: Vec<(u64, u32)> = Vec::new();
    for &p in pages {
        match runs.last_mut() {
            Some((start, n)) if *start as usize + *n as usize * PAGE == p => *n += 1,
            _ => runs.push((p as u64, 1)),
        }
    }
    runs
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DiffStats {
    pub dirty_pages: u64,
    pub changed_pages: u64,
    pub prepare: Duration,
    pub compare: Duration,
    pub gather: Duration,
    pub compress: Duration,
    pub output_bytes: u64,
    pub dsa_ops: u64,
    pub dsa_cpu_redo: u64,
    pub populate_calls: u64,
    pub dedup_pages: u64,
    pub dedup: Duration,
}

/// Anonymous, populated, page-aligned buffer.
#[cfg(feature = "qpl")]
pub(crate) struct Buffer {
    address: ptr::NonNull<u8>,
    length: usize,
}

#[cfg(feature = "qpl")]
impl Buffer {
    pub(crate) fn new(length: usize) -> io::Result<Self> {
        let length = length.max(PAGE);
        // SAFETY: anonymous private mapping; the result is checked.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            address: ptr::NonNull::new(address.cast()).expect("mmap returned null"),
            length,
        })
    }

    pub(crate) fn page(&self, index: usize) -> *mut u8 {
        debug_assert!((index + 1) * PAGE <= self.length);
        // SAFETY: within the mapping.
        unsafe { self.address.as_ptr().add(index * PAGE) }
    }
}

#[cfg(feature = "qpl")]
impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: unmapping the region created in `new`.
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
    }
}

#[cfg(feature = "qpl")]
fn in_extents(extents: &[(usize, usize)], offset: usize) -> bool {
    let i = extents.partition_point(|&(_, end)| end <= offset);
    extents.get(i).is_some_and(|&(start, _)| start <= offset)
}

#[cfg(feature = "qpl")]
fn page_slice(mapping: &Mapping, offset: usize) -> io::Result<&[u8]> {
    mapping.slice(offset, PAGE)
}

/// Copy `src` page into the reference page and the gather page on the CPU.
///
/// # Safety
/// The three pages are valid, distinct, and not accessed concurrently.
#[cfg(feature = "qpl")]
unsafe fn copy_two(reference: *mut u8, gather: *mut u8, src: *const u8, reference_too: bool) {
    // SAFETY: caller guarantees validity and disjointness.
    unsafe {
        ptr::copy_nonoverlapping(src, gather, PAGE);
        if reference_too {
            ptr::copy_nonoverlapping(src, reference, PAGE);
        }
    }
}

/// Snapshot one slot as a diff against its reference.
#[cfg(feature = "qpl")]
#[expect(clippy::too_many_arguments)]
pub(crate) fn diff_snapshot_slot(
    source: &File,
    source_offset: u64,
    size: u64,
    reference: &Path,
    dirty: &[(usize, usize)],
    out_dir: &Path,
    slot: u32,
    codec: Codec,
    huffman: HuffmanMode,
    chunk_size: usize,
    workers: usize,
    options: DiffOptions,
) -> Result<DiffStats, Error> {
    let mut stats = DiffStats::default();
    let started = Instant::now();
    let src = Mapping::map_file(source, source_offset, size, false)?;
    let reference_file = OpenOptions::new().read(true).write(true).open(reference)?;
    if reference_file.metadata()?.len() != size {
        return Err(Error::Invalid(format!(
            "reference {} does not match slot size {size}",
            reference.display()
        )));
    }
    let refm = Mapping::map_file(&reference_file, 0, size, true)?;
    let ref_extents = data_extents(&reference_file, 0, size)?;
    let pages: Vec<usize> = dirty
        .iter()
        .flat_map(|&(s, e)| (s..e).step_by(PAGE))
        .collect();
    stats.dirty_pages = pages.len() as u64;
    // Page-table entries for what the engines will touch: the dirty source
    // pages (the guest wrote them, so they exist) and the reference pages
    // that hold data. Reference holes are compared against zero instead.
    for &(s, e) in dirty {
        src.populate_range(s, e - s, false)?;
    }
    for &(s, e) in dirty {
        let mut i = ref_extents.partition_point(|&(_, end)| end <= s);
        while let Some(&(rs, re)) = ref_extents.get(i) {
            if rs >= e {
                break;
            }
            let (a, b) = (rs.max(s), re.min(e));
            refm.populate_range(a, b - a, true)?;
            i += 1;
        }
    }
    let has_ref: Vec<bool> = pages.iter().map(|&p| in_extents(&ref_extents, p)).collect();
    stats.prepare = started.elapsed();

    // 1. which dirty pages changed
    let t = Instant::now();
    let changed: Vec<usize> = match options.compare {
        DiffCompare::None => pages.clone(),
        DiffCompare::Cpu => {
            let mut out = Vec::new();
            for (i, &p) in pages.iter().enumerate() {
                let page = page_slice(&src, p)?;
                let differs = if has_ref[i] {
                    page != page_slice(&refm, p)?
                } else {
                    !is_zero(page)
                };
                if differs {
                    out.push(p);
                }
            }
            out
        }
        DiffCompare::Dsa => compare_dsa(&src, &refm, &pages, &has_ref, options, &mut stats)?,
    };
    stats.compare = t.elapsed();
    stats.changed_pages = changed.len() as u64;

    // 2. gather the changed pages and bring the reference up to date
    let t = Instant::now();
    let runs = runs_of(&changed);
    let update_reference = options.compare != DiffCompare::None;
    if update_reference {
        // Reference pages written below must exist first: a device write to a
        // hole would take a page-request fault per page.
        for &(start, n) in &runs {
            refm.populate_range(start as usize, n as usize * PAGE, true)?;
        }
    }
    let gather = Buffer::new(changed.len() * PAGE)?;
    match options.compare {
        DiffCompare::Dsa => gather_dsa(&src, &refm, &gather, &changed, options, &mut stats)?,
        _ => {
            for (i, &p) in changed.iter().enumerate() {
                // SAFETY: page-aligned pages inside three distinct live mappings.
                unsafe {
                    copy_two(
                        refm.mut_ptr(p, PAGE)?,
                        gather.page(i),
                        src.ptr(p, PAGE)?,
                        update_reference,
                    );
                };
            }
        }
    }
    stats.gather = t.elapsed();

    // 3. compress the gather buffer
    let t = Instant::now();
    let data = data_path(out_dir, slot);
    let output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&data)?;
    let total = changed.len() * PAGE;
    let mut chunks = Vec::new();
    let mut output_offset = 0_u64;
    if total > 0 {
        let mut pool = JobPool::new(ExecutionPath::Hardware, huffman, workers)?;
        let count = total.div_ceil(chunk_size);
        let mut active: Vec<Option<(usize, usize)>> = vec![None; pool.capacity()];
        let mut next = 0_usize;
        let mut live = 0_usize;
        loop {
            for (slot_index, entry) in active.iter_mut().enumerate() {
                if let Some((offset, length)) = *entry {
                    let Some(size_out) = pool.poll(slot_index)? else {
                        continue;
                    };
                    output.write_all_at(pool.output(slot_index, size_out), output_offset)?;
                    chunks.push(ChunkRecord {
                        uncompressed_offset: offset as u64,
                        uncompressed_length: length as u32,
                        compressed_offset: output_offset,
                        compressed_length: size_out as u32,
                        zero: false,
                        crc32c: None,
                    });
                    output_offset += size_out as u64;
                    *entry = None;
                    live -= 1;
                }
                if entry.is_none() && next < count {
                    let offset = next * chunk_size;
                    let length = (total - offset).min(chunk_size);
                    // SAFETY: the gather buffer outlives the pool and is not
                    // written while jobs run.
                    unsafe {
                        pool.submit_compress_from(slot_index, gather.page(offset / PAGE), length)?;
                    };
                    *entry = Some((offset, length));
                    next += 1;
                    live += 1;
                }
            }
            if next == count && live == 0 {
                break;
            }
            thread::yield_now();
        }
        chunks.sort_unstable_by_key(|r| r.uncompressed_offset);
    }
    output.sync_all()?;
    let manifest = DiffManifest {
        version: DIFF_VERSION,
        codec,
        chunk_size: chunk_size as u32,
        slot_size: size,
        runs,
        chunks,
        dirty_pages: stats.dirty_pages,
        changed_pages: stats.changed_pages,
        dedup: Vec::new(),
        dedup_image: None,
    };
    fs::write(manifest_path(out_dir, slot), serde_json::to_vec(&manifest)?)?;
    stats.compress = t.elapsed();
    stats.output_bytes = output_offset;
    Ok(stats)
}

#[cfg(all(feature = "qpl", feature = "dto"))]
fn compare_dsa(
    src: &Mapping,
    refm: &Mapping,
    pages: &[usize],
    has_ref: &[bool],
    options: DiffOptions,
    stats: &mut DiffStats,
) -> Result<Vec<usize>, Error> {
    let zero = ZeroBuffer::new(PAGE)?;
    let depth = options.depth.max(1);
    let mut batches: Vec<Batch> = (0..depth)
        .map(|_| Batch::new(options.batch))
        .collect::<io::Result<_>>()?;
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); depth];
    let mut live = vec![false; depth];
    let mut changed = Vec::new();
    let mut next = 0_usize;
    let cpu_differs = |i: usize| -> io::Result<bool> {
        let page = page_slice(src, pages[i])?;
        Ok(if has_ref[i] {
            page != page_slice(refm, pages[i])?
        } else {
            !is_zero(page)
        })
    };
    while next < pages.len() || live.iter().any(|l| *l) {
        for k in 0..depth {
            if live[k] {
                if batches[k].poll() == BatchPoll::Pending {
                    continue;
                }
                for (j, &i) in members[k].iter().enumerate() {
                    if batches[k].status(j) == 1 {
                        if batches[k].differs(j) {
                            changed.push(pages[i]);
                        }
                    } else {
                        stats.dsa_cpu_redo += 1;
                        if cpu_differs(i)? {
                            changed.push(pages[i]);
                        }
                    }
                }
                live[k] = false;
            }
            if next < pages.len() {
                batches[k].reset();
                members[k].clear();
                while next < pages.len() && members[k].len() < options.batch {
                    let p = pages[next];
                    let other = if has_ref[next] {
                        refm.ptr(p, PAGE)?
                    } else {
                        zero.as_ptr()
                    };
                    // SAFETY: both pages stay mapped and unchanged (VM paused,
                    // reference owned by this process) until the batch completes.
                    if !unsafe { batches[k].add_compare(src.ptr(p, PAGE)?, other, PAGE) } {
                        break;
                    }
                    members[k].push(next);
                    next += 1;
                }
                stats.dsa_ops += members[k].len() as u64;
                if batches[k].submit() == Submit::Submitted {
                    live[k] = true;
                } else {
                    stats.dsa_cpu_redo += members[k].len() as u64;
                    for &i in &members[k] {
                        if cpu_differs(i)? {
                            changed.push(pages[i]);
                        }
                    }
                }
            }
        }
    }
    changed.sort_unstable();
    Ok(changed)
}

#[cfg(all(feature = "qpl", feature = "dto"))]
fn gather_dsa(
    src: &Mapping,
    refm: &Mapping,
    gather: &Buffer,
    changed: &[usize],
    options: DiffOptions,
    stats: &mut DiffStats,
) -> Result<(), Error> {
    let depth = options.depth.max(1);
    let mut batches: Vec<Batch> = (0..depth)
        .map(|_| Batch::new(options.batch))
        .collect::<io::Result<_>>()?;
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); depth];
    let mut live = vec![false; depth];
    let mut next = 0_usize;
    let cpu_copy = |i: usize| -> io::Result<()> {
        let p = changed[i];
        // SAFETY: page-aligned pages inside three distinct live mappings.
        unsafe {
            copy_two(
                refm.mut_ptr(p, PAGE)?,
                gather.page(i),
                src.ptr(p, PAGE)?,
                true,
            );
        };
        Ok(())
    };
    while next < changed.len() || live.iter().any(|l| *l) {
        for k in 0..depth {
            if live[k] {
                if batches[k].poll() == BatchPoll::Pending {
                    continue;
                }
                for (j, &i) in members[k].iter().enumerate() {
                    if batches[k].status(j) != 1 {
                        stats.dsa_cpu_redo += 1;
                        cpu_copy(i)?;
                    }
                }
                live[k] = false;
            }
            if next < changed.len() {
                batches[k].reset();
                members[k].clear();
                while next < changed.len() && members[k].len() < options.batch {
                    let p = changed[next];
                    // SAFETY: the source is stable (VM paused); the reference
                    // page and the gather page are owned by this process and
                    // untouched until the batch completes.
                    if !unsafe {
                        batches[k].add_dualcast(
                            refm.mut_ptr(p, PAGE)?,
                            gather.page(next),
                            src.ptr(p, PAGE)?,
                            PAGE,
                        )
                    } {
                        break;
                    }
                    members[k].push(next);
                    next += 1;
                }
                stats.dsa_ops += members[k].len() as u64;
                if batches[k].submit() == Submit::Submitted {
                    live[k] = true;
                } else {
                    stats.dsa_cpu_redo += members[k].len() as u64;
                    for &i in &members[k] {
                        cpu_copy(i)?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(feature = "qpl", not(feature = "dto")))]
fn compare_dsa(
    _: &Mapping,
    _: &Mapping,
    _: &[usize],
    _: &[bool],
    _: DiffOptions,
    _: &mut DiffStats,
) -> Result<Vec<usize>, Error> {
    Err(Error::DsaUnavailable)
}

#[cfg(all(feature = "qpl", not(feature = "dto")))]
fn gather_dsa(
    _: &Mapping,
    _: &Mapping,
    _: &Buffer,
    _: &[usize],
    _: DiffOptions,
    _: &mut DiffStats,
) -> Result<(), Error> {
    Err(Error::DsaUnavailable)
}

/// After a full checkpoint: make `reference` a copy of the slot's non-zero
/// chunks (zero chunks stay holes, which read as zero).
#[cfg(feature = "qpl")]
pub(crate) fn init_reference(
    source: &File,
    source_offset: u64,
    size: u64,
    reference: &Path,
    nonzero: &[(usize, usize)],
    use_dsa: bool,
) -> Result<Duration, Error> {
    let started = Instant::now();
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(reference)?;
    file.set_len(size)?;
    let src = Mapping::map_file(source, source_offset, size, false)?;
    let refm = Mapping::map_file(&file, 0, size, true)?;
    for &(offset, length) in nonzero {
        src.populate_range(offset, length, false)?;
        refm.populate_range(offset, length, true)?;
    }
    #[cfg(feature = "dto")]
    if use_dsa {
        let mut batch = Batch::new(1024)?;
        for group in nonzero.chunks(1024) {
            batch.reset();
            for &(offset, length) in group {
                // SAFETY: source stable while paused; reference owned here.
                unsafe {
                    batch.add_memmove(
                        refm.mut_ptr(offset, length)?,
                        src.ptr(offset, length)?,
                        length,
                    )
                };
            }
            let submitted = batch.submit() == Submit::Submitted;
            if submitted {
                while batch.poll() == BatchPoll::Pending {
                    hint::spin_loop();
                }
            }
            for (j, &(offset, length)) in group.iter().enumerate() {
                if !submitted || batch.status(j) != 1 {
                    refm.write(offset, src.slice(offset, length)?)?;
                }
            }
        }
        return Ok(started.elapsed());
    }
    let _ = use_dsa;
    for &(offset, length) in nonzero {
        refm.write(offset, src.slice(offset, length)?)?;
    }
    Ok(started.elapsed())
}

/// Apply one diff checkpoint's pages for `slot` onto the restored memfd.
pub(crate) fn apply_diff(
    dir: &Path,
    slot: u32,
    destination: &File,
    destination_offset: u64,
    size: u64,
) -> Result<u64, Error> {
    let manifest: DiffManifest = serde_json::from_slice(&fs::read(manifest_path(dir, slot))?)?;
    if manifest.version != DIFF_VERSION || manifest.slot_size != size {
        return Err(Error::Invalid(format!(
            "{}: version/size mismatch",
            dir.display()
        )));
    }
    let total: usize = manifest.runs.iter().map(|&(_, n)| n as usize * PAGE).sum();
    let destination = Destination::open(destination, destination_offset, size)?;
    if let Some(image) = &manifest.dedup_image {
        dedup::apply(image, &manifest.dedup, &destination)?;
    }
    if total == 0 {
        return Ok(manifest.changed_pages);
    }
    let data = fs::read(data_path(dir, slot))?;
    let mut gather = vec![0_u8; total];
    let mut worker = CodecWorker::new(manifest.codec, 1)?;
    for record in &manifest.chunks {
        let start = record.compressed_offset as usize;
        let end = start + record.compressed_length as usize;
        let input = data
            .get(start..end)
            .ok_or_else(|| Error::Invalid("chunk beyond data".to_owned()))?;
        let out = worker.decompress(input, record.uncompressed_length as usize)?;
        let at = record.uncompressed_offset as usize;
        gather
            .get_mut(at..at + out.len())
            .ok_or_else(|| Error::Invalid("chunk beyond gather".to_owned()))?
            .copy_from_slice(out);
    }
    let mut at = 0_usize;
    for &(offset, pages) in &manifest.runs {
        let length = pages as usize * PAGE;
        destination.write(offset as usize, &gather[at..at + length])?;
        at += length;
    }
    Ok(manifest.changed_pages)
}

/// Which of `pages` differ from the reference (or from zero where
/// `has_ref[i]` is false), by the configured method.
#[cfg(feature = "qpl")]
pub(crate) fn changed_pages(
    src: &Mapping,
    refm: &Mapping,
    pages: &[usize],
    has_ref: &[bool],
    options: DiffOptions,
    stats: &mut DiffStats,
) -> Result<Vec<usize>, Error> {
    match options.compare {
        DiffCompare::None => Ok(pages.to_vec()),
        DiffCompare::Cpu => {
            let mut out = Vec::new();
            for (i, &p) in pages.iter().enumerate() {
                let page = page_slice(src, p)?;
                let differs = if has_ref[i] {
                    page != page_slice(refm, p)?
                } else {
                    !is_zero(page)
                };
                if differs {
                    out.push(p);
                }
            }
            Ok(out)
        }
        DiffCompare::Dsa => compare_dsa(src, refm, pages, has_ref, options, stats),
    }
}

/// Copy the `changed` pages into `gather` (in order) and, unless the dirty
/// log is trusted as-is, into the reference.
#[cfg(feature = "qpl")]
pub(crate) fn gather_pages(
    src: &Mapping,
    refm: &Mapping,
    gather: &Buffer,
    changed: &[usize],
    options: DiffOptions,
    stats: &mut DiffStats,
) -> Result<(), Error> {
    if options.compare == DiffCompare::Dsa {
        return gather_dsa(src, refm, gather, changed, options, stats);
    }
    let update_reference = options.compare != DiffCompare::None;
    for (i, &p) in changed.iter().enumerate() {
        // SAFETY: page-aligned pages inside three distinct live mappings.
        unsafe {
            copy_two(
                refm.mut_ptr(p, PAGE)?,
                gather.page(i),
                src.ptr(p, PAGE)?,
                update_reference,
            );
        }
    }
    Ok(())
}
