// Copyright © 2026 The Cloud Hypervisor Authors
//
// SPDX-License-Identifier: Apache-2.0

#[cfg(feature = "qpl")]
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::mem;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fmt, io, thread};

use lz4_flex::block::{compress as lz4_compress, decompress as lz4_decompress};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zstd::bulk::{compress as zstd_compress, decompress as zstd_decompress};

use crate::classify::{AccelOptions, Error as ClassifyError};
#[cfg(feature = "qpl")]
use crate::classify::{Classifier, Verdict};
use crate::crc::{crc32c, is_zero};
#[cfg(feature = "dto")]
use crate::dto::{Op as DtoOp, Poll as DtoPoll, Stats as DtoStats, Submit as DtoSubmit};
use crate::mapping::Mapping;
#[cfg(feature = "qpl")]
use crate::mapping::data_extents;
#[cfg(feature = "qpl")]
use crate::qpl::{
    Error as QplError, ExecutionPath, HuffmanMode, Job as QplJob, JobPool as QplJobPool,
};

pub(crate) const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Codec {
    Lz4,
    Zstd,
    QplHardware,
    QplHardwareStatic,
    QplHardwareDynamic,
    QplHardwareStaticAsync,
    QplHardwareDynamicAsync,
    QplAuto,
}

impl fmt::Display for Codec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
            Self::QplHardware => "qpl-hardware",
            Self::QplHardwareStatic => "qpl-hardware-static",
            Self::QplHardwareDynamic => "qpl-hardware-dynamic",
            Self::QplHardwareStaticAsync => "qpl-hardware-static-async",
            Self::QplHardwareDynamicAsync => "qpl-hardware-dynamic-async",
            Self::QplAuto => "qpl-auto",
        };
        formatter.write_str(name)
    }
}

impl FromStr for Codec {
    type Err = ParseCodecError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "lz4" => Ok(Self::Lz4),
            "zstd" => Ok(Self::Zstd),
            "qpl-hardware" => Ok(Self::QplHardware),
            "qpl-hardware-static" => Ok(Self::QplHardwareStatic),
            "qpl-hardware-dynamic" => Ok(Self::QplHardwareDynamic),
            "qpl-hardware-static-async" => Ok(Self::QplHardwareStaticAsync),
            "qpl-hardware-dynamic-async" => Ok(Self::QplHardwareDynamicAsync),
            "qpl-auto" => Ok(Self::QplAuto),
            _ => Err(ParseCodecError(value.to_owned())),
        }
    }
}

