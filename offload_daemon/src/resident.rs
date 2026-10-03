// Copyright © 2026 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Resident checkpointing (`offload_daemon serve`).
//!
//! A one-shot `snapshot` daemon starts, maps every guest slot, builds an IAA
//! job pool per slot thread, populates page tables, checkpoints once and
//! exits. A resident daemon keeps all of that across checkpoints of the same
//! VM: the slot mappings (and a bitmap of which pages already have page-table
//! entries in them), the reference copy used for dirty-log diffs (mapped,
//! with a bitmap of which reference pages hold data), DTO's work queues, and
//! one IAA job pool shared by every slot. Each checkpoint then only
//! populates pages it has never mapped, and does so in as few calls as
//! possible: on hugetlbfs (no holes) one call over the dirty span; on shmem
//! one call per run of unmapped pages, with runs bridged across pages that
//! are already mapped (populating those again is harmless, whereas
//! populating a hole would allocate it).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use log::info;
use thiserror::Error;

use crate::classify::AccelOptions;
use crate::compression::{
    ChunkRecord, Codec, Error as CompressionError, SlotJob, SlotManifest, compress_slots,
    validate_chunk_size,
};
use crate::dedup::{self, DedupRun, Index};
use crate::diff::{
    self, Buffer, DIFF_VERSION, DiffCompare, DiffManifest, DiffMarker, DiffOptions, DiffStats, PAGE,
};
#[cfg(feature = "dto")]
use crate::dto::{Batch, BatchPoll, Submit};
use crate::mapping::{Mapping, data_extents};
use crate::qpl::{Error as QplError, ExecutionPath, HuffmanMode, JobPool};

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("Resident checkpoint I/O")]
    Io(#[from] io::Error),
    #[error("Resident checkpoint compression")]
    Compression(#[from] CompressionError),
    #[error("Resident checkpoint diff")]
    Diff(#[from] diff::Error),
    #[error("Resident checkpoint QPL")]
    Qpl(#[from] QplError),
    #[error("Resident checkpoint manifest")]
    Json(#[from] serde_json::Error),
    #[error("Resident checkpoints need an async QPL codec")]
    Codec,
}

/// Configuration fixed for the daemon's lifetime.
pub(crate) struct ResidentConfig {
    pub codec: Codec,
    pub chunk_size: usize,
    pub workers: usize,
    pub accel: AccelOptions,
    /// Write dirty-log diffs against a reference kept in this directory.
    pub reference_dir: Option<PathBuf>,
    pub diff: DiffOptions,
    /// Full checkpoints only: delete the previous one once the next exists.
    pub keep_only_last: bool,
    /// Deduplicate diff pages against the 4 KiB blocks of this (immutable)
    /// disk image.
    pub dedup_image: Option<PathBuf>,
}

/// A slot as received for one checkpoint.
pub(crate) struct SlotInput {
    pub slot: u32,
    pub gpa: u64,
    pub size: u64,
    pub file_offset: u64,
    pub file: File,
}

/// One bit per 4 KiB page.
struct Bits(Vec<u64>);

impl Bits {
    fn new(pages: usize) -> Self {
        Self(vec![0; pages.div_ceil(64)])
    }

    fn get(&self, page: usize) -> bool {
        self.0[page / 64] & (1 << (page % 64)) != 0
    }

    fn set_range(&mut self, start: usize, end: usize) {
        for page in start..end {
            self.0[page / 64] |= 1 << (page % 64);
        }
    }
}

struct Reference {
    _file: File,
    map: Mapping,
    has: Bits,
}

struct SlotState {
    slot: u32,
    gpa: u64,
    size: u64,
    file_offset: u64,
    id: (u64, u64),
    _file: File,
    src: Mapping,
    /// Pages with page-table entries in `src`.
    mapped: Bits,
    reference: Option<Reference>,
}

pub(crate) struct Resident {
    config: ResidentConfig,
    chunk_size_u32: u32,
    pool: JobPool,
    slots: Vec<SlotState>,
    last_dir: Option<PathBuf>,
    index: Option<Index>,
}

/// One slot's share of a diff checkpoint.
struct DiffItem {
    slot: u32,
    size: u64,
    /// Changed pages stored in the gather buffer.
    kept: Vec<usize>,
    gather: Buffer,
    dirty_pages: u64,
    changed_pages: u64,
    dedup: Vec<DedupRun>,
}

/// Runs of `pages` (sorted page offsets) that `need` selects, merged across
/// gaps whose pages all satisfy `bridge`.
fn bridged_runs(
    pages: &[usize],
    need: impl Fn(usize) -> bool,
    bridge: impl Fn(usize) -> bool,
) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &p in pages {
        if !need(p) {
            continue;
        }
        if let Some(last) = runs.last_mut()
            && (p <= last.1 || (last.1..p).step_by(PAGE).all(&bridge))
        {
            last.1 = last.1.max(p + PAGE);
            continue;
        }
        runs.push((p, p + PAGE));
    }
    runs
}

fn file_id(file: &File) -> io::Result<(u64, u64)> {
    let m = file.metadata()?;
    Ok((m.dev(), m.ino()))
}

impl Resident {
    pub(crate) fn new(config: ResidentConfig) -> Result<Self, Error> {
        let huffman: HuffmanMode = config.codec.async_huffman_mode().ok_or(Error::Codec)?;
        let chunk_size_u32 = validate_chunk_size(config.chunk_size)?;
        let pool = JobPool::new(ExecutionPath::Hardware, huffman, config.workers)?;
        info!(
            "resident: codec={} workers={} diff={}",
            config.codec,
            config.workers,
            if config.reference_dir.is_some() {
                config.diff.compare.to_string()
            } else {
                "off".to_owned()
            }
        );
        let index = match &config.dedup_image {
            Some(path) => {
                let use_dsa = config.diff.compare == DiffCompare::Dsa;
                let (index, stats) = Index::build(path, use_dsa)?;
                info!(
                    "dedup index: {path:?}: {} blocks, {} indexed, built in {:.1} ms ({})",
                    stats.blocks,
                    stats.indexed,
                    ms(stats.build),
                    if use_dsa { "DSA CRC" } else { "CPU CRC" }
                );
                Some(index)
            }
            None => None,
        };
        Ok(Self {
            config,
            chunk_size_u32,
            pool,
            slots: Vec::new(),
            last_dir: None,
            index,
        })
    }

    /// Forget the chain (after a failed checkpoint): the next one is full.
    pub(crate) fn reset(&mut self) {
        self.slots.clear();
        self.last_dir = None;
    }

    fn same_vm(&self, slots: &[SlotInput]) -> io::Result<bool> {
        if self.slots.len() != slots.len() || self.slots.is_empty() {
            return Ok(false);
        }
        for s in slots {
            let Some(state) = self.slots.iter().find(|st| st.slot == s.slot) else {
                return Ok(false);
            };
            if state.size != s.size
                || state.file_offset != s.file_offset
                || state.gpa != s.gpa
                || state.id != file_id(&s.file)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Write one checkpoint into `dir`: a diff when this VM's chain exists, a
    /// dirty log came with it and diffs are enabled; a full one otherwise.
    /// Returns a one-line summary.
    pub(crate) fn checkpoint(
        &mut self,
        slots: Vec<SlotInput>,
        dirty: Option<&[(u64, u64)]>,
        dir: &Path,
    ) -> Result<String, Error> {
        let started = Instant::now();
        let same = self.same_vm(&slots)?;
        let summary = match dirty {
            Some(dirty)
                if same && self.config.reference_dir.is_some() && self.last_dir.is_some() =>
            {
                drop(slots);
                self.diff(dirty, dir)?
            }
            _ => self.full(slots, same, dir)?,
        };
        if self.config.keep_only_last
            && let Some(previous) = &self.last_dir
            && previous != dir
        {
            let _ = fs::remove_dir_all(previous);
        }
        self.last_dir = Some(dir.to_path_buf());
        Ok(format!("{summary} total_ms={:.1}", ms(started.elapsed())))
    }

    fn full(&mut self, slots: Vec<SlotInput>, same: bool, dir: &Path) -> Result<String, Error> {
        if !same {
            self.slots.clear();
        }
        let (mut out_bytes, mut calls, mut zero, mut chunks) = (0_u64, 0_u64, 0_usize, 0_usize);
        let started = Instant::now();
        // 1. per slot, sequentially: state, data extents, page tables
        let mut work: Vec<SlotWork> = Vec::new();
        for input in slots {
            let t = Instant::now();
            if !same {
                let src = Mapping::map_file(&input.file, input.file_offset, input.size, false)?;
                self.slots.push(SlotState {
                    slot: input.slot,
                    gpa: input.gpa,
                    size: input.size,
                    file_offset: input.file_offset,
                    id: file_id(&input.file)?,
                    src,
                    mapped: Bits::new(input.size as usize / PAGE),
                    reference: None,
                    _file: input.file,
                });
            }
            let index = self
                .slots
                .iter()
                .position(|s| s.slot == input.slot)
                .expect("slot state exists");
            let state = &mut self.slots[index];
            let extents = data_extents(&state._file, state.file_offset, state.size)?;
            let prefault = if self.config.accel.prefault {
                calls += populate_unmapped(state, &extents)?;
                Some(t.elapsed())
            } else {
                None
            };
            work.push((index, extents, prefault));
        }
        let prepare = started.elapsed();

        // 2. every slot through one ready queue and the one job pool
        let t = Instant::now();
        let paths: Vec<(PathBuf, PathBuf)> = work
            .iter()
            .map(|(index, ..)| {
                let slot = self.slots[*index].slot;
                (
                    dir.join(format!("memory-{slot}.compressed")),
                    dir.join(format!("memory-{slot}.index.json")),
                )
            })
            .collect();
        let jobs: Vec<SlotJob<'_>> = work
            .iter()
            .zip(&paths)
            .map(|((index, extents, prefault), (data, manifest))| SlotJob {
                mapping: &self.slots[*index].src,
                extents,
                prefault: *prefault,
                source_size: self.slots[*index].size,
                data_path: data,
                manifest_path: manifest,
            })
            .collect();
        let stats = compress_slots(
            &jobs,
            &mut self.pool,
            self.config.codec,
            self.config.chunk_size,
            self.chunk_size_u32,
            self.config.accel,
        )?;
        drop(jobs);
        let compress = t.elapsed();
        let use_dsa = self.config.diff.compare == DiffCompare::Dsa;
        for (((index, ..), (_, manifest_path)), st) in work.iter().zip(&paths).zip(&stats) {
            out_bytes += st.output_bytes;
            chunks += st.chunks;
            let manifest: SlotManifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
            zero += manifest.chunks.iter().filter(|c| c.zero).count();
            if let Some(reference_dir) = &self.config.reference_dir {
                let nonzero: Vec<(usize, usize)> = manifest
                    .chunks
                    .iter()
                    .filter(|c| !c.zero)
                    .map(|c| {
                        (
                            c.uncompressed_offset as usize,
                            c.uncompressed_length as usize,
                        )
                    })
                    .collect();
                fs::create_dir_all(reference_dir)?;
                let reference =
                    init_reference(&self.slots[*index], reference_dir, &nonzero, use_dsa)?;
                self.slots[*index].reference = Some(reference);
            }
        }
        Ok(format!(
            "kind=full chunks={chunks} zero_chunks={zero} output_bytes={out_bytes} populate_calls={calls} prepare_ms={:.1} compress_ms={:.1}",
            ms(prepare),
            ms(compress)
        ))
    }

    fn diff(&mut self, dirty: &[(u64, u64)], dir: &Path) -> Result<String, Error> {
        let options = self.config.diff;
        let mut stats = DiffStats::default();
        let mut items: Vec<DiffItem> = Vec::new();
        let use_dsa = options.compare == DiffCompare::Dsa;
        for state in &mut self.slots {
            let t = Instant::now();
            let ranges = diff::slot_dirty_ranges(dirty, state.gpa, state.size);
            let pages: Vec<usize> = ranges
                .iter()
                .flat_map(|&(s, e)| (s..e).step_by(PAGE))
                .collect();
            stats.populate_calls += populate_pages(state, &pages)?;
            let reference = state
                .reference
                .as_mut()
                .expect("diff chains have a reference");
            let has_ref: Vec<bool> = pages.iter().map(|&p| reference.has.get(p / PAGE)).collect();
            stats.prepare += t.elapsed();

            let t = Instant::now();
            let changed = diff::changed_pages(
                &state.src,
                &reference.map,
                &pages,
                &has_ref,
                options,
                &mut stats,
            )?;
            stats.compare += t.elapsed();

            let t = Instant::now();
            if options.compare != DiffCompare::None {
                // reference pages about to be written must exist (a device
                // write to a hole takes a page-request fault per page)
                let has = &reference.has;
                let runs = bridged_runs(&changed, |p| !has.get(p / PAGE), |p| has.get(p / PAGE));
                for &(s, e) in &runs {
                    reference.map.populate_range(s, e - s, true)?;
                    reference.has.set_range(s / PAGE, e / PAGE);
                }
                stats.populate_calls += runs.len() as u64;
            }
            stats.gather += t.elapsed();
            // pages equal to a disk-image block are stored as references
            let (hits, kept) = match &self.index {
                Some(index) => {
                    dedup::dedup_pages(&state.src, index, &changed, use_dsa, &mut stats)?
                }
                None => (Vec::new(), changed.clone()),
            };
            let t = Instant::now();
            let gather = Buffer::new(kept.len() * PAGE)?;
            diff::gather_pages(
                &state.src,
                &reference.map,
                &gather,
                &kept,
                options,
                &mut stats,
            )?;
            if options.compare != DiffCompare::None {
                // deduplicated pages still belong in the reference
                for &(p, _, _) in &hits {
                    reference.map.write(p, state.src.slice(p, PAGE)?)?;
                }
            }
            stats.gather += t.elapsed();
            stats.dirty_pages += pages.len() as u64;
            stats.changed_pages += changed.len() as u64;
            items.push(DiffItem {
                slot: state.slot,
                size: state.size,
                kept,
                gather,
                dirty_pages: pages.len() as u64,
                changed_pages: changed.len() as u64,
                dedup: dedup::runs(&hits),
            });
        }

        let t = Instant::now();
        let compressed = compress_gathers(&mut self.pool, &items, dir, self.config.chunk_size)?;
        let dedup_image = self.index.as_ref().map(|i| i.image.clone());
        for (item, (chunks, bytes)) in items.iter().zip(compressed) {
            stats.output_bytes += bytes;
            let manifest = DiffManifest {
                version: DIFF_VERSION,
                codec: self.config.codec,
                chunk_size: self.chunk_size_u32,
                slot_size: item.size,
                runs: diff::runs_of(&item.kept),
                chunks,
                dirty_pages: item.dirty_pages,
                changed_pages: item.changed_pages,
                dedup: item.dedup.clone(),
                dedup_image: if item.dedup.is_empty() {
                    None
                } else {
                    dedup_image.clone()
                },
            };
            fs::write(
                diff::manifest_path(dir, item.slot),
                serde_json::to_vec(&manifest)?,
            )?;
        }
        let parent = self.last_dir.clone().expect("diffs have a parent");
        fs::write(
            diff::marker_path(dir),
            serde_json::to_vec(&DiffMarker { parent })?,
        )?;
        stats.compress = t.elapsed();
        Ok(format!(
            "kind=diff compare={} dirty_pages={} changed_pages={} dedup_pages={} output_bytes={} populate_calls={} prepare_ms={:.1} compare_ms={:.1} gather_ms={:.1} dedup_ms={:.1} compress_ms={:.1} dsa_ops={} dsa_cpu_redo={}",
            options.compare,
            stats.dirty_pages,
            stats.changed_pages,
            stats.dedup_pages,
            stats.output_bytes,
            stats.populate_calls,
            ms(stats.prepare),
            ms(stats.compare),
            ms(stats.gather),
            ms(stats.dedup),
            ms(stats.compress),
            stats.dsa_ops,
            stats.dsa_cpu_redo
        ))
    }
}

/// A slot ready for a full checkpoint: index, data extents, prefault time.
type SlotWork = (usize, Vec<(usize, usize)>, Option<Duration>);

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Page-table entries for the data extents of a slot that this daemon has
/// not mapped yet.
fn populate_unmapped(state: &mut SlotState, extents: &[(usize, usize)]) -> io::Result<u64> {
    if state.src.page_size() > PAGE {
        // hugetlbfs: no holes, so one call covers everything
        if let (Some(first), Some(last)) = (extents.first(), extents.last())
            && (first.0..last.1)
                .step_by(PAGE)
                .any(|p| !state.mapped.get(p / PAGE))
        {
            state.src.populate_range(first.0, last.1 - first.0, false)?;
            state.mapped.set_range(first.0 / PAGE, last.1 / PAGE);
            return Ok(1);
        }
        return Ok(0);
    }
    let mut calls = 0;
    for &(start, end) in extents {
        let pages: Vec<usize> = (start..end).step_by(PAGE).collect();
        calls += populate_pages(state, &pages)?;
    }
    Ok(calls)
}

/// Page-table entries for `pages` (sorted; all known to hold data): one call
/// over the span on hugetlbfs, else one per run of unmapped pages, bridged
/// across already-mapped pages.
fn populate_pages(state: &mut SlotState, pages: &[usize]) -> io::Result<u64> {
    let (Some(&first), Some(&last)) = (pages.first(), pages.last()) else {
        return Ok(0);
    };
    if state.src.page_size() > PAGE {
        state
            .src
            .populate_range(first, last + PAGE - first, false)?;
        state.mapped.set_range(first / PAGE, (last + PAGE) / PAGE);
        return Ok(1);
    }
    let mapped = &state.mapped;
    let runs = bridged_runs(pages, |p| !mapped.get(p / PAGE), |p| mapped.get(p / PAGE));
    for &(s, e) in &runs {
        state.src.populate_range(s, e - s, false)?;
        state.mapped.set_range(s / PAGE, e / PAGE);
    }
    Ok(runs.len() as u64)
}

/// Reference = the slot's non-zero chunks, in a sparse file mapped for the
/// daemon's lifetime.
fn init_reference(
    state: &SlotState,
    reference_dir: &Path,
    nonzero: &[(usize, usize)],
    use_dsa: bool,
) -> Result<Reference, Error> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(diff::reference_path(reference_dir, state.slot))?;
    file.set_len(state.size)?;
    let map = Mapping::map_file(&file, 0, state.size, true)?;
    let mut has = Bits::new(state.size as usize / PAGE);
    for &(offset, length) in nonzero {
        map.populate_range(offset, length, true)?;
        has.set_range(offset / PAGE, (offset + length).div_ceil(PAGE));
    }
    #[cfg(feature = "dto")]
    if use_dsa {
        let mut batch = Batch::new(1024)?;
        for group in nonzero.chunks(1024) {
            batch.reset();
            for &(offset, length) in group {
                // SAFETY: source stable while the VM is paused; the reference
                // mapping is owned by this daemon.
                unsafe {
                    batch.add_memmove(
                        map.mut_ptr(offset, length)?,
                        state.src.ptr(offset, length)?,
                        length,
                    )
                };
            }
            let submitted = batch.submit() == Submit::Submitted;
            if submitted {
                while batch.poll() == BatchPoll::Pending {
                    thread::yield_now();
                }
            }
            for (j, &(offset, length)) in group.iter().enumerate() {
                if !submitted || batch.status(j) != 1 {
                    map.write(offset, state.src.slice(offset, length)?)?;
                }
            }
        }
        return Ok(Reference {
            _file: file,
            map,
            has,
        });
    }
    let _ = use_dsa;
    for &(offset, length) in nonzero {
        map.write(offset, state.src.slice(offset, length)?)?;
    }
    Ok(Reference {
        _file: file,
        map,
        has,
    })
}

/// Compress every slot's gather buffer through the one shared job pool.
/// Returns per slot (chunk records, compressed bytes).
fn compress_gathers(
    pool: &mut JobPool,
    items: &[DiffItem],
    dir: &Path,
    chunk_size: usize,
) -> Result<Vec<(Vec<ChunkRecord>, u64)>, Error> {
    let mut outputs = Vec::with_capacity(items.len());
    for DiffItem { slot, .. } in items {
        outputs.push(
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(diff::data_path(dir, *slot))?,
        );
    }
    let mut work: Vec<(usize, usize, usize)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let total = item.kept.len() * PAGE;
        for offset in (0..total).step_by(chunk_size) {
            work.push((i, offset, (total - offset).min(chunk_size)));
        }
    }
    let mut results: Vec<(Vec<ChunkRecord>, u64)> = vec![(Vec::new(), 0); items.len()];
    let mut active: Vec<Option<(usize, usize, usize)>> = vec![None; pool.capacity()];
    let (mut next, mut live) = (0_usize, 0_usize);
    loop {
        for (index, entry) in active.iter_mut().enumerate() {
            if let Some((item, offset, length)) = *entry {
                let Some(size) = pool.poll(index)? else {
                    continue;
                };
                let (records, bytes) = &mut results[item];
                outputs[item].write_all_at(pool.output(index, size), *bytes)?;
                records.push(ChunkRecord {
                    uncompressed_offset: offset as u64,
                    uncompressed_length: length as u32,
                    compressed_offset: *bytes,
                    compressed_length: size as u32,
                    zero: false,
                    crc32c: None,
                });
                *bytes += size as u64;
                *entry = None;
                live -= 1;
            }
            if entry.is_none() && next < work.len() {
                let (item, offset, length) = work[next];
                let src = items[item].gather.page(offset / PAGE);
                // SAFETY: the gather buffers outlive the pool's use of them
                // and are not written while jobs run.
                unsafe { pool.submit_compress_from(index, src, length)? };
                *entry = Some((item, offset, length));
                next += 1;
                live += 1;
            }
        }
        if next == work.len() && live == 0 {
            break;
        }
        thread::yield_now();
    }
    for (records, _) in &mut results {
        records.sort_unstable_by_key(|r| r.uncompressed_offset);
    }
    for output in &outputs {
        output.sync_all()?;
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_bridge_over_mapped_pages_only() {
        let pages: Vec<usize> = [0, 1, 2, 5, 6, 9].iter().map(|p| p * PAGE).collect();
        // pages 3 and 4 are mapped, 7 and 8 are not
        let mapped = |p: usize| matches!(p / PAGE, 3 | 4);
        let runs = bridged_runs(&pages, |p| !mapped(p), mapped);
        assert_eq!(runs, vec![(0, 7 * PAGE), (9 * PAGE, 10 * PAGE)]);
    }

    #[test]
    fn bits_roundtrip() {
        let mut b = Bits::new(200);
        b.set_range(60, 70);
        assert!(!b.get(59) && b.get(60) && b.get(69) && !b.get(70));
    }
}
