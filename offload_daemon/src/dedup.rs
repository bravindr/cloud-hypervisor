// Copyright © 2026 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Disk-block deduplication for diff checkpoints.
//!
//! Pages an agent's guest reads from its root filesystem (compilers,
//! libraries, interpreters, data files) sit in the guest page cache as
//! byte-identical copies of 4 KiB blocks of the disk image. A diff checkpoint
//! does not need to compress or store them: it records which image block
//! each one equals and restore reads it back from the image.
//!
//! The index is built once from the *immutable* template image (never the
//! guest's writable copy, whose blocks change): a CRC32C per non-zero 4 KiB
//! block, sorted. At a checkpoint every changed page gets a CRC (one batched
//! pass of DSA CRC generation, or SSE4.2 on the CPU); pages whose CRC is in
//! the index are compared against the candidate image block (DSA COMPARE or
//! memcmp) so that a CRC collision can never make a snapshot wrong; matches
//! become dedup records carrying the page's CRC, which restore checks again
//! against the block it reads from the image.

use std::fs::File;
#[cfg(all(feature = "qpl", feature = "dto"))]
use std::io;
use std::os::unix::fs::FileExt;
#[cfg(feature = "qpl")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "dto")]
use std::thread;
#[cfg(feature = "qpl")]
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::compression::Destination;
use crate::crc::crc32c;
#[cfg(feature = "qpl")]
use crate::diff::DiffStats;
use crate::diff::{Error, PAGE};
#[cfg(feature = "dto")]
use crate::dto::{Batch, BatchPoll, Submit};
#[cfg(feature = "qpl")]
use crate::mapping::Mapping;

#[cfg(feature = "qpl")]
/// Candidate image blocks examined per page (CRC collisions, repeated blocks).
const MAX_CANDIDATES: usize = 4;

/// A run of guest pages equal to consecutive image blocks.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct DedupRun {
    /// Slot byte offset of the first page.
    pub offset: u64,
    /// Image byte offset of the first block.
    pub image_offset: u64,
    /// CRC32C (DSA convention) of each page, in order.
    pub crcs: Vec<u32>,
}

/// The image a checkpoint's dedup records refer to.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct DedupImage {
    pub path: PathBuf,
    pub size: u64,
}

#[cfg(feature = "qpl")]
pub(crate) struct Index {
    pub image: DedupImage,
    _file: File,
    map: Mapping,
    /// (crc, image byte offset), sorted by crc.
    entries: Vec<(u32, u64)>,
}

#[cfg(feature = "qpl")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IndexStats {
    pub blocks: u64,
    pub indexed: u64,
    pub build: Duration,
}

#[cfg(feature = "qpl")]
impl Index {
    /// CRC every non-zero 4 KiB block of `path` (DSA when `use_dsa`).
    pub(crate) fn build(path: &Path, use_dsa: bool) -> Result<(Self, IndexStats), Error> {
        let started = Instant::now();
        let file = File::open(path)?;
        let size = file.metadata()?.len() / PAGE as u64 * PAGE as u64;
        let map = Mapping::map_file(&file, 0, size, false)?;
        map.populate_range(0, size as usize, false)?;
        let blocks = size as usize / PAGE;
        let crcs = page_crcs(
            &map,
            &(0..blocks).map(|b| b * PAGE).collect::<Vec<_>>(),
            use_dsa,
        )?;
        // A zero block's CRC is 0 in this convention: zero pages are elided
        // long before dedup, so leave them (and the rare non-zero block whose
        // CRC is 0) out of the index.
        let mut entries: Vec<(u32, u64)> = crcs
            .iter()
            .enumerate()
            .filter(|(_, crc)| **crc != 0)
            .map(|(b, crc)| (*crc, (b * PAGE) as u64))
            .collect();
        entries.sort_unstable();
        let stats = IndexStats {
            blocks: blocks as u64,
            indexed: entries.len() as u64,
            build: started.elapsed(),
        };
        Ok((
            Self {
                image: DedupImage {
                    path: path.to_path_buf(),
                    size,
                },
                _file: file,
                map,
                entries,
            },
            stats,
        ))
    }

    fn candidates(&self, crc: u32) -> impl Iterator<Item = u64> + '_ {
        let start = self.entries.partition_point(|&(c, _)| c < crc);
        self.entries[start..]
            .iter()
            .take_while(move |&&(c, _)| c == crc)
            .take(MAX_CANDIDATES)
            .map(|&(_, offset)| offset)
    }

    #[cfg(feature = "dto")]
    fn block(&self, image_offset: u64) -> io::Result<*const u8> {
        self.map.ptr(image_offset as usize, PAGE)
    }
}