#[cfg(feature = "qpl")]
impl Codec {
    pub(crate) fn async_huffman_mode(self) -> Option<HuffmanMode> {
        match self {
            Self::QplHardwareStaticAsync => Some(HuffmanMode::Static),
            Self::QplHardwareDynamicAsync => Some(HuffmanMode::Dynamic),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
#[error("Unknown compression codec {0:?}")]
pub(crate) struct ParseCodecError(String);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ChunkRecord {
    pub uncompressed_offset: u64,
    pub uncompressed_length: u32,
    pub compressed_offset: u64,
    pub compressed_length: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub zero: bool,
    /// CRC32C (seed 0, no final xor: the DSA CRC generation convention) of the
    /// uncompressed chunk, when the snapshot was taken with `--crc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crc32c: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SlotManifest {
    pub version: u32,
    pub codec: Codec,
    pub chunk_size: u32,
    pub uncompressed_size: u64,
    pub chunks: Vec<ChunkRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompressionStats {
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub chunks: usize,
    pub elapsed: Duration,
}

impl CompressionStats {
    pub(crate) fn ratio(self) -> f64 {
        if self.input_bytes == 0 {
            return 1.0;
        }
        self.output_bytes as f64 / self.input_bytes as f64
    }

    pub(crate) fn throughput_gib_per_second(self) -> f64 {
        if self.elapsed.is_zero() {
            return 0.0;
        }
        self.input_bytes as f64 / (1_u64 << 30) as f64 / self.elapsed.as_secs_f64()
    }
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[cfg(not(feature = "qpl"))]
    #[error("QPL support is not available in this build")]
    QplUnavailable,
    #[cfg(feature = "qpl")]
    #[error("QPL operation failed")]
    Qpl(#[from] QplError),
    #[error("Invalid chunk size {0}")]
    InvalidChunkSize(usize),
    #[error("Chunk is too large")]
    ChunkTooLarge,
    #[error("Decompressed chunk has length {actual}, expected {expected}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("Compression failed")]
    Compress(#[source] io::Error),
    #[error("Decompression failed")]
    Decompress(#[source] io::Error),
    #[error("Reading compressed snapshot")]
    Read(#[source] io::Error),
    #[error("Writing compressed snapshot")]
    Write(#[source] io::Error),
    #[error("Reading compression manifest")]
    ReadManifest(#[source] serde_json::Error),
    #[error("Writing compression manifest")]
    WriteManifest(#[source] serde_json::Error),
    #[error("Unsupported compression format version {0}")]
    UnsupportedVersion(u32),
    #[error("Invalid compression manifest: {0}")]
    InvalidManifest(String),
    #[error("Compression worker panicked")]
    WorkerPanic,
    #[error(
        "Chunk at offset {offset:#x} failed CRC32C verification (manifest {expected:#010x}, data {actual:#010x})"
    )]
    CrcMismatch {
        offset: u64,
        expected: u32,
        actual: u32,
    },
    #[error("Chunk classification failed")]
    Classify(#[from] ClassifyError),
}

pub(crate) struct CodecWorker {
    codec: Codec,
    zstd_level: i32,
    output: Vec<u8>,
    #[cfg(feature = "qpl")]
    qpl_job: Option<QplJob>,
}

impl CodecWorker {
    pub(crate) fn new(codec: Codec, zstd_level: i32) -> Result<Self, Error> {
        #[cfg(feature = "qpl")]
        let qpl_job = match codec {
            Codec::QplHardware | Codec::QplHardwareDynamic | Codec::QplHardwareDynamicAsync => {
                Some(QplJob::new_with_mode(
                    ExecutionPath::Hardware,
                    HuffmanMode::Dynamic,
                )?)
            }
            Codec::QplHardwareStatic | Codec::QplHardwareStaticAsync => Some(
                QplJob::new_with_mode(ExecutionPath::Hardware, HuffmanMode::Static)?,
            ),
            Codec::QplAuto => Some(QplJob::new(ExecutionPath::Auto)?),
            Codec::Lz4 | Codec::Zstd => None,
        };
        #[cfg(not(feature = "qpl"))]
        if matches!(
            codec,
            Codec::QplHardware
                | Codec::QplHardwareStatic
                | Codec::QplHardwareDynamic
                | Codec::QplHardwareStaticAsync
                | Codec::QplHardwareDynamicAsync
                | Codec::QplAuto
        ) {
            return Err(Error::QplUnavailable);
        }
        Ok(Self {
            codec,
            zstd_level,
            output: Vec::new(),
            #[cfg(feature = "qpl")]
            qpl_job,
        })
    }

    fn compress(&mut self, input: &[u8]) -> Result<&[u8], Error> {
        match self.codec {
            Codec::Lz4 => self.output = lz4_compress(input),
            Codec::Zstd => {
                self.output = zstd_compress(input, self.zstd_level).map_err(Error::Compress)?;
            }
            Codec::QplHardware
            | Codec::QplHardwareStatic
            | Codec::QplHardwareDynamic
            | Codec::QplHardwareStaticAsync
            | Codec::QplHardwareDynamicAsync
            | Codec::QplAuto => {
                #[cfg(feature = "qpl")]
                self.qpl_job
                    .as_mut()
                    .unwrap()
                    .compress_into(input, &mut self.output)
                    .map_err(Error::Qpl)?;
                #[cfg(not(feature = "qpl"))]
                unreachable!();
            }
        }
        Ok(&self.output)
    }

    pub(crate) fn decompress(
        &mut self,
        input: &[u8],
        expected_length: usize,
    ) -> Result<&[u8], Error> {
        match self.codec {
            Codec::Lz4 => {
                self.output = lz4_decompress(input, expected_length)
                    .map_err(|error| Error::Decompress(io::Error::other(error.to_string())))?;
            }
            Codec::Zstd => {
                self.output = zstd_decompress(input, expected_length).map_err(Error::Decompress)?;
            }
            Codec::QplHardware
            | Codec::QplHardwareStatic
            | Codec::QplHardwareDynamic
            | Codec::QplHardwareStaticAsync
            | Codec::QplHardwareDynamicAsync
            | Codec::QplAuto => {
                #[cfg(feature = "qpl")]
                self.qpl_job
                    .as_mut()
                    .unwrap()
                    .decompress_into(input, expected_length, &mut self.output)
                    .map_err(Error::Qpl)?;
                #[cfg(not(feature = "qpl"))]
                unreachable!();
            }
        }
        if self.output.len() != expected_length {
            return Err(Error::LengthMismatch {
                expected: expected_length,
                actual: self.output.len(),
            });
        }
        Ok(&self.output)
    }
}

#[cfg(test)]
fn compress_chunk(codec: Codec, input: &[u8], zstd_level: i32) -> Result<Vec<u8>, Error> {
    Ok(CodecWorker::new(codec, zstd_level)?
        .compress(input)?
        .to_vec())
}

#[cfg(test)]
fn decompress_chunk(codec: Codec, input: &[u8], expected_length: usize) -> Result<Vec<u8>, Error> {
    Ok(CodecWorker::new(codec, 1)?
        .decompress(input, expected_length)?
        .to_vec())
}

pub(crate) fn validate_chunk_size(chunk_size: usize) -> Result<u32, Error> {
    if chunk_size == 0 {
        return Err(Error::InvalidChunkSize(chunk_size));
    }
    u32::try_from(chunk_size).map_err(|_| Error::ChunkTooLarge)
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn compress_file(
    source: &File,
    source_offset: u64,
    source_size: u64,
    data_path: &Path,
    manifest_path: &Path,
    codec: Codec,
    chunk_size: usize,
    workers: usize,
    zstd_level: i32,
    accel: AccelOptions,
) -> Result<CompressionStats, Error> {
    let chunk_size_u32 = validate_chunk_size(chunk_size)?;
    #[cfg(feature = "qpl")]
    if let Some(huffman_mode) = codec.async_huffman_mode() {
        return compress_file_qpl_async(
            source,
            source_offset,
            source_size,
            data_path,
            manifest_path,
            codec,
            huffman_mode,
            chunk_size,
            chunk_size_u32,
            workers,
            accel,
        );
    }
    let started = Instant::now();
    let output = Arc::new(
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(data_path)
            .map_err(Error::Write)?,
    );
    let chunk_count = source_size.div_ceil(chunk_size as u64) as usize;
    let next_chunk = AtomicUsize::new(0);
    let next_output_offset = AtomicU64::new(0);
    let records = Mutex::new(Vec::with_capacity(chunk_count));
    let first_error = Mutex::new(None);

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers.max(1));
        for _ in 0..workers.max(1) {
            let output = Arc::clone(&output);
            let records = &records;
            let first_error = &first_error;
            let next_chunk = &next_chunk;
            let next_output_offset = &next_output_offset;
            handles.push(scope.spawn(move || {
                let mut worker = match CodecWorker::new(codec, zstd_level) {
                    Ok(worker) => worker,
                    Err(error) => {
                        let mut stored_error = first_error.lock().unwrap();
                        if stored_error.is_none() {
                            *stored_error = Some(error);
                        }
                        return;
                    }
                };
                let mut input = Vec::with_capacity(chunk_size);
                loop {
                    let chunk_index = next_chunk.fetch_add(1, Ordering::Relaxed);
                    if chunk_index >= chunk_count || first_error.lock().unwrap().is_some() {
                        return;
                    }
                    let uncompressed_offset = chunk_index as u64 * chunk_size as u64;
                    let uncompressed_length =
                        (source_size - uncompressed_offset).min(chunk_size as u64) as usize;
                    let result = (|| {
                        input.resize(uncompressed_length, 0);
                        source
                            .read_exact_at(&mut input, source_offset + uncompressed_offset)
                            .map_err(Error::Read)?;
                        if is_zero(&input) {
                            records.lock().unwrap().push(ChunkRecord {
                                uncompressed_offset,
                                uncompressed_length: uncompressed_length as u32,
                                compressed_offset: 0,
                                compressed_length: 0,
                                zero: true,
                                crc32c: None,
                            });
                            return Ok(());
                        }
                        let crc = accel.crc.then(|| crc32c(&input));
                        let compressed = worker.compress(&input)?;
                        let compressed_length =
                            u32::try_from(compressed.len()).map_err(|_| Error::ChunkTooLarge)?;
                        let compressed_offset = next_output_offset
                            .fetch_add(compressed.len() as u64, Ordering::Relaxed);
                        output
                            .write_all_at(compressed, compressed_offset)
                            .map_err(Error::Write)?;
                        records.lock().unwrap().push(ChunkRecord {
                            uncompressed_offset,
                            uncompressed_length: uncompressed_length as u32,
                            compressed_offset,
                            compressed_length,
                            zero: false,
                            crc32c: crc,
                        });
                        Ok(())
                    })();
                    if let Err(error) = result {
                        let mut stored_error = first_error.lock().unwrap();
                        if stored_error.is_none() {
                            *stored_error = Some(error);
                        }
                        return;
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().map_err(|_| Error::WorkerPanic)?;
        }
        Ok::<(), Error>(())
    })?;
    if let Some(error) = first_error.into_inner().unwrap() {
        return Err(error);
    }

    output.sync_all().map_err(Error::Write)?;
    let output_bytes = next_output_offset.load(Ordering::Relaxed);
    let mut chunks = records.into_inner().unwrap();
    chunks.sort_unstable_by_key(|record| record.uncompressed_offset);
    let manifest = SlotManifest {
        version: FORMAT_VERSION,
        codec,
        chunk_size: chunk_size_u32,
        uncompressed_size: source_size,
        chunks,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(Error::WriteManifest)?;
    fs::write(manifest_path, manifest_bytes).map_err(Error::Write)?;

    Ok(CompressionStats {
        input_bytes: source_size,
        output_bytes,
        chunks: chunk_count,
        elapsed: started.elapsed(),
    })
}

#[cfg(feature = "qpl")]
#[expect(clippy::too_many_arguments)]
fn compress_file_qpl_async(
    source: &File,
    source_offset: u64,
    source_size: u64,
    data_path: &Path,
    manifest_path: &Path,
    codec: Codec,
    huffman_mode: HuffmanMode,
    chunk_size: usize,
    chunk_size_u32: u32,
    workers: usize,
    accel: AccelOptions,
) -> Result<CompressionStats, Error> {
    let mapping =
        Mapping::map_file(source, source_offset, source_size, false).map_err(Error::Read)?;
    let extents = data_extents(source, source_offset, source_size).map_err(Error::Read)?;
    let prefault = if accel.prefault {
        let started = Instant::now();
        for &(start, end) in &extents {
            mapping
                .populate_range(start, end - start, false)
                .map_err(Error::Read)?;
        }
        Some(started.elapsed())
    } else {
        None
    };

    let mut pool = QplJobPool::new(ExecutionPath::Hardware, huffman_mode, workers)?;
    compress_mapped(
        &mapping,
        &extents,
        prefault,
        &mut pool,
        source_size,
        data_path,
        manifest_path,
        codec,
        chunk_size,
        chunk_size_u32,
        accel,
    )
}

/// The full-checkpoint pipeline on a slot that is already mapped, with its
/// data extents known (and populated, if `prefault` says so), using the
/// caller's IAA job pool: lets a resident daemon keep both across slots and
/// checkpoints.
#[cfg(feature = "qpl")]
#[expect(clippy::too_many_arguments)]
pub(crate) fn compress_mapped(
    mapping: &Mapping,
    extents: &[(usize, usize)],
    prefault: Option<Duration>,
    pool: &mut QplJobPool,
    source_size: u64,
    data_path: &Path,
    manifest_path: &Path,
    codec: Codec,
    chunk_size: usize,
    chunk_size_u32: u32,
    accel: AccelOptions,
) -> Result<CompressionStats, Error> {
    let started = Instant::now();
    let output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(data_path)
        .map_err(Error::Write)?;
    let chunk_count = source_size.div_ceil(chunk_size as u64) as usize;
    let mut chunk_has_data = vec![false; chunk_count];
    for &(start, end) in extents {
        chunk_has_data[start / chunk_size..end.div_ceil(chunk_size).min(chunk_count)].fill(true);
    }
    let hole_chunks = chunk_has_data.iter().filter(|has| !**has).count();
    let data_bytes: usize = extents.iter().map(|(start, end)| end - start).sum();
    let mut classifier = Classifier::new(accel, chunk_size)?;
    let mut records = Vec::with_capacity(chunk_count);
    let mut output_offset = 0_u64;

    // Stage one (classify) runs ahead of stage two (compress) by up to the
    // classifier's depth; verdicts wait in `ready` for a free IAA slot.
    let mut ready: VecDeque<Verdict> = VecDeque::new();
    let mut active: Vec<Option<Verdict>> = (0..pool.capacity()).map(|_| None).collect();
    let mut active_count = 0_usize;
    let mut next_chunk = 0_usize;
    // Bound how far classification may run ahead of a stalled IAA pool so the
    // CPU classifier does not scan the whole slot before compressing starts.
    let ready_limit = pool.capacity().max(accel.dsa_depth) * 2;

    loop {
        let mut made_progress = false;

        while next_chunk < chunk_count && ready.len() < ready_limit {
            let offset = next_chunk * chunk_size;
            let length = (source_size as usize - offset).min(chunk_size);
            if !chunk_has_data[next_chunk] {
                ready.push_back(Verdict {
                    offset,
                    length,
                    zero: true,
                    crc32c: None,
                });
                next_chunk += 1;
                made_progress = true;
                continue;
            }
            if !classifier.push(mapping, offset, length)? {
                break;
            }
            next_chunk += 1;
            made_progress = true;
        }

        let before = ready.len();
        classifier.drain(mapping, &mut ready)?;
        made_progress |= ready.len() != before;

        for (slot, entry) in active.iter_mut().enumerate() {
            if let Some(verdict) = entry.as_ref() {
                let Some(output_size) = pool.poll(slot)? else {
                    continue;
                };
                let compressed_length =
                    u32::try_from(output_size).map_err(|_| Error::ChunkTooLarge)?;
                output
                    .write_all_at(pool.output(slot, output_size), output_offset)
                    .map_err(Error::Write)?;
                records.push(ChunkRecord {
                    uncompressed_offset: verdict.offset as u64,
                    uncompressed_length: verdict.length as u32,
                    compressed_offset: output_offset,
                    compressed_length,
                    zero: false,
                    crc32c: verdict.crc32c,
                });
                output_offset += output_size as u64;
                *entry = None;
                active_count -= 1;
                made_progress = true;
            }
            if entry.is_none() {
                while let Some(verdict) = ready.pop_front() {
                    if verdict.zero {
                        records.push(ChunkRecord {
                            uncompressed_offset: verdict.offset as u64,
                            uncompressed_length: verdict.length as u32,
                            compressed_offset: 0,
                            compressed_length: 0,
                            zero: true,
                            crc32c: None,
                        });
                        made_progress = true;
                        continue;
                    }
                    let input = mapping
                        .ptr(verdict.offset, verdict.length)
                        .map_err(Error::Read)?;
                    // SAFETY: the mapping outlives the pool and the VM is
                    // paused for the whole snapshot, so the input is stable
                    // until the job completes.
                    unsafe { pool.submit_compress_from(slot, input, verdict.length)? };
                    *entry = Some(verdict);
                    active_count += 1;
                    made_progress = true;
                    break;
                }
            }
        }

        if next_chunk == chunk_count
            && classifier.is_idle()
            && ready.is_empty()
            && active_count == 0
        {
            break;
        }
        if !made_progress {
            thread::yield_now();
        }
    }

    output.sync_all().map_err(Error::Write)?;
    records.sort_unstable_by_key(|record| record.uncompressed_offset);
    let zero_chunks = records.iter().filter(|record| record.zero).count();
    let manifest = SlotManifest {
        version: FORMAT_VERSION,
        codec,
        chunk_size: chunk_size_u32,
        uncompressed_size: source_size,
        chunks: records,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(Error::WriteManifest)?;
    fs::write(manifest_path, manifest_bytes).map_err(Error::Write)?;

    let report = classifier.report();
    log::info!(
        "classify={} prefault={} zero_chunks={zero_chunks}/{chunk_count} hole_chunks={hole_chunks} data_mib={} dsa submitted={} fallback={} failed={} cpu_scans={} crc={}",
        accel.classify,
        prefault.map_or_else(
            || "off".to_owned(),
            |elapsed| format!("{:.1}ms", elapsed.as_secs_f64() * 1e3)
        ),
        data_bytes >> 20,
        report.dsa_submitted,
        report.dsa_fallback,
        report.dsa_failed,
        report.cpu_scans,
        accel.crc,
    );

    Ok(CompressionStats {
        input_bytes: source_size,
        output_bytes: output_offset,
        chunks: chunk_count,
        elapsed: started.elapsed(),
    })
}

/// Where decompressed chunks go: pwrite into the file, or a memcpy into a
/// mapping when the file is hugetlbfs-backed (no write(2) there).
pub(crate) enum Destination<'a> {
    File { file: &'a File, offset: u64 },
    Mapped(Mapping),
}

const HUGETLBFS_MAGIC: i64 = 0x958458f6;

impl<'a> Destination<'a> {
    pub(crate) fn open(file: &'a File, offset: u64, length: u64) -> io::Result<Self> {
        // SAFETY: zeroed statfs buffer filled by the kernel.
        let mut stat: libc::statfs = unsafe { mem::zeroed() };
        // SAFETY: valid fd and out pointer.
        let hugetlb = unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } == 0
            && stat.f_type == HUGETLBFS_MAGIC;
        if hugetlb {
            Ok(Self::Mapped(Mapping::map_file(file, offset, length, true)?))
        } else {
            Ok(Self::File { file, offset })
        }
    }

    pub(crate) fn write(&self, chunk_offset: usize, data: &[u8]) -> io::Result<()> {
        match self {
            Self::File { file, offset } => file.write_all_at(data, offset + chunk_offset as u64),
            Self::Mapped(mapping) => mapping.write(chunk_offset, data),
        }
    }
}

pub(crate) fn decompress_file(
    data_path: &Path,
    manifest_path: &Path,
    destination: &File,
    destination_offset: u64,
    expected_size: u64,
    workers: usize,
    verify_crc: bool,
) -> Result<CompressionStats, Error> {
    let started = Instant::now();
    let manifest_bytes = fs::read(manifest_path).map_err(Error::Read)?;
    let manifest: SlotManifest =
        serde_json::from_slice(&manifest_bytes).map_err(Error::ReadManifest)?;
    if manifest.version != FORMAT_VERSION {
        return Err(Error::UnsupportedVersion(manifest.version));
    }
    if manifest.uncompressed_size != expected_size {
        return Err(Error::LengthMismatch {
            expected: expected_size as usize,
            actual: manifest.uncompressed_size as usize,
        });
    }
    let input = Arc::new(File::open(data_path).map_err(Error::Read)?);
    let compressed_size = input.metadata().map_err(Error::Read)?.len();
    let mut expected_offset = 0_u64;
    for record in &manifest.chunks {
        if record.uncompressed_offset != expected_offset {
            return Err(Error::InvalidManifest(format!(
                "chunk starts at {}, expected {expected_offset}",
                record.uncompressed_offset
            )));
        }
        expected_offset = expected_offset
            .checked_add(record.uncompressed_length as u64)
            .ok_or_else(|| Error::InvalidManifest("uncompressed range overflow".to_owned()))?;
        let compressed_end = record
            .compressed_offset
            .checked_add(record.compressed_length as u64)
            .ok_or_else(|| Error::InvalidManifest("compressed range overflow".to_owned()))?;
        if compressed_end > compressed_size {
            return Err(Error::InvalidManifest(format!(
                "compressed range ends at {compressed_end}, file length is {compressed_size}"
            )));
        }
        if record.zero && record.compressed_length != 0 {
            return Err(Error::InvalidManifest(
                "zero chunk has compressed data".to_owned(),
            ));
        }
        if !record.zero && record.uncompressed_length != 0 && record.compressed_length == 0 {
            return Err(Error::InvalidManifest(
                "non-zero chunk has no compressed data".to_owned(),
            ));
        }
    }
    if expected_offset != expected_size {
        return Err(Error::InvalidManifest(format!(
            "chunks cover {expected_offset} bytes, expected {expected_size}"
        )));
    }
    let codec = manifest.codec;
    let records = Arc::new(manifest.chunks);
    if expected_size == 0 {
        return Ok(CompressionStats {
            input_bytes: 0,
            output_bytes: compressed_size,
            chunks: records.len(),
            elapsed: started.elapsed(),
        });
    }
    // Hugetlb memfds have no write(2) path, so decompressed chunks are copied
    // into a mapping of the destination there. On 4 KiB-backed memfds pwrite
    // stays: writing through a fresh mapping takes one page fault per page,
    // which measured 2x slower than letting the kernel allocate per write.
    let destination =
        Destination::open(destination, destination_offset, expected_size).map_err(Error::Write)?;
    #[cfg(feature = "qpl")]
    if let Some(huffman_mode) = codec.async_huffman_mode() {
        return decompress_file_qpl_async(
            &input,
            &records,
            &destination,
            expected_size,
            workers,
            huffman_mode,
            verify_crc,
            started,
        );
    }
    let next_chunk = AtomicUsize::new(0);
    let first_error = Mutex::new(None);

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers.max(1));
        for _ in 0..workers.max(1) {
            let input = Arc::clone(&input);
            let records = Arc::clone(&records);
            let first_error = &first_error;
            let next_chunk = &next_chunk;
            let destination = &destination;
            handles.push(scope.spawn(move || {
                let mut worker = match CodecWorker::new(codec, 1) {
                    Ok(worker) => worker,
                    Err(error) => {
                        let mut stored_error = first_error.lock().unwrap();
                        if stored_error.is_none() {
                            *stored_error = Some(error);
                        }
                        return;
                    }
                };
                let mut compressed = Vec::new();
                loop {
                    let chunk_index = next_chunk.fetch_add(1, Ordering::Relaxed);
                    let Some(record) = records.get(chunk_index) else {
                        return;
                    };
                    if first_error.lock().unwrap().is_some() {
                        return;
                    }
                    let result = (|| {
                        if record.zero {
                            return Ok(());
                        }
                        compressed.resize(record.compressed_length as usize, 0);
                        input
                            .read_exact_at(&mut compressed, record.compressed_offset)
                            .map_err(Error::Read)?;
                        let output =
                            worker.decompress(&compressed, record.uncompressed_length as usize)?;
                        if let Some(expected) = record.crc32c.filter(|_| verify_crc) {
                            let actual = crc32c(output);
                            if actual != expected {
                                return Err(Error::CrcMismatch {
                                    offset: record.uncompressed_offset,
                                    expected,
                                    actual,
                                });
                            }
                        }
                        destination
                            .write(record.uncompressed_offset as usize, output)
                            .map_err(Error::Write)
                    })();
                    if let Err(error) = result {
                        let mut stored_error = first_error.lock().unwrap();
                        if stored_error.is_none() {
                            *stored_error = Some(error);
                        }
                        return;
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().map_err(|_| Error::WorkerPanic)?;
        }
        Ok::<(), Error>(())
    })?;
    if let Some(error) = first_error.into_inner().unwrap() {
        return Err(error);
    }

    Ok(CompressionStats {
        input_bytes: expected_size,
        output_bytes: input.metadata().map_err(Error::Read)?.len(),
        chunks: records.len(),
        elapsed: started.elapsed(),
    })
}

#[cfg(feature = "qpl")]
#[expect(clippy::too_many_arguments)]
fn decompress_file_qpl_async(
    input: &File,
    records: &[ChunkRecord],
    destination: &Destination<'_>,
    expected_size: u64,
    workers: usize,
    huffman_mode: HuffmanMode,
    verify_crc: bool,
    started: Instant,
) -> Result<CompressionStats, Error> {
    let mut pool = QplJobPool::new(ExecutionPath::Hardware, huffman_mode, workers)?;
    let mut verifier = ChunkVerifier::new(verify_crc, pool.capacity());
    let mut active = vec![None; pool.capacity()];
    let mut next_chunk = 0_usize;
    let mut active_count = 0_usize;
    for (slot, active_record) in active.iter_mut().enumerate() {
        *active_record =
            submit_next_decompression_chunk(&mut pool, slot, input, records, &mut next_chunk)?;
        active_count += usize::from(active_record.is_some());
    }

    while active_count != 0 {
        let mut made_progress = false;
        for (slot, active_entry) in active.iter_mut().enumerate() {
            let Some(record_index) = *active_entry else {
                continue;
            };
            let record = &records[record_index];
            let output_size = match verifier.state(slot) {
                // Still decompressing.
                None => {
                    let Some(output_size) = pool.poll(slot)? else {
                        continue;
                    };
                    if output_size != record.uncompressed_length as usize {
                        return Err(Error::LengthMismatch {
                            expected: record.uncompressed_length as usize,
                            actual: output_size,
                        });
                    }
                    if let Some(expected) = record.crc32c.filter(|_| verify_crc) {
                        verifier.start(slot, pool.output(slot, output_size), expected);
                        made_progress = true;
                        continue;
                    }
                    output_size
                }
                // CRC in flight on the device.
                Some(Verify::Pending) => {
                    if !verifier.poll(slot, pool.output(slot, record.uncompressed_length as usize))
                    {
                        continue;
                    }
                    made_progress = true;
                    continue;
                }
                Some(Verify::Checked { actual, expected }) => {
                    verifier.clear(slot);
                    if actual != expected {
                        return Err(Error::CrcMismatch {
                            offset: record.uncompressed_offset,
                            expected,
                            actual,
                        });
                    }
                    record.uncompressed_length as usize
                }
            };
            destination
                .write(
                    record.uncompressed_offset as usize,
                    pool.output(slot, output_size),
                )
                .map_err(Error::Write)?;
            active_count -= 1;
            made_progress = true;

            *active_entry =
                submit_next_decompression_chunk(&mut pool, slot, input, records, &mut next_chunk)?;
            if active_entry.is_some() {
                active_count += 1;
            }
        }
        if !made_progress {
            thread::yield_now();
        }
    }
    if verify_crc {
        let (submitted, fallback, failed) = verifier.report();
        log::info!(
            "crc verify: {} chunks checked (dsa submitted={submitted} fallback={fallback} failed={failed})",
            verifier.checked()
        );
    }

    Ok(CompressionStats {
        input_bytes: expected_size,
        output_bytes: input.metadata().map_err(Error::Read)?.len(),
        chunks: records.len(),
        elapsed: started.elapsed(),
    })
}

#[cfg(feature = "qpl")]
fn submit_next_decompression_chunk(
    pool: &mut QplJobPool,
    slot: usize,
    input: &File,
    records: &[ChunkRecord],
    next_chunk: &mut usize,
) -> Result<Option<usize>, Error> {
    while let Some(record) = records.get(*next_chunk) {
        let record_index = *next_chunk;
        *next_chunk += 1;
        if record.zero {
            continue;
        }
        input
            .read_exact_at(
                pool.input_mut(slot, record.compressed_length as usize),
                record.compressed_offset,
            )
            .map_err(Error::Read)?;
        pool.submit_decompress(slot, record.uncompressed_length as usize)?;
        return Ok(Some(record_index));
    }
    Ok(None)
}

/// Per-slot CRC verification state for the async restore path: the CRC of a
/// decompressed chunk is generated on DSA when available (software
/// otherwise) and compared with the manifest before the chunk is written.
#[cfg(feature = "qpl")]
#[derive(Clone, Copy)]
enum Verify {
    #[cfg_attr(not(feature = "dto"), allow(dead_code))]
    Pending,
    Checked {
        actual: u32,
        expected: u32,
    },
}

#[cfg(feature = "qpl")]
struct ChunkVerifier {
    enabled: bool,
    states: Vec<Option<Verify>>,
    expected: Vec<u32>,
    checked: u64,
    #[cfg(feature = "dto")]
    ops: Vec<DtoOp>,
    #[cfg(feature = "dto")]
    stats: DtoStats,
}

#[cfg(feature = "qpl")]
impl ChunkVerifier {
    fn new(enabled: bool, slots: usize) -> Self {
        Self {
            enabled,
            states: vec![None; slots],
            expected: vec![0; slots],
            checked: 0,
            #[cfg(feature = "dto")]
            ops: (0..slots).map(|_| DtoOp::default()).collect(),
            #[cfg(feature = "dto")]
            stats: DtoStats::default(),
        }
    }

    fn state(&self, slot: usize) -> Option<Verify> {
        self.states[slot]
    }

    fn start(&mut self, slot: usize, data: &[u8], expected: u32) {
        debug_assert!(self.enabled);
        self.expected[slot] = expected;
        self.checked += 1;
        #[cfg(feature = "dto")]
        {
            // SAFETY: the pool output buffer is not resized or reused until
            // this slot is cleared, and the op lives in `self.ops`.
            if unsafe { self.ops[slot].submit_crc(data.as_ptr(), data.len(), &self.stats) }
                == DtoSubmit::Submitted
            {
                self.states[slot] = Some(Verify::Pending);
                return;
            }
        }
        self.states[slot] = Some(Verify::Checked {
            actual: crc32c(data),
            expected,
        });
    }

    /// Returns true when the slot moved from pending to checked.
    #[cfg(feature = "dto")]
    fn poll(&mut self, slot: usize, data: &[u8]) -> bool {
        let actual = match self.ops[slot].poll(&self.stats) {
            DtoPoll::Pending => return false,
            DtoPoll::Done => self.ops[slot].crc(),
            DtoPoll::Failed { .. } => crc32c(data),
        };
        self.states[slot] = Some(Verify::Checked {
            actual,
            expected: self.expected[slot],
        });
        true
    }

    #[cfg(not(feature = "dto"))]
    fn poll(&mut self, _slot: usize, _data: &[u8]) -> bool {
        unreachable!("software verification never pends")
    }

    fn clear(&mut self, slot: usize) {
        self.states[slot] = None;
    }

    fn checked(&self) -> u64 {
        self.checked
    }

    #[cfg(feature = "dto")]
    fn report(&self) -> (u64, u64, u64) {
        self.stats.snapshot()
    }

    #[cfg(not(feature = "dto"))]
    fn report(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }
}

/// Restore-side population of the all-zero chunks: eager restore leaves them
/// as holes in the memfd and the guest pays the page allocation on first
/// touch. `Populate::Dsa` MEMFILLs them through DTO (3.1 GiB in 16 ms in the
/// in-VMM measurement), `Populate::Cpu` uses `MADV_POPULATE_WRITE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Populate {
    None,
    Cpu,
    Dsa,
}

impl FromStr for Populate {
    type Err = ParsePopulateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "cpu" => Ok(Self::Cpu),
            "dsa" => Ok(Self::Dsa),
            _ => Err(ParsePopulateError(value.to_owned())),
        }
    }
}

impl fmt::Display for Populate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::None => "none",
            Self::Cpu => "cpu",
            Self::Dsa => "dsa",
        })
    }
}

#[derive(Debug, Error)]
#[error("Unknown populate mode {0:?} (expected none, cpu or dsa)")]
pub(crate) struct ParsePopulateError(String);

/// Largest single fill: stays under any work queue's max_transfer_size.
const POPULATE_RUN_LIMIT: usize = 1 << 30;

pub(crate) fn populate_zero_chunks(
    destination: &File,
    destination_offset: u64,
    expected_size: u64,
    manifest_path: &Path,
    mode: Populate,
    depth: usize,
) -> Result<(u64, Duration), Error> {
    let started = Instant::now();
    if mode == Populate::None {
        return Ok((0, started.elapsed()));
    }
    let manifest_bytes = fs::read(manifest_path).map_err(Error::Read)?;
    let manifest: SlotManifest =
        serde_json::from_slice(&manifest_bytes).map_err(Error::ReadManifest)?;
    // Merge adjacent zero chunks into runs.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for record in manifest.chunks.iter().filter(|record| record.zero) {
        let offset = record.uncompressed_offset as usize;
        let length = record.uncompressed_length as usize;
        match runs.last_mut() {
            Some((start, len))
                if *start + *len == offset && *len + length <= POPULATE_RUN_LIMIT =>
            {
                *len += length;
            }
            _ => runs.push((offset, length)),
        }
    }
    let total: u64 = runs.iter().map(|(_, len)| *len as u64).sum();
    if runs.is_empty() {
        return Ok((0, started.elapsed()));
    }
    let mapping = Mapping::map_file(destination, destination_offset, expected_size, true)
        .map_err(Error::Write)?;
    match mode {
        Populate::None => unreachable!(),
        Populate::Cpu => {
            for &(offset, length) in &runs {
                mapping
                    .populate_range(offset, length, true)
                    .map_err(Error::Write)?;
            }
        }
        Populate::Dsa => populate_dsa(&mapping, &runs, depth)?,
    }
    Ok((total, started.elapsed()))
}

#[cfg(feature = "dto")]
fn populate_dsa(mapping: &Mapping, runs: &[(usize, usize)], depth: usize) -> Result<(), Error> {
    let depth = depth.max(1);
    let mut ops: Vec<DtoOp> = (0..depth).map(|_| DtoOp::default()).collect();
    let mut live: Vec<Option<(usize, usize)>> = vec![None; depth];
    let stats = DtoStats::default();
    let mut next = 0_usize;
    let mut inflight = 0_usize;
    while next < runs.len() || inflight != 0 {
        let mut made_progress = false;
        for (slot, entry) in live.iter_mut().enumerate() {
            if let Some((offset, length)) = *entry {
                match ops[slot].poll(&stats) {
                    DtoPoll::Pending => continue,
                    DtoPoll::Done => {}
                    DtoPoll::Failed { status } => {
                        log::warn!(
                            "DSA memfill of {length} bytes at {offset:#x} failed with status {status:#x}; using MADV_POPULATE_WRITE"
                        );
                        mapping
                            .populate_range(offset, length, true)
                            .map_err(Error::Write)?;
                    }
                }
                *entry = None;
                inflight -= 1;
                made_progress = true;
            }
            if next < runs.len() {
                let (offset, length) = runs[next];
                next += 1;
                let destination = mapping.mut_ptr(offset, length).map_err(Error::Write)?;
                // SAFETY: the destination memfd is owned by this daemon and the
                // mapping outlives the loop; ops stay at fixed addresses.
                match unsafe { ops[slot].submit_memfill_zero(destination, length, &stats) } {
                    DtoSubmit::Submitted => {
                        *entry = Some((offset, length));
                        inflight += 1;
                    }
                    DtoSubmit::Fallback => {
                        mapping
                            .populate_range(offset, length, true)
                            .map_err(Error::Write)?;
                    }
                }
                made_progress = true;
            }
        }
        if !made_progress {
            thread::yield_now();
        }
    }
    let (submitted, fallback, failed) = stats.snapshot();
    log::info!(
        "populate: {} zero runs, dsa submitted={submitted} fallback={fallback} failed={failed}",
        runs.len()
    );
    Ok(())
}

#[cfg(not(feature = "dto"))]
fn populate_dsa(_mapping: &Mapping, _runs: &[(usize, usize)], _depth: usize) -> Result<(), Error> {
    Err(Error::Classify(ClassifyError::DsaUnavailable))
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::os::unix::fs::FileExt;

    use super::*;

    fn sample() -> Vec<u8> {
        (0..256 * 1024).map(|index| (index % 251) as u8).collect()
    }

    #[test]
    fn codec_names_round_trip() {
        for codec in [
            Codec::Lz4,
            Codec::Zstd,
            Codec::QplHardware,
            Codec::QplHardwareStatic,
            Codec::QplHardwareDynamic,
            Codec::QplHardwareStaticAsync,
            Codec::QplHardwareDynamicAsync,
            Codec::QplAuto,
        ] {
            assert_eq!(codec.to_string().parse::<Codec>().unwrap(), codec);
        }
    }

    #[test]
    fn lz4_round_trip() {
        let input = sample();
        let compressed = compress_chunk(Codec::Lz4, &input, 1).unwrap();
        assert_eq!(
            decompress_chunk(Codec::Lz4, &compressed, input.len()).unwrap(),
            input
        );
    }

    #[test]
    fn zstd_round_trip() {
        let input = sample();
        let compressed = compress_chunk(Codec::Zstd, &input, 1).unwrap();
        assert_eq!(
            decompress_chunk(Codec::Zstd, &compressed, input.len()).unwrap(),
            input
        );
    }

    #[test]
    #[cfg(not(feature = "qpl"))]
    fn qpl_fails_explicitly_when_unavailable() {
        assert!(matches!(
            compress_chunk(Codec::QplHardware, b"data", 1),
            Err(Error::QplUnavailable)
        ));
    }

    #[test]
    #[cfg(feature = "qpl")]
    fn qpl_hardware_huffman_modes_round_trip() {
        let input = sample();
        for codec in [Codec::QplHardwareStatic, Codec::QplHardwareDynamic] {
            let compressed = compress_chunk(codec, &input, 1).unwrap();
            assert_eq!(
                decompress_chunk(codec, &compressed, input.len()).unwrap(),
                input
            );
        }
    }

    #[test]
    #[cfg(feature = "qpl")]
    fn qpl_hardware_async_file_round_trip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source_path = temp_dir.path().join("source");
        let data_path = temp_dir.path().join("compressed");
        let manifest_path = temp_dir.path().join("manifest.json");
        let destination_path = temp_dir.path().join("destination");
        let input = sample();
        fs::write(&source_path, &input).unwrap();
        let source = File::open(source_path).unwrap();
        let destination = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(destination_path)
            .unwrap();
        destination.set_len(input.len() as u64).unwrap();

        for codec in [
            Codec::QplHardwareStaticAsync,
            Codec::QplHardwareDynamicAsync,
        ] {
            compress_file(
                &source,
                0,
                input.len() as u64,
                &data_path,
                &manifest_path,
                codec,
                64 * 1024,
                4,
                1,
                AccelOptions::default(),
            )
            .unwrap();
            decompress_file(
                &data_path,
                &manifest_path,
                &destination,
                0,
                input.len() as u64,
                4,
                false,
            )
            .unwrap();

            let mut actual = vec![0_u8; input.len()];
            destination.read_exact_at(&mut actual, 0).unwrap();
            assert_eq!(actual, input);
        }
    }

    #[test]
    fn stats_report_ratio_and_throughput() {
        let stats = CompressionStats {
            input_bytes: 2 << 30,
            output_bytes: 1 << 30,
            chunks: 2,
            elapsed: Duration::from_secs(2),
        };
        assert_eq!(stats.ratio(), 0.5);
        assert_eq!(stats.throughput_gib_per_second(), 1.0);
    }

    #[test]
    fn parallel_file_round_trip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source_path = temp_dir.path().join("source");
        let data_path = temp_dir.path().join("compressed");
        let manifest_path = temp_dir.path().join("index.json");
        let restored_path = temp_dir.path().join("restored");
        let input: Vec<u8> = (0..3 * 1024 * 1024 + 17)
            .map(|index| ((index * 17) % 251) as u8)
            .collect();
        fs::write(&source_path, &input).unwrap();
        let source = File::open(source_path).unwrap();

        let compressed = compress_file(
            &source,
            0,
            input.len() as u64,
            &data_path,
            &manifest_path,
            Codec::Lz4,
            256 * 1024,
            4,
            1,
            AccelOptions::default(),
        )
        .unwrap();
        assert_eq!(compressed.chunks, 13);

        let restored = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(restored_path)
            .unwrap();
        restored.set_len(input.len() as u64).unwrap();
        let decompressed = decompress_file(
            &data_path,
            &manifest_path,
            &restored,
            0,
            input.len() as u64,
            3,
            false,
        )
        .unwrap();
        assert_eq!(decompressed.chunks, 13);
        let mut actual = vec![0_u8; input.len()];
        restored.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(actual, input);
    }

    #[test]
    fn zero_chunks_are_elided_and_restored() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source_path = temp_dir.path().join("source");
        let data_path = temp_dir.path().join("compressed");
        let manifest_path = temp_dir.path().join("index.json");
        let restored_path = temp_dir.path().join("restored");
        let mut input = vec![0_u8; 3 * 64 * 1024];
        input[64 * 1024..2 * 64 * 1024].fill(0x5a);
        fs::write(&source_path, &input).unwrap();

        compress_file(
            &File::open(source_path).unwrap(),
            0,
            input.len() as u64,
            &data_path,
            &manifest_path,
            Codec::Lz4,
            64 * 1024,
            2,
            1,
            AccelOptions::default(),
        )
        .unwrap();
        let manifest: SlotManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.chunks.iter().filter(|chunk| chunk.zero).count(), 2);
        assert!(
            manifest
                .chunks
                .iter()
                .filter(|chunk| chunk.zero)
                .all(|chunk| chunk.compressed_length == 0)
        );

        let restored = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(restored_path)
            .unwrap();
        restored.set_len(input.len() as u64).unwrap();
        decompress_file(
            &data_path,
            &manifest_path,
            &restored,
            0,
            input.len() as u64,
            2,
            false,
        )
        .unwrap();
        let mut actual = vec![0_u8; input.len()];
        restored.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(actual, input);
    }
}
