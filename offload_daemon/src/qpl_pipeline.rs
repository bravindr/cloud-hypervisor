// Copyright © 2026 The Cloud Hypervisor Authors
//
// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::hint::spin_loop;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::ptr::{NonNull, null_mut};
use std::time::Instant;
use std::{io, slice, thread};

use crate::compression::{
    ChunkRecord, Codec, CompressionStats, Error, FORMAT_VERSION, SlotManifest,
};
use crate::qpl::{ExecutionPath, HuffmanMode, JobPool};

pub(crate) struct CompressionFile {
    pub(crate) slot: u32,
    pub(crate) source: File,
    pub(crate) source_offset: u64,
    pub(crate) source_size: u64,
    pub(crate) data_path: PathBuf,
    pub(crate) manifest_path: PathBuf,
}

pub(crate) struct DecompressionFile {
    pub(crate) slot: u32,
    pub(crate) data_path: PathBuf,
    pub(crate) manifest_path: PathBuf,
    pub(crate) destination: File,
    pub(crate) destination_offset: u64,
    pub(crate) expected_size: u64,
}

struct FileMapping {
    address: NonNull<u8>,
    length: usize,
}

impl FileMapping {
    fn new(file: &File, length: usize) -> io::Result<Self> {
        if length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot map an empty file",
            ));
        }
        // SAFETY: file is open, length is non-zero, and Self owns the returned mapping.
        let address = unsafe {
            libc::mmap(
                null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            address: NonNull::new(address.cast())
                .expect("successful mmap returned a non-null address"),
            length,
        })
    }

    fn read_ptr(&self, offset: usize, length: usize) -> io::Result<*const u8> {
        self.check_range(offset, length)?;
        // SAFETY: check_range established that offset is inside this live mapping.
        Ok(unsafe { self.address.as_ptr().add(offset) })
    }

    fn is_zero(&self, offset: usize, length: usize) -> io::Result<bool> {
        let pointer = self.read_ptr(offset, length)?;
        // SAFETY: read_ptr validated the complete range in this live mapping.
        Ok(unsafe { slice::from_raw_parts(pointer, length) }
            .iter()
            .all(|byte| *byte == 0))
    }

    fn check_range(&self, offset: usize, length: usize) -> io::Result<()> {
        if offset
            .checked_add(length)
            .is_none_or(|end| end > self.length)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mapped file range is out of bounds",
            ));
        }
        Ok(())
    }
}

impl Drop for FileMapping {
    fn drop(&mut self) {
        // SAFETY: address and length describe the mapping exclusively owned by Self.
        unsafe {
            libc::munmap(self.address.as_ptr().cast(), self.length);
        }
    }
}

struct CompressionState {
    slot: u32,
    source: FileMapping,
    source_offset: usize,
    source_size: usize,
    output: File,
    manifest_path: PathBuf,
    next_chunk: usize,
    chunk_count: usize,
    output_bytes: u64,
    records: Vec<ChunkRecord>,
}

#[derive(Clone, Copy)]
struct CompressionJob {
    state_index: usize,
    uncompressed_offset: usize,
    uncompressed_length: usize,
}