/// CRC32C of each page at `offsets` in `map`.
#[cfg(feature = "qpl")]
fn page_crcs(map: &Mapping, offsets: &[usize], use_dsa: bool) -> Result<Vec<u32>, Error> {
    #[cfg(feature = "dto")]
    if use_dsa {
        return page_crcs_dsa(map, offsets);
    }
    let _ = use_dsa;
    offsets
        .iter()
        .map(|&o| Ok(crc32c(map.slice(o, PAGE)?)))
        .collect()
}

#[cfg(all(feature = "qpl", feature = "dto"))]
fn page_crcs_dsa(map: &Mapping, offsets: &[usize]) -> Result<Vec<u32>, Error> {
    const BATCH: usize = 256;
    const DEPTH: usize = 8;
    let mut out = vec![0_u32; offsets.len()];
    let mut batches: Vec<Batch> = (0..DEPTH)
        .map(|_| Batch::new(BATCH))
        .collect::<io::Result<_>>()?;
    let mut members: Vec<(usize, usize)> = vec![(0, 0); DEPTH];
    let mut live = [false; DEPTH];
    let mut next = 0_usize;
    while next < offsets.len() || live.iter().any(|l| *l) {
        for k in 0..DEPTH {
            if live[k] {
                if batches[k].poll() == BatchPoll::Pending {
                    continue;
                }
                let (first, count) = members[k];
                for j in 0..count {
                    out[first + j] = if batches[k].status(j) == 1 {
                        batches[k].crc(j)
                    } else {
                        crc32c(map.slice(offsets[first + j], PAGE)?)
                    };
                }
                live[k] = false;
            }
            if next < offsets.len() {
                batches[k].reset();
                let first = next;
                while next < offsets.len() && next - first < BATCH {
                    // SAFETY: the mapping is read-only and outlives the batch.
                    if !unsafe { batches[k].add_crc(map.ptr(offsets[next], PAGE)?, PAGE) } {
                        break;
                    }
                    next += 1;
                }
                members[k] = (first, next - first);
                if batches[k].submit() == Submit::Submitted {
                    live[k] = true;
                } else {
                    for i in first..next {
                        out[i] = crc32c(map.slice(offsets[i], PAGE)?);
                    }
                }
            }
        }
        if live.iter().any(|l| *l) {
            thread::yield_now();
        }
    }
    Ok(out)
}

/// Pages of `changed` equal to an image block. Returns (page, image
/// offset, crc) for the deduplicated pages and the pages still to store.
/// Deduplicated pages as (page, image offset, crc), and the pages still to store.
#[cfg(feature = "qpl")]
pub(crate) type DedupSplit = (Vec<(usize, u64, u32)>, Vec<usize>);

#[cfg(feature = "qpl")]
pub(crate) fn dedup_pages(
    src: &Mapping,
    index: &Index,
    changed: &[usize],
    use_dsa: bool,
    stats: &mut DiffStats,
) -> Result<DedupSplit, Error> {
    let started = Instant::now();
    let crcs = page_crcs(src, changed, use_dsa)?;
    // candidates: (index into changed, image offset, crc), one round per
    // candidate rank so most pages are settled by their first candidate
    let mut pending: Vec<(usize, Vec<u64>)> = changed
        .iter()
        .enumerate()
        .filter(|(i, _)| crcs[*i] != 0)
        .filter_map(|(i, _)| {
            let c: Vec<u64> = index.candidates(crcs[i]).collect();
            (!c.is_empty()).then_some((i, c))
        })
        .collect();
    let mut matched: Vec<Option<u64>> = vec![None; changed.len()];
    for rank in 0..MAX_CANDIDATES {
        let round: Vec<(usize, u64)> = pending
            .iter()
            .filter_map(|(i, c)| c.get(rank).map(|o| (*i, *o)))
            .collect();
        if round.is_empty() {
            break;
        }
        let equal = compare_blocks(src, index, changed, &round, use_dsa)?;
        for (&(i, offset), eq) in round.iter().zip(equal) {
            if eq {
                matched[i] = Some(offset);
            }
        }
        pending.retain(|(i, _)| matched[*i].is_none());
    }
    let mut hits = Vec::new();
    let mut kept = Vec::new();
    for (i, &p) in changed.iter().enumerate() {
        match matched[i] {
            Some(offset) => hits.push((p, offset, crcs[i])),
            None => kept.push(p),
        }
    }
    stats.dedup_pages += hits.len() as u64;
    stats.dedup += started.elapsed();
    Ok((hits, kept))
}

