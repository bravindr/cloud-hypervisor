// Copyright © 2026 The Cloud Hypervisor Authors
//
// SPDX-License-Identifier: Apache-2.0

//! Reference offload daemon for Cloud Hypervisor snapshot/restore.
//!
//! It acts as the local live-migration peer of CH's existing
//! `vm.send-migration` and `vm.receive-migration` endpoints, persisting the
//! migration stream to a directory and replaying it later.
//!
//! Snapshot (daemon receives): writes each guest RAM slot to `memory-<slot>`,
//! the `VmMigrationConfig` to `migration_config.json`, and the device state to
//! `state.json`.
//!
//! Restore (daemon sends): replays those files back to CH. `--resume` resumes
//! the VM, and `--ondemand` serves pages on demand over the postcopy fault
//! connection instead of preloading them.

mod classify;
mod compression;
mod crc;
mod diff;
#[cfg(feature = "dto")]
mod dto;
mod mapping;
#[cfg(feature = "qpl")]
mod qpl;

use std::ffi::{CString, NulError};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "qpl")]
use std::time::Duration;
use std::time::Instant;
use std::{result, thread};

use clap::{Parser, Subcommand};
use log::{debug, info};
use thiserror::Error;
use vm_memory::mmap::{MmapRegion, MmapRegionError};
use vm_memory::{
    Address, Bytes, FileOffset, GuestAddress, GuestMemoryError, GuestMemoryRegion, GuestRegionMmap,
    MemoryRegionAddress,
};
use vm_migration::MigratableError;
use vm_migration::protocol::{
    Command, ConnectionRole, MemoryRange, MemoryRangeTable, Request, Response, Status,
};
use vmm::VmMigrationConfig;
use vmm::api::MigrationMode;
use vmm::migration::SNAPSHOT_STATE_FILE;
use vmm::sparse::copy_region;
use vmm_sys_util::errno;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use crate::classify::{AccelOptions, Classify};
use crate::compression::{Codec, Populate, compress_file, decompress_file, populate_zero_chunks};
use crate::diff::{DiffCompare, DiffOptions};
use crate::mapping::Mapping;

const MIGRATION_CONFIG_FILENAME: &str = "migration_config.json";

