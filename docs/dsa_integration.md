# DSA integration for the offload daemon (design)

Branch: `dsa-integration` (from `iaa-integration` @ a84016250). Status: design,
no code yet. Companion: `docs/snapshot_restore.md` ("Offload Snapshot and
Restore", "Compression evaluation") describes the IAA side this builds on.

## 1. Starting points

**What `iaa-integration` has.** All snapshot/restore work lives in
`offload_daemon/`, a live-migration peer of the VMM (`vm.send-migration` /
`vm.receive-migration`, `memory_mode=memfds`). Per memory slot the daemon
either dumps the memfd to a sparse file (`vmm::sparse::copy_region`) or, with
`--compression`, cuts it into `--chunk-size` (1 MiB) chunks and compresses each
one. The QPL path is layered as:

| layer | file | role |
|---|---|---|
| C shim | `offload_daemon/src/qpl_shim.c` | one `qpl_job` per slot: create / submit_compress / check / wait / destroy |
| Rust FFI + pool | `offload_daemon/src/qpl.rs` | `Job`, `JobPool` (bounded in-flight slots, reused in/out buffers, `QUEUES_ARE_BUSY` retry, 60 s bound) |
| pipeline | `offload_daemon/src/qpl_pipeline.rs` | `compress_files` / `decompress_files`: ring over the pool, `FileMapping` (mmap) source, `is_zero` byte scan, manifest records |
| codec plumbing | `offload_daemon/src/compression.rs` | `Codec`, `ChunkRecord { uncompressed_offset/len, compressed_offset/len, zero }`, `SlotManifest`, sync path |
| build | `offload_daemon/build.rs`, feature `qpl` | `cc::Build` of the shim, static `libqpl.a`, `QPL_INCLUDE_DIR` / `QPL_LIB_DIR` |

All-zero chunks already get a manifest record and no payload; eager restore
leaves them as holes in the fresh memfd. That record is the hook DSA plugs into.

**What `djb-dsa-iaa` has (kept local).** The same chain implemented inside the
VMM (`vmm/src/dsa.rs`, `vmm/src/iaa.rs`) with direct ENQCMD on shared user WQs:
DSA COMPARE-against-zero classify at 64 KiB, DSA MEMMOVE gather of the kept
ranges, IAA deflate, and on restore IAA inflate + DSA MEMMOVE raw copy + DSA
MEMFILL of the elided ranges. Measured on a 4 GiB guest (4 DSA WQs, node 0):

| stage | CPU | DSA | note |
|---|---|---|---|
| classify 4 GiB | — | 37 ms, 65 % of the per-snapshot chain | device-bound, independent of how much is zero |
| raw move ~475 MiB (restore) | 15.1 ms | 2.3 ms | `CH_RESTORE_RAW=dsa` |
| fill 3.1 GiB of elided ranges (restore) | 0.36 s | 16 ms | `CH_RESTORE_FILL=dsa` |
| real Terminal-Bench agent snapshot | | 57 ms total, 0.05 core-s | 475 MiB kept → 209 MiB |

Those numbers are the targets; the daemon pays the same costs in a different
process.

**DTO** (`~/DTO`, `libdto.so` installed in `/usr/lib64` and `/usr/local/lib`)
is Intel's DSA transparent-offload library. Besides interposing
`memcpy/memmove/memset/memcmp` it exports an explicit API in `dto.h`:

```c
typedef struct dto_async_op { unsigned char opaque[192] __attribute__((aligned(64))); } dto_async_op;
int      dto_submit_memcpy_crc(dto_async_op *op, void *dest, const void *src, size_t n, int cache_control);
int      dto_submit_crc(dto_async_op *op, const void *src, size_t n);
int      dto_async_poll(dto_async_op *op);           /* PENDING 0 / DONE 1 / FAILED -1 */
uint64_t dto_async_crc_val(const dto_async_op *op);
void     dto_memset_pages(void *start, void *end, size_t page_size);
void     dto_batch_copy(void **dst, void **src, size_t *sizes, int count, void (*cb)(void *), void *arg);
void     dto_memcpy_async(void *dest, const void *src, size_t n, callback_t cb, void *args);
```

Submit returns `DTO_ASYNC_SUBMITTED` or `DTO_ASYNC_FALLBACK` (no WQ, below
`DTO_MIN_BYTES`, thread disabled). Internally `dto_submit_async_common()` picks
a WQ by the NUMA node of the buffer (`DTO_IS_NUMA_AWARE`), sets
`CRAV|RCR|BOF`, and ENQCMDs; `dto_async_poll` reads the completion record.
That is the same shape as `qpl.rs`'s submit/check, so a `dto.rs` can mirror it.

## 2. Where DSA pays in the daemon

| stage | today | DSA op | DTO entry point | expected |
|---|---|---|---|---|
| **zero classify** per chunk | `FileMapping::is_zero` byte loop on the worker thread (`qpl_pipeline.rs:77`), synchronous before each IAA submit | COMPARE chunk vs. a resident zero buffer | **missing in `dto.h`** (only the `memcmp` interposer, synchronous); add `dto_submit_compare` (§4) | ~37 ms / 4 GiB, no CPU; today the 4 GiB scan is all CPU and sits in the IAA submit path |
| **gather / copy** of kept chunks into the IAA input | none needed: IAA reads the mmap'd source directly (`submit_compress_mapped_input`) | — | — | nothing to do while the chunk is the compression unit |
| **raw snapshot** (no `--compression`) | `copy_region` memfd → sparse file via syscalls | MEMMOVE | `dto_batch_copy` / `dto_submit_memcpy_crc` on an mmap'd destination | 15 → 2 ms per 475 MiB (in-VMM figure); needs mmap on both sides |
| **raw restore** | `copy_region` file → memfd | MEMMOVE | same | same |
| **restore fill** of zero chunks | holes left in the memfd; guest pays first-touch later | MEMFILL | `dto_memset_pages(start, end, 4096)` on the mapped memfd | 3.1 GiB in 16 ms vs 0.36 s; only when the restore wants populated memory (`--populate`, see §6) |
| **integrity** | none | CRC32 as a side effect of COPY_CRC / CRCGEN | `dto_submit_crc`, `dto_async_crc_val` | free per-chunk checksum in the manifest; IAA also returns CRC32 in `qpl_job` |

The classify row is the one that matters for snapshot latency: in the in-VMM
chain it is 65 % of the per-snapshot time, and in the daemon it also serialises
with IAA submission on the same worker thread.

## 3. Chunk granularity: 1 MiB is fine

The in-VMM chain classifies at 64 KiB because sub-2 MiB DSA COMPAREs keep all
engines busy. The daemon's compression unit is 1 MiB. Measured on a raw 4 GiB
dump of the synthetic warm guest (`/mnt/nvme/chrest/base/memory-ranges`):

| granule | kept | elided |
|---|---|---|
| 4 KiB | 919 MiB | 77.6 % |
| 64 KiB | 927 MiB | 77.4 % |
| 1 MiB | 943 MiB | 77.0 % |
| 2 MiB | 952 MiB | 76.8 % |

Zero pages cluster, so coarser classify costs under 1 % of elision. One DSA
COMPARE descriptor per 1 MiB chunk is therefore enough; no sub-chunk zero mask,
no gather stage, no manifest change. (Real agent guests are 85–90 % zero at
64 KiB; the same flatness is expected but should be re-measured once a raw dump
of one exists.)

## 4. Design

### 4.1 `dto.rs`: a DSA pool mirroring `qpl.rs`

No C shim is required: `dto_async_op` is an opaque 192-byte, 64-byte-aligned
blob, so Rust declares `#[repr(C, align(64))] struct DtoAsyncOp([u8; 192])` and
calls `dto_*` directly through `extern "C"`.

```rust
pub(crate) struct DsaPool { ops: Vec<DtoAsyncOp>, zero: ZeroBuffer /* chunk_size, page-aligned */ }
impl DsaPool {
    fn new(depth: usize, chunk_size: usize) -> Result<Self, Error>;
    fn capacity(&self) -> usize;
    /// COMPARE chunk against the zero buffer. Ok(true) = submitted; Ok(false) = DTO fell back (caller scans on CPU).
    fn submit_is_zero(&mut self, index: usize, chunk: *const u8, len: usize) -> Result<bool, Error>;
    /// None = pending; Some(true) = all zero; Some(false) = differs.
    fn poll(&mut self, index: usize) -> Result<Option<bool>, Error>;
}
```

`submit_is_zero` wraps `dto_submit_compare` (§4.4). On `DTO_ASYNC_FALLBACK` the
caller runs the existing byte loop, so behaviour without a usable WQ is exactly
today's. `poll` maps `DTO_ASYNC_DONE` to the completion record's result
(`comp.result == 0` → equal → zero chunk), `DTO_ASYNC_FAILED` to an error that
again falls back to the byte loop rather than failing the snapshot.

### 4.2 Pipeline: a two-stage ring in `qpl_pipeline.rs`

Today `submit_compression_job` runs `is_zero` inline and then submits IAA. The
change is to make classify its own asynchronous stage feeding the IAA ring:

```
chunk cursor ──► DsaPool (depth D) ──► zero?  ──yes──► ChunkRecord{zero:true}
                                        │
                                        no
                                        ▼
                                  JobPool (workers W) ──► output file + ChunkRecord
```

- The DSA stage runs ahead of the IAA stage by up to `D` chunks; a completed
  COMPARE that reports "differs" moves the chunk into the first free IAA slot,
  "equal" writes the zero record immediately.
- Ordering is unchanged: records are collected and sorted by
  `uncompressed_offset` at the end, as now.
- `next_completion` already round-robins the IAA pool; the DSA pool gets the
  same cursor treatment. Both are polled in one loop so the worker thread
  never blocks on either engine.
- Memory: the zero buffer is one chunk (1 MiB) per pool; no copies are added.
- The source is a `FileMapping` of the memfd, which is fully populated when a
  running VM is snapshotted. DTO sets `BOF`, so a hole (restore of a sparse
  dump, or an unbacked memfd page) is handled by the kernel fault path instead
  of a `PAGE_FAULT_NOBOF` error.

Depth: one COMPARE at 1 MiB takes ~5–7 µs on one engine; with `D = 32` per
pool the DSA stage stays ahead of `W = 8` IAA jobs that each take ~100 µs per
MiB. Expose `--dsa-depth` (default 32) next to `--workers`.

Sync codecs (`lz4`, `zstd`, `qpl-hardware-*` non-async in `compression.rs`)
keep the byte loop; DSA classify is wired into the async path only, which is
the one the benchmark harness exercises.

### 4.3 Build and configuration

- `offload_daemon/Cargo.toml`: feature `dto = []`, independent of `qpl` so the
  DSA stage can be benchmarked with the LZ4/Zstd codecs too.
- `build.rs`: with `CARGO_FEATURE_DTO`, `cargo:rustc-link-search` on
  `DTO_LIB_DIR` (default `/usr/lib64`) and `cargo:rustc-link-lib=dylib=dto`,
  plus `numa` and `accel-config` (DTO's own dependencies). DTO is linked
  dynamically because it is an LD_PRELOAD-style interposer that is normally a
  shared object; a static build would need its `make libdto` changed.
- Runtime: `DTO_WQ_LIST=wq0.0,wq2.0,...` (shared user WQs, as `accelConfig.sh`
  creates them), `DTO_WAIT_METHOD=umwait` (MSR 0x123 must be 0; `busypoll`
  otherwise), `DTO_IS_NUMA_AWARE=1`, WQs on the same node as the IAA WQs.
- **Interposition hazard.** Linking `libdto.so` makes every
  `memcpy/memmove/memset/memcmp` ≥ `DTO_MIN_BYTES` (default 32 KiB) in the
  daemon process go to DSA synchronously, including QPL's internal copies and
  Rust `Vec` moves. Until measured, run with `DTO_DSA_MEMCPY=0 DTO_DSA_MEMMOVE=0
  DTO_DSA_MEMSET=0 DTO_DSA_MEMCMP=0` so only the explicit `dto_*` calls use the
  device; the daemon should set these itself with `setenv` before the first DTO
  call (DTO reads them in its constructor, so this needs a check that the
  constructor has not already run, or a tiny `dto_init()` hook, §4.4).

### 4.4 Changes needed in DTO

1. **`dto_submit_compare(dto_async_op *op, const void *a, const void *b, size_t n)`**:
   `dto_submit_async_common(op, DSA_OPCODE_COMPARE, (void *)b, a, n, 0)` with
   `src2_addr` set; ~20 lines. `dto_async_poll` already returns DONE/FAILED;
   add `int dto_async_compare_result(const dto_async_op *op)` returning
   `comp.result` (0 = equal). A `dto_submit_compare_val` (COMPARE_VAL opcode,
   pattern 0) would avoid the zero buffer but DSA 1.0 lacks it; keep COMPARE.
2. **Fallback status**: `dto_async_poll` logs an error and returns FAILED on
   `PAGE_FAULT_NOBOF` and partial completions; for classify a partial
   completion (bytes_completed < n) should be reported so the caller finishes
   on CPU. Expose `bytes_completed`.
3. Optional: `dto_submit_memset(op, dst, c, n)` so restore fill is async too
   (`dto_memset_pages` is synchronous, one descriptor per page range).
4. Optional: a `dto_set_interpose(mask)` or respect of the `DTO_DSA_*` knobs
   after init, for §4.3.

These go in a fork of `~/DTO` (branch `ch-async-compare`) until upstreamed; the
daemon's `build.rs` points `DTO_INCLUDE_DIR`/`DTO_LIB_DIR` at it.

## 5. Snapshot path summary (what changes, what does not)

| | `iaa-integration` | after `dsa-integration` phase 1 |
|---|---|---|
| classify | CPU byte loop per chunk, on the IAA submit thread | DSA COMPARE, async ring ahead of IAA |
| compression unit | 1 MiB chunk | unchanged |
| manifest | `ChunkRecord{…, zero}` | unchanged (optional `crc32` later) |
| output file | compressed chunks, zero chunks absent | unchanged |
| restore | unchanged | unchanged |
| build | feature `qpl` | feature `qpl` + `dto` |

Compatibility: snapshots written by either branch restore on the other.

## 6. Restore side (phase 3)

Eager restore (`create_memfd_with_contents`) decompresses with IAA into the
memfd and leaves zero chunks as holes. Two DSA additions, both optional flags:

- `--populate`: after the memfd is filled, `dto_memset_pages` over every
  zero-record range so the guest never faults them in. In-VMM: 3.1 GiB in
  16 ms vs 0.36 s with `MADV_POPULATE_WRITE`. Useful when restore latency to
  first useful instruction matters more than host memory (agent sandboxes).
- Raw (uncompressed) restore and snapshot: replace the `copy_region` syscall
  loop with `dto_batch_copy` over mmap'd source and destination, honouring the
  sparse holes (`SEEK_DATA`/`SEEK_HOLE` walk, DSA copies only data extents).
  Expected 15 → 2 ms per ~475 MiB.

`--ondemand` restore is unaffected; UFFDIO_COPY of 4 KiB pages is below any
sensible offload threshold in user space (the kernel-side offload is a separate
project).

## 7. Phases and validation

| phase | content | cell to run | pass criterion |
|---|---|---|---|
| 1 | DTO fork + `dto.rs` + two-stage ring, feature `dto` | `benchmark/` harness, `qpl-hardware-static-async`, 4 GiB guest, `--workers 8`, `--dsa-depth 32`, A/B feature on/off | same output bytes and manifest; snapshot wall ≤ today; worker CPU (`perf stat` of the daemon) down by the `is_zero` share |
| 2 | per-chunk CRC32 from `dto_submit_crc` or IAA `crc32`, manifest `crc32: Option<u32>` | restore with a deliberately corrupted chunk | restore refuses the chunk |
| 3 | `--populate` MEMFILL, raw copy via `dto_batch_copy` | restore-latency cell: time to guest console marker | populate ≥ 10× faster than MADV_POPULATE_WRITE on the same guest |

Before phase 1 lands, one measurement is worth taking on this branch as-is:
`perf record` of the daemon during a 4 GiB `qpl-hardware-static-async`
snapshot, to size the `is_zero` share of CPU and of wall time. The in-VMM
figure (65 % of the chain) is for a DSA classify competing with IAA on memory
bandwidth; the CPU loop's share will differ.

## 8. Open questions for the `iaa-integration` authors

- Chunk size: 1 MiB is kept (§3). If a smaller chunk is ever wanted for IAA
  reasons, DSA classify still works per chunk down to `DTO_MIN_BYTES`.
- Do they want DSA classify also in the sync codec paths (`compress_file` in
  `compression.rs`)? It is a drop-in there too but adds a pool per worker.
- Static vs dynamic DTO linking in release builds; whether a DTO fork is
  acceptable or the compare op should go upstream first.
- Whether the interposed `memcpy/memset` in the daemon are wanted at all. Our
  in-VMM experience is that transparent offload of small copies loses; explicit
  calls only is the recommended default.