pub(crate) fn compress_files(
    files: Vec<CompressionFile>,
    codec: Codec,
    chunk_size: usize,
    workers: usize,
) -> Result<Vec<(u32, CompressionStats)>, Error> {
    let started = Instant::now();
    let chunk_size_u32 = super::compression::validate_chunk_size(chunk_size)?;
    let huffman_mode = match codec {
        Codec::QplHardwareStaticAsync => HuffmanMode::Static,
        Codec::QplHardwareDynamicAsync => HuffmanMode::Dynamic,
        _ => unreachable!("global QPL compression requires an asynchronous hardware codec"),
    };
    let mut pool = JobPool::new(ExecutionPath::Hardware, huffman_mode, workers)?;
    let mut states = files
        .into_iter()
        .map(|file| {
            let source_length = usize::try_from(
                file.source_offset
                    .checked_add(file.source_size)
                    .ok_or(Error::ChunkTooLarge)?,
            )
            .map_err(|_| Error::ChunkTooLarge)?;
            let source_offset =
                usize::try_from(file.source_offset).map_err(|_| Error::ChunkTooLarge)?;
            let source_size =
                usize::try_from(file.source_size).map_err(|_| Error::ChunkTooLarge)?;
            let chunk_count = source_size.div_ceil(chunk_size);
            let output = OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&file.data_path)
                .map_err(Error::Write)?;
            Ok(CompressionState {
                slot: file.slot,
                source: FileMapping::new(&file.source, source_length).map_err(Error::Read)?,
                source_offset,
                source_size,
                output,
                manifest_path: file.manifest_path,
                next_chunk: 0,
                chunk_count,
                output_bytes: 0,
                records: Vec::with_capacity(chunk_count),
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let mut active = vec![None; pool.capacity()];
    let mut next_state = 0;
    let mut active_count = 0;
    for (job_index, active_job) in active.iter_mut().enumerate() {
        *active_job = submit_compression_job(
            &mut pool,
            job_index,
            &mut states,
            &mut next_state,
            chunk_size,
        )?;
        active_count += usize::from(active_job.is_some());
    }

    let mut completion_cursor = 0;
    while active_count != 0 {
        let (job_index, output_size) = next_completion(&mut pool, &active, &mut completion_cursor)?;
        let job = active[job_index].take().unwrap();
        let compressed_length = u32::try_from(output_size).map_err(|_| Error::ChunkTooLarge)?;
        let state = &mut states[job.state_index];
        let compressed_offset = state.output_bytes;
        state
            .output
            .write_all_at(pool.output(job_index, output_size), compressed_offset)
            .map_err(Error::Write)?;
        state.output_bytes += output_size as u64;
        state.records.push(ChunkRecord {
            uncompressed_offset: job.uncompressed_offset as u64,
            uncompressed_length: job.uncompressed_length as u32,
            compressed_offset,
            compressed_length,
            zero: false,
        });
        active_count -= 1;
        active[job_index] = submit_compression_job(
            &mut pool,
            job_index,
            &mut states,
            &mut next_state,
            chunk_size,
        )?;
        active_count += usize::from(active[job_index].is_some());
    }
    drop(pool);

    let elapsed = started.elapsed();
    states
        .into_iter()
        .map(|mut state| {
            state
                .records
                .sort_unstable_by_key(|record| record.uncompressed_offset);
            state.output.sync_all().map_err(Error::Write)?;
            let manifest = SlotManifest {
                version: FORMAT_VERSION,
                codec,
                chunk_size: chunk_size_u32,
                uncompressed_size: state.source_size as u64,
                chunks: state.records,
            };
            let manifest_bytes =
                serde_json::to_vec_pretty(&manifest).map_err(Error::WriteManifest)?;
            fs::write(&state.manifest_path, manifest_bytes).map_err(Error::Write)?;
            Ok((
                state.slot,
                CompressionStats {
                    input_bytes: state.source_size as u64,
                    output_bytes: state.output_bytes,
                    chunks: state.chunk_count,
                    elapsed,
                },
            ))
        })
        .collect()
}

fn submit_compression_job(
    pool: &mut JobPool,
    job_index: usize,
    states: &mut [CompressionState],
    next_state: &mut usize,
    chunk_size: usize,
) -> Result<Option<CompressionJob>, Error> {
    for _ in 0..states.len() {
        let state_index = *next_state;
        *next_state = (*next_state + 1) % states.len();
        let state = &mut states[state_index];
        while state.next_chunk < state.chunk_count {
            let chunk_index = state.next_chunk;
            state.next_chunk += 1;
            let uncompressed_offset = chunk_index
                .checked_mul(chunk_size)
                .ok_or(Error::ChunkTooLarge)?;
            let uncompressed_length = (state.source_size - uncompressed_offset).min(chunk_size);
            let source_offset = state.source_offset + uncompressed_offset;
            if state
                .source
                .is_zero(source_offset, uncompressed_length)
                .map_err(Error::Read)?
            {
                state.records.push(ChunkRecord {
                    uncompressed_offset: uncompressed_offset as u64,
                    uncompressed_length: uncompressed_length as u32,
                    compressed_offset: 0,
                    compressed_length: 0,
                    zero: true,
                });
                continue;
            }
            let input = state
                .source
                .read_ptr(source_offset, uncompressed_length)
                .map_err(Error::Read)?;
            // SAFETY: the mapped input range outlives the pool and is not modified.
            unsafe {
                pool.submit_compress_mapped_input(job_index, input, uncompressed_length)?;
            }
            return Ok(Some(CompressionJob {
                state_index,
                uncompressed_offset,
                uncompressed_length,
            }));
        }
    }
    Ok(None)
}

struct DecompressionState {
    slot: u32,
    input: Option<FileMapping>,
    input_bytes: u64,
    output: File,
    destination_offset: u64,
    expected_size: usize,
    records: Vec<ChunkRecord>,
    next_chunk: usize,
}

#[derive(Clone, Copy)]
struct DecompressionJob {
    state_index: usize,
    record_index: usize,
}

pub(crate) fn decompress_files(
    files: Vec<DecompressionFile>,
    codec: Codec,
    workers: usize,
) -> Result<Vec<(u32, CompressionStats)>, Error> {
    let started = Instant::now();
    let huffman_mode = match codec {
        Codec::QplHardwareStaticAsync => HuffmanMode::Static,
        Codec::QplHardwareDynamicAsync => HuffmanMode::Dynamic,
        _ => unreachable!("global QPL decompression requires an asynchronous hardware codec"),
    };
    let mut states = files
        .into_iter()
        .map(|file| {
            let manifest_bytes = fs::read(&file.manifest_path).map_err(Error::Read)?;
            let manifest: SlotManifest =
                serde_json::from_slice(&manifest_bytes).map_err(Error::ReadManifest)?;
            let input_file = File::open(&file.data_path).map_err(Error::Read)?;
            let input_bytes = input_file.metadata().map_err(Error::Read)?.len();
            validate_manifest(&manifest, file.expected_size, input_bytes, codec)?;
            Ok(DecompressionState {
                slot: file.slot,
                input: if input_bytes == 0 {
                    None
                } else {
                    Some(FileMapping::new(&input_file, input_bytes as usize).map_err(Error::Read)?)
                },
                input_bytes,
                output: file.destination,
                destination_offset: file.destination_offset,
                expected_size: usize::try_from(file.expected_size)
                    .map_err(|_| Error::ChunkTooLarge)?,
                records: manifest.chunks,
                next_chunk: 0,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let mut pool = JobPool::new(ExecutionPath::Hardware, huffman_mode, workers)?;
    let mut active = vec![None; pool.capacity()];
    let mut next_state = 0;
    let mut active_count = 0;
    for (job_index, active_job) in active.iter_mut().enumerate() {
        *active_job = submit_decompression_job(&mut pool, job_index, &mut states, &mut next_state)?;
        active_count += usize::from(active_job.is_some());
    }

    let mut completion_cursor = 0;
    while active_count != 0 {
        let (job_index, output_size) = next_completion(&mut pool, &active, &mut completion_cursor)?;
        let job = active[job_index].take().unwrap();
        let record = &states[job.state_index].records[job.record_index];
        if output_size != record.uncompressed_length as usize {
            return Err(Error::LengthMismatch {
                expected: record.uncompressed_length as usize,
                actual: output_size,
            });
        }
        states[job.state_index]
            .output
            .write_all_at(
                pool.output(job_index, output_size),
                states[job.state_index].destination_offset + record.uncompressed_offset,
            )
            .map_err(Error::Write)?;
        active_count -= 1;
        active[job_index] =
            submit_decompression_job(&mut pool, job_index, &mut states, &mut next_state)?;
        active_count += usize::from(active[job_index].is_some());
    }
    drop(pool);

    let elapsed = started.elapsed();
    Ok(states
        .into_iter()
        .map(|state| {
            (
                state.slot,
                CompressionStats {
                    input_bytes: state.expected_size as u64,
                    output_bytes: state.input_bytes,
                    chunks: state.records.len(),
                    elapsed,
                },
            )
        })
        .collect())
}

fn submit_decompression_job(
    pool: &mut JobPool,
    job_index: usize,
    states: &mut [DecompressionState],
    next_state: &mut usize,
) -> Result<Option<DecompressionJob>, Error> {
    for _ in 0..states.len() {
        let state_index = *next_state;
        *next_state = (*next_state + 1) % states.len();
        let state = &mut states[state_index];
        while let Some(record) = state.records.get(state.next_chunk) {
            let record_index = state.next_chunk;
            state.next_chunk += 1;
            if record.zero {
                continue;
            }
            let input = state
                .input
                .as_ref()
                .ok_or_else(|| Error::InvalidManifest("missing compressed data".to_owned()))?
                .read_ptr(
                    record.compressed_offset as usize,
                    record.compressed_length as usize,
                )
                .map_err(Error::Read)?;
            // SAFETY: manifest validation established a valid mapped input range which outlives
            // the submitted job.
            unsafe {
                pool.submit_decompress_mapped_input(
                    job_index,
                    input,
                    record.compressed_length as usize,
                    record.uncompressed_length as usize,
                )?;
            }
            return Ok(Some(DecompressionJob {
                state_index,
                record_index,
            }));
        }
    }
    Ok(None)
}

fn next_completion<T>(
    pool: &mut JobPool,
    active: &[Option<T>],
    cursor: &mut usize,
) -> Result<(usize, usize), Error> {
    let mut empty_scans = 0;
    loop {
        for distance in 0..active.len() {
            let index = (*cursor + distance) % active.len();
            if active[index].is_some()
                && let Some(output_size) = pool.poll(index)?
            {
                *cursor = (index + 1) % active.len();
                return Ok((index, output_size));
            }
        }
        empty_scans += 1;
        if empty_scans % 64 == 0 {
            thread::yield_now();
        } else {
            spin_loop();
        }
    }
}

fn validate_manifest(
    manifest: &SlotManifest,
    expected_size: u64,
    compressed_size: u64,
    expected_codec: Codec,
) -> Result<(), Error> {
    if manifest.version != FORMAT_VERSION {
        return Err(Error::UnsupportedVersion(manifest.version));
    }
    if manifest.codec != expected_codec {
        return Err(Error::InvalidManifest(format!(
            "codec is {}, expected {expected_codec}",
            manifest.codec
        )));
    }
    if manifest.uncompressed_size != expected_size {
        return Err(Error::LengthMismatch {
            expected: expected_size as usize,
            actual: manifest.uncompressed_size as usize,
        });
    }
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn manifest_rejects_compressed_range_past_file() {
        let manifest = SlotManifest {
            version: FORMAT_VERSION,
            codec: Codec::QplHardwareDynamicAsync,
            chunk_size: 4096,
            uncompressed_size: 4096,
            chunks: vec![ChunkRecord {
                uncompressed_offset: 0,
                uncompressed_length: 4096,
                compressed_offset: 128,
                compressed_length: 64,
                zero: false,
            }],
        };

        assert!(validate_manifest(&manifest, 4096, 191, manifest.codec).is_err());
        validate_manifest(&manifest, 4096, 192, manifest.codec).unwrap();
    }

    #[test]
    fn global_pipeline_round_trip() {
        let directory = tempdir().unwrap();
        let inputs = [
            (0..640 * 1024)
                .map(|index| if index < 64 * 1024 { 0 } else { index as u8 })
                .collect::<Vec<_>>(),
            (0..64 * 1024)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>(),
        ];
        let compression_files = inputs
            .iter()
            .enumerate()
            .map(|(slot, input)| {
                let source_path = directory.path().join(format!("source-{slot}"));
                fs::write(&source_path, input).unwrap();
                CompressionFile {
                    slot: slot as u32,
                    source: File::open(source_path).unwrap(),
                    source_offset: 0,
                    source_size: input.len() as u64,
                    data_path: directory.path().join(format!("memory-{slot}.compressed")),
                    manifest_path: directory.path().join(format!("memory-{slot}.index.json")),
                }
            })
            .collect();

        let codec = Codec::QplHardwareDynamicAsync;
        let compressed = compress_files(compression_files, codec, 64 * 1024, 4).unwrap();
        assert_eq!(compressed.len(), 2);
        for (slot, stats) in &compressed {
            let data_path = directory.path().join(format!("memory-{slot}.compressed"));
            assert_eq!(fs::metadata(data_path).unwrap().len(), stats.output_bytes);
        }

        let destination_paths = [
            directory.path().join("destination-0"),
            directory.path().join("destination-1"),
        ];
        let decompression_files = destination_paths
            .iter()
            .enumerate()
            .map(|(slot, path)| {
                let destination = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap();
                destination.set_len(inputs[slot].len() as u64).unwrap();
                DecompressionFile {
                    slot: slot as u32,
                    data_path: directory.path().join(format!("memory-{slot}.compressed")),
                    manifest_path: directory.path().join(format!("memory-{slot}.index.json")),
                    destination,
                    destination_offset: 0,
                    expected_size: inputs[slot].len() as u64,
                }
            })
            .collect();

        let decompressed = decompress_files(decompression_files, codec, 4).unwrap();
        assert_eq!(decompressed.len(), 2);
        for (path, input) in destination_paths.iter().zip(inputs) {
            assert_eq!(fs::read(path).unwrap(), input);
        }
    }
}