#[derive(Debug, Error)]
enum Error {
    #[error("Creating the output directory")]
    CreateOutputDir(#[source] io::Error),
    #[error("Opening the socket lock file")]
    OpenLockFile(#[source] io::Error),
    #[error("Socket {0:?} is already in use by another daemon")]
    SocketInUse(PathBuf),
    #[error("Locking the socket")]
    LockSocket(#[source] io::Error),
    #[error("Binding the UNIX socket")]
    BindSocket(#[source] io::Error),
    #[error("Connecting to the CH socket")]
    Connect(#[source] io::Error),
    #[error("Accepting CH connection")]
    Accept(#[source] io::Error),
    #[error("Migration protocol")]
    Protocol(#[source] MigratableError),
    #[error("Receiving memory fd")]
    RecvMemoryFd(#[source] errno::Error),
    #[error("MemoryFd command carried no file descriptor")]
    MissingMemoryFd,
    #[error("Sending memory fd")]
    SendMemoryFd(#[source] errno::Error),
    #[error("Reading a snapshot artifact")]
    ReadFile(#[source] io::Error),
    #[error("Writing a snapshot artifact")]
    WriteFile(#[source] io::Error),
    #[error("Reading migration payload")]
    ReadPayload(#[source] io::Error),
    #[error("Sending migration payload")]
    WritePayload(#[source] io::Error),
    #[error("(De)serializing VmMigrationConfig")]
    Config(#[from] serde_json::Error),
    #[error("Unexpected command {0:?} while expecting {1}")]
    UnexpectedCommand(Command, &'static str),
    #[error("CH abandoned the snapshot")]
    Abandoned,
    #[error("CH rejected the {0} command")]
    Rejected(&'static str),
    #[error("Completion received before {0}")]
    PrematureCompletion(&'static str),
    #[error("Creating memfd")]
    MemfdCreate(#[source] io::Error),
    #[error("Invalid memfd name")]
    MemfdName(#[source] NulError),
    #[error("Sizing memfd")]
    MemfdSetLen(#[source] io::Error),
    #[error("Copying snapshot memory")]
    CopyMemory(#[source] io::Error),
    #[error("No MemoryFd received for slot {0}")]
    MissingSlot(u32),
    #[error("Field {0:?} missing from memory_manager_data")]
    MissingField(&'static str),
    #[error("Mapping memfd for the on demand slot")]
    Mmap(#[source] MmapRegionError),
    #[error("Guest region does not match the mapped size")]
    GuestRegion,
    #[error("Cloning memfd for mmap")]
    CloneMemfd(#[source] io::Error),
    #[error("Spawning the fault serve thread")]
    SpawnServeThread(#[source] io::Error),
    #[error("The fault serve thread panicked")]
    ServeThreadPanic,
    #[error("A memory slot worker panicked")]
    WorkerPanic,
    #[error("PageFault gpa={0:#x} len={1} is not within any slot")]
    PageFaultUnmapped(u64, u64),
    #[error("Writing a faulted page into guest memory")]
    WriteGuestMemory(#[source] GuestMemoryError),
    #[error("Processing compressed snapshot memory")]
    Compression(#[from] compression::Error),
    #[error("Compressed snapshots do not support on demand restore")]
    CompressedOnDemand,
    #[error("Diff checkpoint")]
    Diff(#[from] diff::Error),
    #[error("Diff checkpoints need an async QPL codec")]
    DiffCodec,
}

type Result<T> = result::Result<T, Error>;

fn memory_slot_filename(slot: u32) -> String {
    format!("memory-{slot}")
}

#[derive(Parser, Debug)]
#[command(name = "offload_daemon")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand, Debug)]
enum Mode {
    /// Receive a snapshot from CH and persist it to disk.
    Snapshot {
        /// Path to a UNIX socket to bind and listen on.
        #[arg(long)]
        socket: PathBuf,
        /// Directory to write snapshot artifacts into.
        #[arg(long)]
        output_dir: PathBuf,
        /// Compression codec. Omit to preserve the sparse raw format.
        #[arg(long)]
        compression: Option<Codec>,
        /// Independently compressed chunk size in bytes.
        #[arg(long, default_value_t = 1 << 20)]
        chunk_size: usize,
        /// Number of compression workers.
        #[arg(long, default_value_t = default_workers())]
        workers: usize,
        /// Zstd compression level.
        #[arg(long, default_value_t = 1)]
        zstd_level: i32,
        /// Zero-chunk classification for the async QPL codecs: `cpu` (word
        /// scan) or `dsa` (DSA COMPARE through DTO; needs DTO_WQ_LIST).
        #[arg(long, default_value_t = default_classify())]
        classify: Classify,
        /// DSA operations kept in flight per memory slot.
        #[arg(long, default_value_t = 32)]
        dsa_depth: usize,
        /// Record a CRC32C per compressed chunk (generated on DSA when
        /// classify=dsa).
        #[arg(long)]
        crc: bool,
        /// Do not MADV_POPULATE_READ the slot mapping before classification.
        #[arg(long)]
        no_prefault: bool,
        /// Keep a reference copy of the guest (one sparse file per slot) in
        /// this directory: a full checkpoint initialises it, a diff
        /// checkpoint compares dirty pages against it and updates it.
        #[arg(long)]
        reference_dir: Option<PathBuf>,
        /// The previous checkpoint of this chain. With a dirty log from CH
        /// (send-migration dirty_log=keep|consume) and a reference, write a
        /// diff against it instead of a full checkpoint.
        #[arg(long)]
        parent: Option<PathBuf>,
        /// How dirty pages are checked against the reference: none (store
        /// every dirty page), cpu, or dsa (batched COMPARE + DUALCAST).
        #[arg(long, default_value = "cpu")]
        diff_compare: DiffCompare,
        /// Pages per DSA batch descriptor.
        #[arg(long, default_value_t = 256)]
        diff_batch: usize,
        /// DSA batch descriptors in flight.
        #[arg(long, default_value_t = 8)]
        diff_depth: usize,
    },
    /// Read a snapshot from disk and stream it to a listening CH instance.
    Restore {
        /// Path of the UNIX socket that CH is listening on.
        #[arg(long)]
        socket: PathBuf,
        /// Directory to read snapshot artifacts from.
        #[arg(long)]
        input_dir: PathBuf,
        /// If set, the restored VM is resumed (Complete) instead of left
        /// paused (CompletePaused).
        #[arg(long)]
        resume: bool,
        /// On demand paging.
        #[arg(long)]
        ondemand: bool,
        /// Number of decompression workers.
        #[arg(long, default_value_t = default_workers())]
        workers: usize,
        /// Verify each chunk's CRC32C against the manifest (DSA when built
        /// with the dto feature, software otherwise).
        #[arg(long)]
        verify_crc: bool,
        /// Populate the all-zero chunks of the restored memory instead of
        /// leaving holes: `none`, `cpu` (MADV_POPULATE_WRITE) or `dsa` (MEMFILL).
        #[arg(long, default_value_t = Populate::None)]
        populate: Populate,
        /// DSA operations kept in flight per memory slot.
        #[arg(long, default_value_t = 32)]
        dsa_depth: usize,
        /// Back the restored memory with 2 MiB hugetlb pages (MFD_HUGETLB)
        /// instead of 4 KiB pages.
        #[arg(long)]
        hugetlb: bool,
    },
}

fn default_classify() -> Classify {
    if cfg!(feature = "dto") {
        Classify::Dsa
    } else {
        Classify::Cpu
    }
}

#[derive(Clone)]
struct CompressionOptions {
    codec: Codec,
    chunk_size: usize,
    workers: usize,
    zstd_level: i32,
    accel: AccelOptions,
    reference_dir: Option<PathBuf>,
    parent: Option<PathBuf>,
    #[cfg_attr(not(feature = "qpl"), allow(dead_code))]
    diff: DiffOptions,
}

#[derive(Clone, Copy)]
struct RestoreOptions {
    workers: usize,
    verify_crc: bool,
    populate: Populate,
    dsa_depth: usize,
    hugetlb: bool,
}

fn default_workers() -> usize {
    thread::available_parallelism().map_or(1, usize::from)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    match cli.mode {
        Mode::Snapshot {
            socket,
            output_dir,
            compression,
            chunk_size,
            workers,
            zstd_level,
            classify,
            dsa_depth,
            crc,
            no_prefault,
            reference_dir,
            parent,
            diff_compare,
            diff_batch,
            diff_depth,
        } => {
            let compression = compression.map(|codec| CompressionOptions {
                codec,
                chunk_size,
                workers,
                zstd_level,
                accel: AccelOptions {
                    classify,
                    dsa_depth,
                    crc,
                    prefault: !no_prefault,
                },
                reference_dir,
                parent,
                diff: DiffOptions {
                    compare: diff_compare,
                    batch: diff_batch,
                    depth: diff_depth,
                },
            });
            run_snapshot(&socket, &output_dir, compression.as_ref())
        }
        Mode::Restore {
            socket,
            input_dir,
            resume,
            ondemand,
            workers,
            verify_crc,
            populate,
            dsa_depth,
            hugetlb,
        } => run_restore(
            &socket,
            &input_dir,
            resume,
            ondemand,
            RestoreOptions {
                workers,
                verify_crc,
                populate,
                dsa_depth,
                hugetlb,
            },
        ),
    }
}

/// Take an exclusive lock on `<socket>.lock` so two daemons can't bind the same
/// socket, and so removing a stale socket file is safe.
fn acquire_socket_lock(socket_path: &Path) -> Result<File> {
    let lock_path: PathBuf = {
        let mut p = socket_path.as_os_str().to_os_string();
        p.push(".lock");
        p.into()
    };
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(Error::OpenLockFile)?;

    let flock = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0, // 0 means the whole file.
        l_pid: 0,
    };
    loop {
        // SAFETY: fcntl() with F_OFD_SETLK and a valid flock pointer on an owned fd.
        if unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_OFD_SETLK, &flock) } == 0 {
            break;
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // Interrupted by a signal before the lock was taken: retry.
            Some(libc::EINTR) => continue,
            // The lock is held by another daemon.
            Some(libc::EACCES) | Some(libc::EAGAIN) => {
                return Err(Error::SocketInUse(socket_path.to_path_buf()));
            }
            _ => return Err(Error::LockSocket(err)),
        }
    }
    Ok(lock)
}

// Snapshot mode (migration receiver).
fn run_snapshot(
    socket_path: &Path,
    output_dir: &Path,
    compression: Option<&CompressionOptions>,
) -> Result<()> {
    fs::create_dir_all(output_dir).map_err(Error::CreateOutputDir)?;

    // Hold the lock for the daemon's lifetime. While we hold it, any socket at
    // this path is stale from a crashed run, so removing it before bind is safe.
    let _lock = acquire_socket_lock(socket_path)?;
    let _ = fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path).map_err(Error::BindSocket)?;
    info!("Offload daemon listening at {socket_path:?}");

    let (mut stream, _) = listener.accept().map_err(Error::Accept)?;
    info!("CH connected; starting snapshot receive");

    expect_command(&mut stream, Command::Start, "Start")?;
    Response::ok()
        .write_to(&mut stream)
        .map_err(Error::Protocol)?;

    let mut memory_slots: Vec<(u32, File)> = Vec::new();
    let mut dirty: Option<Vec<(u64, u64)>> = None;
    let mut migration_config: Option<VmMigrationConfig> = None;
    let mut state_bytes: Option<Vec<u8>> = None;

    loop {
        let req = Request::read_from(&mut stream).map_err(Error::Protocol)?;
        debug!("snapshot: received command {:?}", req.command());
        match req.command() {
            Command::MemoryFd => {
                let (slot, file) = recv_memory_fd(&stream)?;
                debug!("snapshot: received memory fd for slot {slot}");
                memory_slots.push((slot, file));
                Response::ok()
                    .write_to(&mut stream)
                    .map_err(Error::Protocol)?;
            }
            Command::Config => {
                let mut buf = vec![0u8; req.length() as usize];
                stream.read_exact(&mut buf).map_err(Error::ReadPayload)?;
                migration_config = Some(serde_json::from_slice(&buf)?);
                fs::write(output_dir.join(MIGRATION_CONFIG_FILENAME), &buf)
                    .map_err(Error::WriteFile)?;
                Response::ok()
                    .write_to(&mut stream)
                    .map_err(Error::Protocol)?;
            }
            Command::DirtyLog => {
                let table = MemoryRangeTable::read_from(&mut stream, req.length())
                    .map_err(Error::Protocol)?;
                info!(
                    "dirty log: {} ranges, {} KiB",
                    table.regions().len(),
                    table.effective_size() >> 10
                );
                dirty = Some(table.regions().iter().map(|r| (r.gpa, r.length)).collect());
                Response::ok()
                    .write_to(&mut stream)
                    .map_err(Error::Protocol)?;
            }
            Command::State => {
                let mut buf = vec![0u8; req.length() as usize];
                stream.read_exact(&mut buf).map_err(Error::ReadPayload)?;
                fs::write(output_dir.join(SNAPSHOT_STATE_FILE), &buf).map_err(Error::WriteFile)?;
                state_bytes = Some(buf);
                Response::ok()
                    .write_to(&mut stream)
                    .map_err(Error::Protocol)?;
            }
            Command::CompletePaused | Command::Complete => {
                // Invariant: drain + fsync every memory fd BEFORE ACKing —
                // CH may exit right after and these fds are our only copy.
                let mm = migration_config
                    .as_ref()
                    .ok_or(Error::PrematureCompletion("Config"))?;
                let _ = state_bytes
                    .as_ref()
                    .ok_or(Error::PrematureCompletion("State"))?;
                dump_memory_slots(&memory_slots, mm, output_dir, compression, dirty.as_deref())?;
                Response::ok()
                    .write_to(&mut stream)
                    .map_err(Error::Protocol)?;
                info!("Snapshot persisted to {output_dir:?}");
                break;
            }
            #[expect(deprecated)] // last sent in v52
            Command::Abandon => {
                // ACK before bailing so CH's ok_or_fatal_error() read returns
                // cleanly instead of hitting EOF.
                Response::ok().write_to(&mut stream).ok();
                return Err(Error::Abandoned);
            }
            c => return Err(Error::UnexpectedCommand(c, "a snapshot command")),
        }
    }

    Ok(())
}

fn expect_command(stream: &mut UnixStream, want: Command, name: &'static str) -> Result<Request> {
    let req = Request::read_from(stream).map_err(Error::Protocol)?;
    if req.command() != want {
        return Err(Error::UnexpectedCommand(req.command(), name));
    }
    Ok(req)
}

fn recv_memory_fd(stream: &UnixStream) -> Result<(u32, File)> {
    let mut buf = [0u8; 4];
    let (_n, file) = stream.recv_with_fd(&mut buf).map_err(Error::RecvMemoryFd)?;
    let file = file.ok_or(Error::MissingMemoryFd)?;
    Ok((u32::from_le_bytes(buf), file))
}

fn dump_memory_slots(
    slots: &[(u32, File)],
    config: &VmMigrationConfig,
    output_dir: &Path,
    compression: Option<&CompressionOptions>,
    dirty: Option<&[(u64, u64)]>,
) -> Result<()> {
    let sizes = slot_sizes(config)?;
    for (expected_slot, _, _) in &sizes {
        if !slots.iter().any(|(s, _)| s == expected_slot) {
            return Err(Error::MissingSlot(*expected_slot));
        }
    }
    if let Some(options) = compression {
        let workers_per_slot = options.workers.div_ceil(slots.len().max(1)).max(1);
        let info = slot_info(config)?;
        let diff_parent = match (&options.parent, dirty, &options.reference_dir) {
            (Some(parent), Some(_), Some(_)) => Some(parent.clone()),
            _ => None,
        };
        thread::scope(|scope| {
            let mut handles = Vec::with_capacity(slots.len());
            for (slot, file) in slots {
                let (gpa, size, file_offset) = info
                    .iter()
                    .find(|(candidate, ..)| candidate == slot)
                    .map(|&(_, gpa, size, file_offset)| (gpa, size, file_offset))
                    .ok_or(Error::MissingSlot(*slot))?;
                let data_path = output_dir.join(format!("memory-{slot}.compressed"));
                let manifest_path = output_dir.join(format!("memory-{slot}.index.json"));
                let diff_parent = diff_parent.as_ref();
                handles.push(scope.spawn(move || {
                    if let (Some(_), Some(dirty), Some(reference_dir)) =
                        (diff_parent, dirty, options.reference_dir.as_ref())
                    {
                        return diff_slot(
                            file,
                            *slot,
                            gpa,
                            size,
                            file_offset,
                            dirty,
                            reference_dir,
                            output_dir,
                            options,
                            workers_per_slot,
                        );
                    }
                    let stats = compress_file(
                        file,
                        file_offset,
                        size,
                        &data_path,
                        &manifest_path,
                        options.codec,
                        options.chunk_size,
                        workers_per_slot,
                        options.zstd_level,
                        options.accel,
                    )?;
                    info!(
                        "Compressed slot {slot}: codec={}, chunks={}, input={} bytes, output={} bytes, ratio={:.3}, throughput={:.3} GiB/s",
                        options.codec,
                        stats.chunks,
                        stats.input_bytes,
                        stats.output_bytes,
                        stats.ratio(),
                        stats.throughput_gib_per_second(),
                    );
                    if let Some(reference_dir) = &options.reference_dir {
                        init_reference_slot(
                            file,
                            *slot,
                            size,
                            file_offset,
                            &manifest_path,
                            reference_dir,
                            options,
                        )?;
                    }
                    Ok::<(), Error>(())
                }));
            }
            for handle in handles {
                handle.join().map_err(|_| Error::WorkerPanic)??;
            }
            Ok::<(), Error>(())
        })?;
        if let Some(parent) = diff_parent {
            let marker = diff::DiffMarker { parent };
            fs::write(diff::marker_path(output_dir), serde_json::to_vec(&marker)?)
                .map_err(Error::WriteFile)?;
        }
        return Ok(());
    }
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(slots.len());
        for (slot, file) in slots {
            let (size, file_offset) = sizes
                .iter()
                .find(|(candidate, _, _)| candidate == slot)
                .map(|(_, size, file_offset)| (*size, *file_offset))
                .ok_or(Error::MissingSlot(*slot))?;
            let path = output_dir.join(memory_slot_filename(*slot));
            handles.push(scope.spawn(move || {
                dump_fd_to_path(file, file_offset, size, &path)?;
                debug!(
                    "dumped {size} bytes from slot {slot} (fd offset {file_offset:#x}) to {path:?}"
                );
                Ok::<(), Error>(())
            }));
        }
        for handle in handles {
            handle.join().map_err(|_| Error::WorkerPanic)??;
        }
        Ok(())
    })
}

fn dump_fd_to_path(file: &File, src_offset: u64, size: u64, path: &Path) -> Result<()> {
    let out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(Error::WriteFile)?;
    out.set_len(size).map_err(Error::WriteFile)?;
    // Keep `out` sparse: holes are left as holes (read back as zero).
    copy_region(file, src_offset, &out, 0, size).map_err(Error::CopyMemory)?;
    out.sync_all().map_err(Error::WriteFile)?;
    Ok(())
}

/// (slot, size, file_offset) per memory slot. `file_offset` is non-zero when a
/// zone spans multiple regions sharing one backing memfd.
fn slot_sizes(config: &VmMigrationConfig) -> Result<Vec<(u32, u64, u64)>> {
    Ok(slot_info(config)?
        .into_iter()
        .map(|(slot, _gpa, size, file_offset)| (slot, size, file_offset))
        .collect())
}

fn slot_info(config: &VmMigrationConfig) -> Result<Vec<(u32, u64, u64, u64)>> {
    parse_guest_ram_mappings(&serde_json::to_value(config.memory_manager_data())?)
}

/// Parse the `guest_ram_mappings` array out of the serialized
/// `MemoryManagerSnapshotData`.
fn parse_guest_ram_mappings(value: &serde_json::Value) -> Result<Vec<(u32, u64, u64, u64)>> {
    let mappings = value
        .get("guest_ram_mappings")
        .and_then(|v| v.as_array())
        .ok_or(Error::MissingField("guest_ram_mappings"))?;
    let mut out = Vec::new();
    for m in mappings {
        let slot = m
            .get("slot")
            .and_then(|v| v.as_u64())
            .ok_or(Error::MissingField("slot"))? as u32;
        let gpa = m
            .get("gpa")
            .and_then(|v| v.as_u64())
            .ok_or(Error::MissingField("gpa"))?;
        let size = m
            .get("size")
            .and_then(|v| v.as_u64())
            .ok_or(Error::MissingField("size"))?;
        let file_offset = m
            .get("file_offset")
            .and_then(|v| v.as_u64())
            .ok_or(Error::MissingField("file_offset"))?;
        // CH allocates one fresh memslot per GuestRegionMmap, so each
        // (slot, gpa, size, file_offset) appears at most once here.
        out.push((slot, gpa, size, file_offset));
    }
    Ok(out)
}

// Restore mode (migration sender).
fn run_restore(
    socket_path: &Path,
    input_dir: &Path,
    resume: bool,
    ondemand: bool,
    options: RestoreOptions,
) -> Result<()> {
    let workers = options.workers;
    let migration_config_bytes =
        fs::read(input_dir.join(MIGRATION_CONFIG_FILENAME)).map_err(Error::ReadFile)?;
    let mut migration_config: VmMigrationConfig = serde_json::from_slice(&migration_config_bytes)?;
    let state_bytes = fs::read(input_dir.join(SNAPSHOT_STATE_FILE)).map_err(Error::ReadFile)?;

    migration_config.set_memory_mode(if ondemand {
        MigrationMode::Postcopy
    } else {
        // Ignored
        MigrationMode::default()
    });
    let migration_config_bytes = serde_json::to_vec(&migration_config)?;

    let mut stream = UnixStream::connect(socket_path).map_err(Error::Connect)?;
    info!("Offload daemon connected to {socket_path:?} (ondemand={ondemand})");

    send_request_expect_ok(&mut stream, Request::start(), "Start")?;

    let mut ondemand_slots: Vec<OnDemandSlot> = Vec::new();
    let slots = slot_info(&migration_config)?;
    // A diff checkpoint restores its full base, then each diff, oldest first.
    let chain = diff::chain(input_dir)?;
    let base_dir = chain
        .last()
        .cloned()
        .unwrap_or_else(|| input_dir.to_path_buf());
    let diffs: Vec<PathBuf> = chain[..chain.len() - 1].iter().rev().cloned().collect();
    if ondemand && !diffs.is_empty() {
        return Err(Error::CompressedOnDemand);
    }
    let (base_dir, diffs) = (&base_dir, &diffs);

    if !ondemand {
        let workers_per_slot = workers.div_ceil(slots.len().max(1)).max(1);
        let memfds = thread::scope(|scope| {
            let mut handles = Vec::with_capacity(slots.len());
            for &(slot, _, size, file_offset) in &slots {
                let disk_path = base_dir.join(memory_slot_filename(slot));
                let compressed_data_path = base_dir.join(format!("memory-{slot}.compressed"));
                let manifest_path = base_dir.join(format!("memory-{slot}.index.json"));
                handles.push(scope.spawn(move || {
                    let compressed = manifest_path.exists();
                    let memfd = create_memfd_with_contents(
                        &disk_path,
                        compressed
                            .then_some((compressed_data_path.as_path(), manifest_path.as_path())),
                        file_offset,
                        size,
                        &format!("offload-slot-{slot}"),
                        RestoreOptions {
                            workers: workers_per_slot,
                            ..options
                        },
                    )?;
                    if !diffs.is_empty() {
                        let started = Instant::now();
                        let mut pages = 0_u64;
                        for dir in diffs {
                            pages += diff::apply_diff(dir, slot, &memfd, file_offset, size)?;
                        }
                        info!(
                            "Applied {} diffs to slot {slot}: {pages} pages in {:.1} ms",
                            diffs.len(),
                            started.elapsed().as_secs_f64() * 1e3
                        );
                    }
                    Ok::<_, Error>((slot, size, file_offset, memfd))
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().map_err(|_| Error::WorkerPanic)?)
                .collect::<Result<Vec<_>>>()
        })?;
        for (slot, size, file_offset, memfd) in memfds {
            send_memory_fd(&mut stream, slot, &memfd)?;
            debug!(
                "restore: sent memory fd for slot {slot} ({size} bytes at fd offset \
                 {file_offset:#x}, ondemand=false)"
            );
        }
    }

    for (slot, gpa, size, file_offset) in slots {
        if !ondemand {
            continue;
        }
        let disk_path = input_dir.join(memory_slot_filename(slot));
        let manifest_path = input_dir.join(format!("memory-{slot}.index.json"));
        let compressed = manifest_path.exists();
        if compressed {
            return Err(Error::CompressedOnDemand);
        }
        let memfd = create_empty_memfd(file_offset + size, &format!("offload-slot-{slot}"))?;
        ondemand_slots.push(OnDemandSlot::new(
            &memfd,
            gpa,
            size,
            file_offset,
            &disk_path,
        )?);
        send_memory_fd(&mut stream, slot, &memfd)?;
        debug!(
            "restore: sent memory fd for slot {slot} ({size} bytes at fd offset \
             {file_offset:#x}, ondemand={ondemand})"
        );
    }

    send_payload_expect_ok(
        &mut stream,
        Request::config(migration_config_bytes.len() as u64),
        &migration_config_bytes,
        "Config",
    )?;

    // For on demand (postcopy) restore the fault connection must be serving before
    // CH processes State, so connect it here and serve on its own thread.
    let serve_handle = if ondemand {
        let slots = Arc::new(ondemand_slots);
        let mut fault_stream = UnixStream::connect(socket_path).map_err(Error::Connect)?;
        ConnectionRole::Fault
            .write_to(&mut fault_stream)
            .map_err(Error::Protocol)?;
        info!(
            "offload daemon: connected dedicated fault connection, serving {} slot(s)",
            slots.len()
        );
        let serve_slots = Arc::clone(&slots);
        let handle = thread::Builder::new()
            .name("offload-fault-serve".to_owned())
            .spawn(move || serve_page_faults(&mut fault_stream, serve_slots.as_slice()))
            .map_err(Error::SpawnServeThread)?;
        Some(handle)
    } else {
        None
    };

    send_payload_expect_ok(
        &mut stream,
        Request::state(state_bytes.len() as u64),
        &state_bytes,
        "State",
    )?;

    let final_req = if resume {
        Request::complete()
    } else {
        Request::complete_paused()
    };
    send_request_expect_ok(&mut stream, final_req, "Complete")?;

    if let Some(handle) = serve_handle {
        info!("Offload daemon: waiting for fault serving to finish");
        handle.join().map_err(|_| Error::ServeThreadPanic)??;
    }

    info!("Restore replay finished");
    Ok(())
}

/// Per-slot state for serving PageFault requests in on demand mode.
struct OnDemandSlot {
    region: GuestRegionMmap,
    disk: File,
}

impl OnDemandSlot {
    fn new(memfd: &File, gpa: u64, size: u64, file_offset: u64, disk_path: &Path) -> Result<Self> {
        // Map the same memfd CH maps, so our page writes are visible to it.
        let fo = FileOffset::new(memfd.try_clone().map_err(Error::CloneMemfd)?, file_offset);
        let mmap = MmapRegion::build(
            Some(fo),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
        )
        .map_err(Error::Mmap)?;
        let region = GuestRegionMmap::new(mmap, GuestAddress(gpa)).ok_or(Error::GuestRegion)?;
        let disk = File::open(disk_path).map_err(Error::ReadFile)?;
        Ok(Self { region, disk })
    }

    fn contains(&self, gpa: u64, len: u64) -> bool {
        let base = self.region.start_addr().raw_value();
        let end = base + self.region.len();
        gpa >= base && gpa.saturating_add(len) <= end
    }
}

fn serve_page_faults(stream: &mut UnixStream, slots: &[OnDemandSlot]) -> Result<()> {
    let mut served: u64 = 0;
    loop {
        let req = match Request::read_from(stream) {
            Ok(r) => r,
            Err(e) => {
                info!("Serve loop: socket closed after {served} PageFault(s) ({e:?})");
                return Ok(());
            }
        };
        match req.command() {
            Command::PageFault => {
                let range = MemoryRange::read_from(stream).map_err(Error::Protocol)?;
                served += 1;
                if served <= 5 || served.is_power_of_two() {
                    info!(
                        "PageFault #{served}: gpa={:#x} len={}",
                        range.gpa, range.length
                    );
                }
                let slot = slots
                    .iter()
                    .find(|s| s.contains(range.gpa, range.length))
                    .ok_or(Error::PageFaultUnmapped(range.gpa, range.length))?;
                let offset = range.gpa - slot.region.start_addr().raw_value();
                // Reading a sparse hole returns zeros, so a single read+write
                // path covers both data and unwritten pages.
                let mut buf = vec![0u8; range.length as usize];
                slot.disk
                    .read_exact_at(&mut buf, offset)
                    .map_err(Error::CopyMemory)?;
                slot.region
                    .write_slice(&buf, MemoryRegionAddress(offset))
                    .map_err(Error::WriteGuestMemory)?;
                Response::ok().write_to(stream).map_err(Error::Protocol)?;
            }
            #[expect(deprecated)] // last sent in v52
            Command::Abandon => {
                info!("Serve loop: received Abandon, exiting");
                Response::ok().write_to(stream).ok();
                return Ok(());
            }
            c => return Err(Error::UnexpectedCommand(c, "a PageFault")),
        }
    }
}

fn create_empty_memfd(size: u64, name: &str) -> Result<File> {
    create_empty_memfd_with(size, name, false)
}

fn create_empty_memfd_with(size: u64, name: &str, hugetlb: bool) -> Result<File> {
    let cname = CString::new(name).map_err(Error::MemfdName)?;
    let flags = if hugetlb {
        libc::MFD_HUGETLB | libc::MFD_HUGE_2MB
    } else {
        0
    };
    // SAFETY: memfd_create has no preconditions. We check the return value.
    let raw = unsafe { libc::memfd_create(cname.as_ptr(), flags) };
    if raw < 0 {
        return Err(Error::MemfdCreate(io::Error::last_os_error()));
    }
    // SAFETY: `raw` is a fresh fd we now own.
    let memfd = unsafe { File::from_raw_fd(raw) };
    memfd.set_len(size).map_err(Error::MemfdSetLen)?;
    Ok(memfd)
}

fn send_request_expect_ok(stream: &mut UnixStream, req: Request, name: &'static str) -> Result<()> {
    req.write_to(stream).map_err(Error::Protocol)?;
    expect_ok_response(stream, name)
}

fn send_payload_expect_ok(
    stream: &mut UnixStream,
    req: Request,
    payload: &[u8],
    name: &'static str,
) -> Result<()> {
    req.write_to(stream).map_err(Error::Protocol)?;
    stream.write_all(payload).map_err(Error::WritePayload)?;
    expect_ok_response(stream, name)
}

fn expect_ok_response(stream: &mut UnixStream, name: &'static str) -> Result<()> {
    let resp = Response::read_from(stream).map_err(Error::Protocol)?;
    if resp.status() != Status::Ok {
        return Err(Error::Rejected(name));
    }
    Ok(())
}

fn send_memory_fd(stream: &mut UnixStream, slot: u32, memfd: &File) -> Result<()> {
    Request::memory_fd(size_of::<u32>() as u64)
        .write_to(stream)
        .map_err(Error::Protocol)?;
    stream
        .send_with_fd(&slot.to_le_bytes()[..], memfd.as_raw_fd())
        .map_err(Error::SendMemoryFd)?;
    expect_ok_response(stream, "MemoryFd")
}

fn create_memfd_with_contents(
    src_path: &Path,
    compressed_paths: Option<(&Path, &Path)>,
    file_offset: u64,
    size: u64,
    name: &str,
    options: RestoreOptions,
) -> Result<File> {
    // Size the memfd to cover the range CH maps at `file_offset`.
    let memfd = create_empty_memfd_with(file_offset + size, name, options.hugetlb)?;
    if let Some((data_path, manifest_path)) = compressed_paths {
        let stats = decompress_file(
            data_path,
            manifest_path,
            &memfd,
            file_offset,
            size,
            options.workers,
            options.verify_crc,
        )?;
        info!(
            "Decompressed {name}: chunks={}, compressed={} bytes, output={} bytes, ratio={:.3}, throughput={:.3} GiB/s",
            stats.chunks,
            stats.output_bytes,
            stats.input_bytes,
            stats.ratio(),
            stats.throughput_gib_per_second(),
        );
        let (bytes, elapsed) = populate_zero_chunks(
            &memfd,
            file_offset,
            size,
            manifest_path,
            options.populate,
            options.dsa_depth,
        )?;
        if options.populate != Populate::None {
            info!(
                "Populated {name}: {bytes} zero bytes via {} in {:.1} ms",
                options.populate,
                elapsed.as_secs_f64() * 1e3
            );
        }
    } else {
        let src = File::open(src_path).map_err(Error::ReadFile)?;
        // Copy sparsely so the memfd keeps the snapshot's holes.
        copy_region(&src, 0, &memfd, file_offset, size).map_err(Error::CopyMemory)?;
        if options.populate != Populate::None {
            // No manifest to tell holes from data: populate the whole slot.
            let mapping =
                Mapping::map_file(&memfd, file_offset, size, true).map_err(Error::CopyMemory)?;
            let elapsed = mapping.populate(true).map_err(Error::CopyMemory)?;
            info!(
                "Populated {name}: whole slot via MADV_POPULATE_WRITE in {:.1} ms",
                elapsed.as_secs_f64() * 1e3
            );
        }
    }
    Ok(memfd)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_memory_slot_filename() {
        assert_eq!(memory_slot_filename(0), "memory-0");
        assert_eq!(memory_slot_filename(13), "memory-13");
    }

    #[test]
    fn test_parse_guest_ram_mappings() {
        let value = json!({
            "guest_ram_mappings": [
                { "slot": 0, "gpa": 0u64, "size": 4096u64, "file_offset": 0u64, "virtio_mem": true },
                { "slot": 1, "gpa": 0x4000u64, "size": 8192u64, "file_offset": 4096u64 },
            ]
        });
        assert_eq!(
            parse_guest_ram_mappings(&value).unwrap(),
            vec![(0, 0, 4096, 0), (1, 0x4000, 8192, 4096)]
        );
    }

    #[test]
    fn test_parse_guest_ram_mappings_missing_array() {
        let value = json!({});
        assert!(matches!(
            parse_guest_ram_mappings(&value),
            Err(Error::MissingField("guest_ram_mappings"))
        ));
    }

    #[test]
    fn test_parse_guest_ram_mappings_missing_field() {
        let value = json!({ "guest_ram_mappings": [{ "slot": 0, "gpa": 0u64, "size": 4096u64 }] });
        assert!(matches!(
            parse_guest_ram_mappings(&value),
            Err(Error::MissingField("file_offset"))
        ));
    }
}

#[cfg(feature = "qpl")]
#[expect(clippy::too_many_arguments)]
fn diff_slot(
    file: &File,
    slot: u32,
    gpa: u64,
    size: u64,
    file_offset: u64,
    dirty: &[(u64, u64)],
    reference_dir: &Path,
    output_dir: &Path,
    options: &CompressionOptions,
    workers: usize,
) -> Result<()> {
    let huffman = options.codec.async_huffman_mode().ok_or(Error::DiffCodec)?;
    let ranges = diff::slot_dirty_ranges(dirty, gpa, size);
    let stats = diff::diff_snapshot_slot(
        file,
        file_offset,
        size,
        &diff::reference_path(reference_dir, slot),
        &ranges,
        output_dir,
        slot,
        options.codec,
        huffman,
        options.chunk_size,
        workers,
        options.diff,
    )?;
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    info!(
        "Diff slot {slot}: compare={} dirty_pages={} changed_pages={} prepare_ms={:.1} compare_ms={:.1} gather_ms={:.1} compress_ms={:.1} output_bytes={} dsa_ops={} dsa_cpu_redo={}",
        options.diff.compare,
        stats.dirty_pages,
        stats.changed_pages,
        ms(stats.prepare),
        ms(stats.compare),
        ms(stats.gather),
        ms(stats.compress),
        stats.output_bytes,
        stats.dsa_ops,
        stats.dsa_cpu_redo,
    );
    Ok(())
}

#[cfg(not(feature = "qpl"))]
#[expect(clippy::too_many_arguments)]
fn diff_slot(
    _: &File,
    _: u32,
    _: u64,
    _: u64,
    _: u64,
    _: &[(u64, u64)],
    _: &Path,
    _: &Path,
    _: &CompressionOptions,
    _: usize,
) -> Result<()> {
    Err(Error::DiffCodec)
}

#[cfg(feature = "qpl")]
fn init_reference_slot(
    file: &File,
    slot: u32,
    size: u64,
    file_offset: u64,
    manifest_path: &Path,
    reference_dir: &Path,
    options: &CompressionOptions,
) -> Result<()> {
    let manifest: compression::SlotManifest =
        serde_json::from_slice(&fs::read(manifest_path).map_err(Error::ReadFile)?)?;
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
    fs::create_dir_all(reference_dir).map_err(Error::WriteFile)?;
    let elapsed = diff::init_reference(
        file,
        file_offset,
        size,
        &diff::reference_path(reference_dir, slot),
        &nonzero,
        options.diff.compare == DiffCompare::Dsa,
    )?;
    info!(
        "Reference slot {slot}: {} non-zero chunks copied in {:.1} ms",
        nonzero.len(),
        elapsed.as_secs_f64() * 1e3
    );
    Ok(())
}

#[cfg(not(feature = "qpl"))]
fn init_reference_slot(
    _: &File,
    _: u32,
    _: u64,
    _: u64,
    _: &Path,
    _: &Path,
    _: &CompressionOptions,
) -> Result<()> {
    Err(Error::DiffCodec)
}