/// For each (index into `changed`, image offset): is the page equal to the block?
#[cfg(feature = "qpl")]
fn compare_blocks(
    src: &Mapping,
    index: &Index,
    changed: &[usize],
    pairs: &[(usize, u64)],
    use_dsa: bool,
) -> Result<Vec<bool>, Error> {
    #[cfg(feature = "dto")]
    if use_dsa {
        const BATCH: usize = 256;
        let mut out = Vec::with_capacity(pairs.len());
        let mut batch = Batch::new(BATCH)?;
        for group in pairs.chunks(BATCH) {
            batch.reset();
            for &(i, offset) in group {
                // SAFETY: both pages stay mapped and unchanged (VM paused,
                // image read-only) until the batch completes.
                unsafe {
                    batch.add_compare(src.ptr(changed[i], PAGE)?, index.block(offset)?, PAGE)
                };
            }
            let submitted = batch.submit() == Submit::Submitted;
            if submitted {
                while batch.poll() == BatchPoll::Pending {
                    thread::yield_now();
                }
            }
            for (j, &(i, offset)) in group.iter().enumerate() {
                out.push(if submitted && batch.status(j) == 1 {
                    !batch.differs(j)
                } else {
                    src.slice(changed[i], PAGE)? == index.map.slice(offset as usize, PAGE)?
                });
            }
        }
        return Ok(out);
    }
    let _ = use_dsa;
    pairs
        .iter()
        .map(|&(i, offset)| {
            Ok(src.slice(changed[i], PAGE)? == index.map.slice(offset as usize, PAGE)?)
        })
        .collect()
}

/// Coalesce sorted hits into runs over consecutive pages and blocks.
#[cfg(feature = "qpl")]
pub(crate) fn runs(hits: &[(usize, u64, u32)]) -> Vec<DedupRun> {
    let mut out: Vec<DedupRun> = Vec::new();
    for &(page, offset, crc) in hits {
        if let Some(last) = out.last_mut() {
            let n = last.crcs.len() as u64;
            if last.offset + n * PAGE as u64 == page as u64
                && last.image_offset + n * PAGE as u64 == offset
            {
                last.crcs.push(crc);
                continue;
            }
        }
        out.push(DedupRun {
            offset: page as u64,
            image_offset: offset,
            crcs: vec![crc],
        });
    }
    out
}

/// Restore side: read each run from the image, check every page's CRC and
/// write it into the destination.
pub(crate) fn apply(
    image: &DedupImage,
    runs: &[DedupRun],
    destination: &Destination<'_>,
) -> Result<u64, Error> {
    if runs.is_empty() {
        return Ok(0);
    }
    let file = File::open(&image.path)?;
    if file.metadata()?.len() / PAGE as u64 * PAGE as u64 != image.size {
        return Err(Error::Invalid(format!(
            "dedup image {} is not the one the checkpoint was taken against",
            image.path.display()
        )));
    }
    let mut buffer = Vec::new();
    let mut pages = 0;
    for run in runs {
        buffer.resize(run.crcs.len() * PAGE, 0);
        file.read_exact_at(&mut buffer, run.image_offset)?;
        for (k, &crc) in run.crcs.iter().enumerate() {
            if crc32c(&buffer[k * PAGE..(k + 1) * PAGE]) != crc {
                return Err(Error::Invalid(format!(
                    "image block at {:#x} no longer matches the checkpoint",
                    run.image_offset + (k * PAGE) as u64
                )));
            }
        }
        destination.write(run.offset as usize, &buffer)?;
        pages += run.crcs.len() as u64;
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "qpl")]
    #[test]
    fn runs_coalesce_only_when_both_sides_are_consecutive() {
        let p = PAGE as u64;
        let hits = vec![
            (0, 10 * p, 1),
            (PAGE, 11 * p, 2),
            (2 * PAGE, 20 * p, 3),
            (4 * PAGE, 21 * p, 4),
        ];
        let r = runs(&hits);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].crcs, vec![1, 2]);
        assert_eq!((r[1].offset, r[1].image_offset), (2 * p, 20 * p));
        assert_eq!((r[2].offset, r[2].image_offset), (4 * p, 21 * p));
    }

    #[test]
    fn zero_page_crc_is_zero() {
        assert_eq!(crc32c(&[0_u8; PAGE]), 0);
    }
}
