# DSA integration for the offload daemon (design)

Branch: `dsa-integration` (from `iaa-integration` @ a84016250). Status:
**implemented and measured** (§7c); §4 below is the design as built, with the
deviations noted inline. Companion: `docs/snapshot_restore.md` ("Offload Snapshot and
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
| live async path | `offload_daemon/src/compression.rs` | `compress_file_qpl_async` → `submit_next_compression_chunk`: **pread** each chunk into the pool's input buffer, `input.iter().all(\|b\| *b == 0)` byte scan, `submit_compress`; `dump_memory_slots` in `main.rs` calls `compress_file` with one thread per slot |
| mmap pipeline | `offload_daemon/src/qpl_pipeline.rs` | `compress_files` / `decompress_files` over a `FileMapping` (mmap) source with `is_zero` and `submit_compress_mapped_input`. **No callers outside its own tests** on this branch; the live path is the pread one above |
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
| **zero classify** per chunk | `input.iter().all(..)` byte loop in `submit_next_compression_chunk`, on the one thread per slot, synchronous before each IAA submit; measured **75–85 % of daemon CPU and the whole wall** (§7a) | COMPARE chunk vs. a resident zero buffer | **missing in `dto.h`** (only the `memcmp` interposer, synchronous); add `dto_submit_compare` (§4) | ~37 ms / 4 GiB, no CPU; today the 4 GiB scan is all CPU and sits in the IAA submit path |
| **source read** of every chunk | `read_exact_at` (pread) of each 1 MiB chunk into the pool buffer: a kernel copy of the whole guest (`hugetlbfs_read_iter`/`_copy_to_iter`, 12 % of daemon CPU, §7a) | none: map the memfd instead | `FileMapping` + `submit_compress_mapped_input` already exist in `qpl_pipeline.rs` | removes the 4 GiB copy; DSA needs the source mapped anyway |
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
- The source becomes a `FileMapping` of the slot memfd (the live path preads
  today, §7a; DSA needs a mapped source, and IAA can take the mapped input via
  `submit_compress_mapped_input`, so the pread copy goes away in the same
  change). The memfd is fully populated when a running VM is snapshotted. DTO sets `BOF`, so a hole (restore of a sparse
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

## 7a. Measured baseline on `iaa-integration` (2026-10-01, gnr-qual1)

4 GiB guest (`--memory size=4G,hugepages=on,hugepage_size=2M,shared=on`, 2
vCPUs, node 0), paused at `AGENT_BASE_POINT`, snapshotted through the daemon
with `send-migration ... memory_mode=memfds,preserve_source=on`. Two guests:
*warm* (synthetic working set, 72 % of 1 MiB chunks all-zero: 2911/3072 in the
3 GiB slot, 42/1024 in the 1 GiB slot) and *idle* (fresh boot, ~90 % zero).
`--chunk-size 1048576 --workers 8` (4 in-flight jobs per slot). Daemon measured
with `/usr/bin/time`, then separately under `perf record --call-graph dwarf`
on a debuginfo build (`CARGO_PROFILE_RELEASE_DEBUG=1 ..._STRIP=false`).
Driver: `~/chlogs/perf_daemon.sh`; results `~/chlogs/perfdaemon_{all,idle}_20261001_13*`.

| cell | migration wall | daemon user | daemon sys | daemon CPU | output |
|---|---|---|---|---|---|
| qpl-hardware-static-async, warm | 1.16 s | 0.99 s | 0.36 s | 1.35 s | 455 MiB |
| qpl-hardware-static-async, idle | 1.20 s | 1.29 s | 0.20 s | 1.49 s | 161 MiB |
| lz4 w8, warm | 0.71 s | 2.62 s | 0.68 s | 3.30 s | 466 MiB |
| raw (sparse copy), warm | 2.18 s | 0.00 s | 1.53 s | 1.53 s | 4097 MiB |

Where the QPL daemon's cycles go (perf, self time):

| symbol | warm | idle | what it is |
|---|---|---|---|
| `compression::submit_next_compression_chunk` (the `iter().all()` loop, inlined) | 75.1 % | 85.4 % | zero scan |
| `_copy_to_iter` + `hugetlbfs_read_iter` (pread of the memfd) | 8.2 % (12.3 % incl. children) | 4.6 % | source copy |
| `iov_iter_zero` | 3.3 % | 5.0 % | pread of unbacked (hole) pages |
| `copy_folio_from_iter_atomic` (pwrite of output) | 4.0 % | 1.1 % | output write |
| QPL submit/poll, everything else | < 2 % | < 2 % | |

Findings:

1. **The zero scan is the snapshot.** Wall is flat at ~1.2 s whether the guest
   holds 455 MiB or 161 MiB of compressible data, because the 3 GiB slot's
   single thread scans ~2.8 GiB of zero chunks at ~2.6 GiB/s before anything
   reaches IAA. The 1 GiB slot (mostly non-zero, early exit) finished at
   2.5 GiB/s; both slots report 2.5–2.7 "GiB/s" because the scan sets the pace.
2. **The loop is byte-at-a-time.** Disassembly of the hot address:
   `cmpb $0,(%rbp,%rax); lea 1(%rax),%rax; je` — the workspace
   `[profile.release] opt-level = "s"` keeps the iterator loop scalar, and
   `all()`'s short-circuit prevents the autovectoriser from widening it. A
   word-wide or SIMD scan (compare 32–64 B per iteration) would run ~10×
   faster on CPU; that, not today's loop, is the baseline DSA classify must
   beat on wall. On CPU time DSA wins regardless (37 ms of device time vs
   ~0.3 s of a core at 10 GiB/s).
3. **The source is copied, not mapped.** Every chunk is pread into a pool
   buffer: a full 4 GiB kernel copy (12 % of CPU, more for hugetlb). The mmap
   variant exists (`qpl_pipeline.rs`) but is unused. Mapping the memfd is a
   prerequisite for DSA and removes this copy for IAA too.
4. **LZ4 beats IAA on wall today** (0.71 vs 1.16 s) only because its 8 worker
   threads split the scan 8 ways; it costs 2.4× the CPU. With the scan off the
   critical path the IAA cell should drop to the 1 GiB slot's IAA time
   (~0.4 s at 4 in flight) or below with the worker budget rebalanced.
5. **Raw path**: 2.2 s and 1.5 s of sys for a 4 GiB sparse copy
   (`copy_folio_from_iter_atomic` 31 %, `_copy_to_iter` 11 %): the DSA
   MEMMOVE row in §2 applies.

Prediction for phase 1 on this cell: snapshot wall 1.16 → ~0.45 s, daemon CPU
1.35 → ~0.3 s (IAA submit/poll + output writes), identical output and manifest.

## 7b. Same guest through `djb-dsa-iaa` (in-VMM chain), for scale

Same warm guest and base point, `chsnap3.sh fullziaa` with 64 KiB classify,
`CH_IAA_ASYNC=128`, spin wait, STREAM, the same 4 node-0 DSA WQs; their daemon
rerun with output on the same tmpfs (`/mnt/chsnap`) so storage is equal.

| | `iaa-integration` daemon, qpl-hardware-static-async w8 | `djb-dsa-iaa`, 1st snapshot of the process | `djb-dsa-iaa`, 2nd snapshot |
|---|---|---|---|
| snapshot wall | 1.12 s | 0.52 s | **0.18 s** |
| CPU | 1.35 core-s | 0.50 core-s | **0.15 core-s** |
| classify | CPU byte scan, ~1.0 core-s, serial per slot | DSA COMPARE 341 ms (336 ms waiting on first-touch IOMMU faults) | DSA COMPARE **32 ms** |
| kept for compression | 1143 MiB (1 MiB chunks) | 1076 MiB (64 KiB chunks) | same |
| compressed output | 441 MiB (0.39 of kept) | 437 MiB (0.397) | same |
| source access | pread copy of the memfd | guest memory mapped in-process | same |
| raw (uncompressed) path | 0.60 s / 0.80 s sys, sparse copy skips holes | 0.84 s / 0.82 core-s `write()` of all 4 GiB | same |

The 6× wall and 9× CPU gap in steady state is the zero scan plus the pread
copy, which is what §4 removes. Two things the daemon architecture adds that
the in-VMM chain does not pay:

- **Cold IOMMU translations.** Our first snapshot spent 336 ms blocked on DSA
  page faults (shared-virtual-addressing first touch of 4 GiB of 2 MiB
  hugetlb, ~2048 faults). The daemon is a fresh process per snapshot, so it
  pays this every time unless it stays resident; and their default guest is
  **not hugepage-backed**, so a 4 GiB memfd is ~1 M 4 KiB faults, not 2048.
  Phase 1 must measure DSA classify on a 4 KiB-backed memfd from a fresh
  process before the 0.45 s prediction is trusted; mitigations are a resident
  daemon, hugepage-backed guests, or `MADV_POPULATE_READ` on the mapping
  (CPU-cheap: it only walks page tables, no data touch) before submitting.
- **Sparse raw copy is better than ours**: their raw path skips holes and
  beat our dense `write()` (0.60 vs 0.84 s). The DSA MEMMOVE row in §2 should
  keep that hole-skipping.

## 7c. Implemented: results (2026-10-01)

What landed on this branch (daemon side) and on DTO branch `ch-async-ops`
(`~/DTO`, commit 9b5eb29):

| piece | where | note |
|---|---|---|
| DTO async COMPARE / MEMFILL / Translation Fetch + result accessors | `DTO/dto.c`, `dto.h` | `dto_submit_compare`, `dto_submit_memfill`, `dto_submit_transl_fetch`, `dto_async_result/bytes_completed/status`; prepare/enqueue split out of the CRC path |
| `libdto_explicit` | `DTO/CMakeLists.txt` (`DTO_BUILD_EXPLICIT`) | same code, no libc interposers; the daemon links this |
| `dto-async-test` | DTO tree | correctness of the new ops, CRC convention, first-touch experiment |
| `dto.rs` | daemon | Rust FFI over `dto_async_op`, `Submit`/`Poll`, zero buffer, counters |
| `mapping.rs` | daemon | slot memfd mapped (read for snapshot, write for restore), `MADV_POPULATE_READ/WRITE` |
| `crc.rs` | daemon | word-wide `is_zero`, software CRC32C in the DSA convention (seed 0, no final xor, SSE4.2) |
| `classify.rs` | daemon | stage-one ring: COMPARE (+CRC) per chunk, CPU fallback per op |
| `compression.rs` | daemon | async path rewritten as the two-stage ring; mapped input to IAA (`JobPool::submit_compress_from`); `ChunkRecord.crc32c`; restore writes into the destination mapping; `ChunkVerifier` (DSA CRC on restore); `populate_zero_chunks` |
| CLI | `main.rs` | snapshot `--classify cpu\|dsa --dsa-depth N --crc --no-prefault`; restore `--verify-crc --populate none\|cpu\|dsa --hugetlb --dsa-depth N` |
| build | feature `dto`, `DTO_SRC_DIR` or `DTO_LIB_DIR` | `DTO_SRC_DIR` = a checkout of https://github.com/byrnedj/DTO `ch-async-ops` (53d3732): cmake-builds `libdto_explicit` into OUT_DIR and links it with an rpath; `DTO_LIB_DIR` takes a prebuilt one |

Deviations from §4: no C shim and no `qpl_pipeline.rs` reuse (its `FileMapping`
became `mapping.rs`); CRC is issued together with COMPARE for every chunk
rather than only for kept chunks (one extra cheap descriptor, no second
stage); Translation Fetch is implemented in DTO but not used by the daemon
(§7c.3).

### 7c.1 Snapshot, same warm 4 GiB guest as §7a/§7b (tmpfs output)

`qpl-hardware-static-async --chunk-size 1 MiB`, 2 slot threads, DTO on the 4
node-0 DSA WQs (`DTO_WQ_LIST`, `DTO_IS_NUMA_AWARE=1`). `w` = IAA jobs in
flight across the two slots.

| cell | wall | daemon CPU | notes |
|---|---|---|---|
| baseline `iaa-integration` (pread + byte scan), w8 | 1120 ms | 1.35 core-s | §7a |
| mmap + word scan (`--classify cpu`), w8 | 331–492 ms | 0.52–0.63 core-s | phase 0 alone: ~2.5× |
| `--classify dsa`, w8 | 214 ms | 0.30 core-s | classify no longer on the critical path; IAA at 4 jobs/slot is |
| `--classify dsa --crc`, w8 | 228 ms | 0.31 core-s | CRC adds ~14 ms (6144 extra descriptors) |
| `--classify dsa --no-prefault`, w8 | 1357 ms | 1.95 core-s | device page-request faults on first touch; the §7c.3 cost |
| `--classify dsa`, w16 | 149 ms | 0.19 core-s | |
| **`--classify dsa`, w32** | **115–121 ms** | **0.20 core-s** | best; 9.5× wall, 6.7× CPU vs baseline |
| `--classify dsa`, w64 / w128 | 137–154 ms | 0.22–0.24 core-s | more jobs than the IAA WQs take without `QUEUES_ARE_BUSY` retries |
| `--classify cpu`, w32 | 221 ms | 0.31 core-s | the fair CPU comparison at the same IAA depth |

Output and manifests are byte-identical between the cpu and dsa classify
paths (2990/4096 chunks zero, same records), and the DSA CRC32C of every
kept chunk equals the software value. `dsa submitted=3072 fallback=0
failed=0` on every run.

Against the in-VMM chain (§7b, 0.18 s / 0.15 core-s steady state): the daemon
at w32 is now **faster on wall** (0.12 s) at 0.20 core-s. The in-VMM chain's
remaining advantage is the 0.05 core-s, mostly the CRC-less 64 KiB classify
batches and in-process IAA output.

### 7c.2 Restore: verify and populate

Restore of the `--crc` snapshot into a fresh VM (`receive-migration`),
guest resumed and checked to progress (`AGENT_DIFF_POINT` reached) in every
passing cell. Verification runs the CRC on DSA (`dsa submitted=1106
fallback=0`); a snapshot with one manifest CRC bit flipped is refused
(`CrcMismatch` at the right offset). A flipped payload byte was already
refused by the length check (deflate stops early).

| restore cell | memfd pages | wall | populate step |
|---|---|---|---|
| `--verify-crc` | 4 KiB | 346–670 ms | holes left |
| `--verify-crc --populate cpu` | 4 KiB | 1161 ms | 959 ms `MADV_POPULATE_WRITE` for 3.1 GiB |
| `--verify-crc --populate dsa` | 4 KiB | 2653–2997 ms | **2295 ms** MEMFILL: every 4 KiB page is a device fault |
| `--verify-crc --hugetlb` | 2 MiB | 271 ms | holes left |
| `--verify-crc --populate cpu --hugetlb` | 2 MiB | 349 ms | 262 ms |
| `--verify-crc --populate dsa --hugetlb` | 2 MiB | 923 ms | 645 ms |

**DSA populate loses, both page sizes.** The cost of populating a hole is
the kernel allocating and clearing the page; a MEMFILL into an unpopulated
memfd makes the device take that fault through the page-request path and
then fill a page the kernel already zeroed. The in-VMM 16 ms figure was
measured on memory the VMM had already populated, which is a different
operation. User-space fill only pays on pre-populated memory; offloading the
clear itself belongs in the kernel (the mm-offload page-zero series). The
flag stays, default `none`, and `cpu` is the recommendation when populated
memory is wanted. `--hugetlb` is worth using whenever the source guest was
hugepage-backed: restore wall 271 vs 346–670 ms and the memory shape matches
the guest's configuration (the daemon previously always restored onto 4 KiB
pages). Hugetlb memfds have no `write(2)`; restore now copies decompressed
chunks into a mapping of the memfd, which also removed the per-chunk pwrite.

### 7c.2b Restore populate through the mm_offload kernel (clear offload)

UFFDIO_COPY from the mm_offload kernel does not apply to the daemon's
restore: the on-demand path serves faults by writing into the shared memfd
and letting CH issue UFFDIO_CONTINUE (no COPY ioctl at all), and the kernel's
batched COPY offload refuses `VM_SHARED` VMAs and is not hooked for shmem or
hugetlb mfill, while offload/migration memory must be shared or hugepage
backed. What the same kernel does hook is the hugetlb/THP page **clear**
(`folio_zero_user`, hugetlb fault path), and that is exactly the populate
cost above. qual1 runs `7.2.0-mmoff-uffd+` with the `dcbm` provider; it
needs kernel dmaengine work queues, which this host did not have (every DSA
had only the user queue). Added `wq0.1/wq2.1/wq4.1/wq6.1`: kernel, dedicated,
size 128 (the user queues keep their 128 of 256), driver `dmaengine`, group
0, so both kinds share the engines. `echo 1 > /sys/module/dcbm/parameters/offloading`
then claims 4 channels.

| restore cell (hugetlb memfd, `--verify-crc`) | provider off | provider on |
|---|---|---|
| no populate | 266–276 ms | 236 ms |
| `--populate cpu` (MADV_POPULATE_WRITE 3.1 GiB) | 347 ms, populate 264 ms, sys 0.45 s | **247 ms, populate 95 ms, sys 0.11 s** |
| 4 KiB memfd, `--populate cpu` | 945–959 ms | 945 ms (order-0 clears are below `min_clear_bytes` = 2 MiB) |

`folios_cleared` rose by 2609 over the run with no failures or gating, so
the 2 MiB clears went to DSA: the hugetlb populate is 2.8× faster and the
no-populate restore gains too, because IAA writing a chunk into a hugetlb
hole triggers the same clear. This is the right split: the kernel clears
the page it allocates, the daemon never fills zeros itself (`--populate dsa`
stays measured-worse), and user-space DSA is left to classify, CRC and
compress. The queue change is per boot (`accel-config` does not persist)
and the device had to be disabled to add a queue, so the DTO/QPL user queues
were recreated identically in the same step; the provider knob was set back
to 0 after the measurement.

### 7c.2c Their harness: `benchmark/benchmark.sh` with daemon variants

`benchmark/benchmark_dsa.env` runs the branch's own harness (Ubuntu 22.04
cloud guest, 4 vCPUs, 4 GiB `shared=on` on 4 KiB pages, 1536 MiB Silesia
working set in guest RAM, daemon pinned to one CPU by `AUTO_CPU_AFFINITY`,
1 warm-up + 3 iterations, medians) with the async QPL codecs expanded into
variants: bare = the unmodified `iaa-integration` daemon (`BASE_OFFLOAD_BIN`),
`+cpu` / `+dsa` / `+dsacrc` = this branch's daemon. 39 % of the chunks are
zero (1612/4096); the rest is Silesia, which IAA deflates at roughly
1 GiB/s per slot thread here. Results in `docs/measurements/harness/`.

Snapshot (median of 3; CPU is the daemon's share of its one pinned core):

| codec / variant | workers | median ms | CPU % | core-s | MiB out |
|---|---|---|---|---|---|
| raw (sparse copy) | 1 | 6651 | 21 | 1.40 | 2557 |
| lz4 | 1 | 8305 | 80 | 6.64 | 1128 |
| zstd -1 | 1 | 11378 | 89 | 10.13 | 811 |
| qpl static async (base) | 8 | 3098 | 40 | 1.24 | 1095 |
| qpl static async (base) | 32 | 3237 | 39 | 1.26 | 1095 |
| qpl static async +cpu | 32 | 2645 | 22 | 0.58 | 1095 |
| qpl static async +dsa | 8 | 2871 | 18 | 0.52 | 1095 |
| qpl static async +dsacrc | 32 | 2606 | 19 | 0.50 | 1095 |
| qpl dynamic async (base) | 8 | 2867 | 51 | 1.46 | 935 |
| qpl dynamic async +dsa | 8 | 2668 | 24 | 0.64 | 935 |
| qpl dynamic async +dsacrc | 32 | 2576 | 18 | 0.46 | 935 |

Restore (`--resume`, median of 3; variants after the pwrite fix below):

| codec / variant | median ms | CPU % |
|---|---|---|
| raw | 1099 | 97 |
| lz4 | 2348 | 98 |
| zstd | 3151 | 98 |
| qpl static async (base) | 798 | 96 |
| qpl static async +cpu / +dsa / +dsacrc | 802 / 813 / 825 | 96–97 |
| qpl dynamic async (base) | 847 | 96 |
| qpl dynamic async +cpu / +dsa / +dsacrc | 814 / 788 / 804 | 96 |

Reading:

- **IAA is the bottleneck on this workload, for every variant.** 2.5 GiB of
  Silesia at ~1 GiB/s per slot sets a ~2.5 s floor; the base daemon spends
  ~0.6 s more scanning and copying. DSA classify therefore buys 10–20 % of
  wall (3.1–3.2 s → 2.6–2.9 s) and **halves the CPU**: 1.24–1.46 core-s →
  0.46–0.64 core-s, with the pinned core 16–24 % busy instead of 40–51 %.
  On the mostly-zero guests of §7c.1 the same change was 9×, because there
  the scan *was* the critical path.
- **The CPU codecs are not in the race**: lz4 and zstd at one worker take
  2.7–3.7× longer than IAA and 5–8× the CPU; `+dsacrc` reaches lz4's ratio
  class at 0.5 core-s against lz4's 6.6.
- **`+dsacrc` costs nothing visible** (CRC descriptors ride alongside the
  compares) and makes every restore verifiable for ~15–25 ms.
- **Restore regression found and fixed.** The first run had the variants at
  1.55–1.74 s against the base daemon's 0.8 s: writing decompressed chunks
  through a mapping of a fresh 4 KiB memfd takes one page fault per page,
  whereas pwrite lets the kernel allocate in bulk per call. The daemon now
  uses pwrite unless the destination is hugetlbfs (detected with
  `fstatfs`), where the mapping is required. Rerun: 788–825 ms, i.e. parity
  with the base daemon plus the verify cost.
- **4 KiB shared guests are the harness default.** Prefault
  (`MADV_POPULATE_READ`) costs 25–70 ms per slot here (first cell 650 ms,
  the §7c.3 first-mapping effect), and none of the hugetlb benefits of
  §7c.2/§7c.2b apply. `MEMORY_SIZE=4G` with `hugepages=on` would be the
  configuration to show them, and is a one-line change in `source-vm.sh`.

### 7c.2d Fair CPU-vs-DSA snapshot comparison (harness, 4 KiB and 2 MiB guests)

Supersedes the snapshot half of §7c.2c, which mixed IAA depths (8 and 32)
across cells and pinned the daemon's two slot threads to one core. Here every
cell uses the same IAA depth (`--workers 8`, the harness default), the VM is
pinned to cores 0-3 and the daemon to cores 4-5 (one core per slot thread),
and only the classifier differs. 1 warm-up + 5 measured iterations, medians.
Same Silesia guest (1536 MiB working set, ~39 % zero chunks), 4 KiB shared
pages vs 2 MiB hugetlb (`HUGEPAGES=1` in `source-vm.sh`). Configs:
`benchmark/benchmark_fair_{4k,2m}.env`; results:
`docs/measurements/harness/fair_{4k,2m}_results.csv`. Every DSA cell submitted
all its descriptors to DSA with zero fallbacks and zero CPU scans.

Snapshot, static Huffman (`qpl-hardware-static-async`):

| classifier | 4 KiB wall | 4 KiB core-s | 2 MiB wall | 2 MiB core-s |
|---|---|---|---|---|
| base daemon (byte scan + pread) | 2553 ms | 1.24 | 2514 ms | 1.16 |
| CPU (word scan, mapped) | 2541 ms | 0.69 | 2739 ms | 0.53 |
| DSA | 2546 ms | 0.57 | 2873 ms | 0.47 |
| CPU + CRC32C | 2734 ms | 1.13 | 2634 ms | 0.90 |
| DSA + CRC32C | 2540 ms | 0.58 | 2452 ms | 0.46 |

Snapshot, dynamic Huffman (`qpl-hardware-dynamic-async`):

| classifier | 4 KiB wall | 4 KiB core-s | 2 MiB wall | 2 MiB core-s |
|---|---|---|---|---|
| base daemon | 2636 ms | 1.78 | 2590 ms | 1.71 |
| CPU | 2362 ms | 0.99 | 2369 ms | 0.85 |
| DSA | 2361 ms | 0.88 | 2225 ms | 0.77 |
| CPU + CRC32C | 2545 ms | 1.43 | 2423 ms | 1.21 |
| DSA + CRC32C | 2341 ms | 0.89 | 2223 ms | 0.78 |

Raw (sparse copy) for reference: 5618 ms / 0.89 core-s on 4 KiB, 8887 ms /
1.16 core-s on 2 MiB (hugetlb has no holes for `SEEK_DATA` to skip, so all
4096 MiB is written).

Reading:

- **Wall is set by the root disk, not by IAA or the classifier** on this
  workload (§7c.2j: snapshots are written to the root RAID volume at
  ~430 MB/s with fsync): CPU and DSA classify are within run-to-run noise (2 MiB static
  spans 2419-2975 ms across iterations). The one wall difference is CRC:
  generating CRC32C on the CPU adds ~190 ms (4 KiB), on DSA it adds nothing.
- **CPU cost, classify only:** DSA saves 0.06-0.12 core-s per snapshot
  over the word-wide CPU scan (10-17 %). Most of the earlier "halved CPU"
  came from mapping instead of pread and from replacing the byte loop, which
  the CPU variant also gets.
- **CPU cost with integrity:** DSA saves 0.44-0.55 core-s per snapshot
  (40-49 %) because COMPARE and CRC both stay on the device.
- **Against the unmodified daemon** DSA+CRC uses 0.46-0.89 core-s instead of
  1.16-1.78 (about half) at equal or lower wall, and adds per-chunk CRC.
- **2 MiB pages:** prefault drops from ~46 ms to ~1 ms per slot and CPU per
  snapshot falls a further 10-20 % in every variant; wall is unchanged
  because the disk write sets it.
- Dynamic Huffman costs more host CPU than static in every variant (QPL
  builds the Huffman table on the CPU), so the classifier savings are a
  smaller fraction there.

### 7c.2e Real-agent microVM lifecycle (benchmark/agent)

Seven recorded Terminal-Bench 2 agent sessions (10-60 turns, 170 turns in
all) replayed in cloud-hypervisor guests booted from each task's own Docker
image (RW copy, 2 vCPUs, 4 GiB `shared=on`, 4 KiB or 2 MiB hugetlb). After
the warm base point and after every agent turn the VM is paused and **every
daemon variant checkpoints the same paused image** (order rotated per turn),
then the VM resumes; each variant's `send-migration` wall time is the pause
it alone would impose. After the last turn the final and the base
checkpoints are restored per variant into a fresh VMM and the guest must
progress (resume the session; fork from the warm base). All variants use
`qpl-hardware-static-async`, 1 MiB chunks, `--workers 8`, daemon pinned to
cores 4-5, VM to cores 0-3. `bash benchmark/agent/run_all.sh 4k 2m`;
results in `docs/measurements/lifecycle/`. 1770 checkpoints, 0 failures,
0 DSA fallbacks; 140/140 restores resumed the guest; manifests identical
across variants (193 MiB compressed per checkpoint, median).

Per-turn checkpoint (median over all 170 turns; p95 within ~25 %):

| variant | 4 KiB wall | 4 KiB core-s | 2 MiB wall | 2 MiB core-s |
|---|---|---|---|---|
| base (unmodified daemon) | 1147 ms | 1.47 | 1416 ms | 1.79 |
| cpu | 97 ms | 0.13 | 229 ms | 0.32 |
| dsa | 97 ms | 0.13 | 96 ms | 0.14 |
| cpu + CRC | 138 ms | 0.20 | 250 ms | 0.39 |
| dsa + CRC | 96 ms | 0.13 | 123 ms | 0.20 |

Daemon CPU for all checkpoints of all seven sessions (171 per variant):
4 KiB base 261 s, cpu 24.3, dsa 23.1, cpucrc 36.5, dsacrc 23.2; 2 MiB base
321 s, cpu 60.2, dsa 25.4, cpucrc 69.5, dsacrc 34.7. Restores: 130-175 ms
for every variant and both page sizes (IAA decompression of ~200 MiB).

Reading:

- **Agent guests are almost all untouched memory.** At 4 KiB the daemon
  sees it directly: `SEEK_DATA` finds ~250 MiB of data per 4 GiB guest and
  2800+ of 3072 chunks in the large slot are holes that are never read.
  Classification then touches 250 MiB per checkpoint and the word-wide CPU
  scan is as fast as DSA (97 ms both). DSA still wins with CRC: 96 vs
  138 ms per turn and 35 % less CPU.
- **hugetlbfs reports no holes**, so on 2 MiB guests every checkpoint
  classifies all 4 GiB. That is where DSA pays: 2.4x lower pause than CPU
  classification (96 vs 229 ms) at 2.3x less CPU, and 2x with CRC.
- **Against the unmodified daemon** a turn checkpoint is 12-15x shorter on
  either page size with DSA, and a 60-turn session's checkpoint CPU drops
  from 117 s to 9-12 s (2 MiB). The base daemon's ~1.1-1.4 s per turn is the
  byte-at-a-time scan plus pread of 4 GiB, regardless of how much the agent
  touched.

**Defect found by this benchmark and fixed (f8d5a1633).** Mapping a shmem
memfd and populating or reading it allocates a zeroed page for every hole.
The first lifecycle run grew each 4 KiB guest's memfd from ~90 MiB to its
full 4096 MiB (system Shmem +4 GiB) at the first checkpoint, and made that
checkpoint ~1.9 s. The daemon now finds data extents with
`SEEK_DATA`/`SEEK_HOLE`, records chunks wholly inside a hole as zero
without touching them, and populates only data extents: memfd stays at
480-490 MiB and the populate step is 5-7 ms. On hugetlb guests the same
populate turns the guest's reserved huge pages into zeroed ones (in use 249
-> 2048, `HugePages_Rsvd` 1799 -> 0, unreserved free unchanged at 1024), so
it takes no memory from anyone else; it costs ~0.2-0.8 s of kernel page
zeroing once, at the first checkpoint, which the mm_offload clear provider
can absorb (§7c.2b). The pre-fix run is kept as
`docs/measurements/lifecycle/prefix_memory_inflation_summary.txt`.

Guest time is small: these agents mostly ran `ls`, `cat` and small edits;
the heaviest turn (`gcc -static` and run) took 0.86 s, and the 170 turns
took 9 s of guest time in all. The restore "guest progressed" times are a
liveness check that includes the guest's own post-step sleep, not a restore
latency.

### 7c.2f Dirty-log diff checkpoints (real-agent lifecycle)

CH now reports the pages written since the previous checkpoint over the memfd
migration path (`send-migration ... dirty_log=keep|consume`, new `DirtyLog`
command; f62af5364). The daemon keeps a sparse reference copy of the last
checkpoint, compares each dirty 4 KiB page against it, writes the changed
pages to the reference and a gather buffer in one pass, and compresses the
gather buffer with IAA (f8425c6ae). Compare and copy run on the CPU or as
batched DSA COMPARE + DUALCAST through DTO's new `dto_batch_*` API
(byrnedj/DTO a8d14aa). Restore follows the parent chain.

**Correctness**: on 4 KiB and 2 MiB guests, the chain of every variant
restores identical to a raw dump of the source at the final checkpoint,
except one page (0xaa401000 / 0xae401000) that a plain restore of that raw
dump rewrites as well. In the lifecycle run, CPU and DSA comparison chose the
same changed pages in 340/340 turns; 70/70 chain restores (up to 60 diffs)
resumed the guest.

**Run**: `benchmark/agent/run_all_diff.sh 4k 2m`, the seven sessions of
§7c.2e, every checkpoint taken by five variants from the same paused image
(all diff variants fed the same dirty set): `full-dsa` (§7c.2e's best full
checkpoint), `diff-none` (store every dirty page), `diff-cpu`, `diff-dsa`
(256 pages per batch descriptor, 8 in flight), `diff-dsa1` (one descriptor
per page, 32 in flight).

Per-turn checkpoint, median over 170 turns:

| variant | 4K wall | 4K cpu-s | 2M wall | 2M cpu-s | stored | compare | gather | compress | prepare |
|---|---|---|---|---|---|---|---|---|---|
| full-dsa | 96 ms | 0.13 | 96 ms | 0.14 | 198 MB | | | | |
| diff-none | 33 ms | 0.03 | 37 ms | 0.03 | 835 KiB | 0 | 1.9 ms | 11-18 ms | 8 ms |
| diff-cpu | 33 ms | 0.03 | 37 ms | 0.02 | 610-622 KiB | 0.5-0.6 ms | 1.9 ms | 10-16 ms | 8 ms |
| diff-dsa | 32 ms | 0.02 | 34 ms | 0.02 | 610-622 KiB | 0.5 ms | 1.0-1.1 ms | 9-16 ms | 7-8 ms |
| diff-dsa1 | 32 ms | 0.03 | 35 ms | 0.02 | 610-622 KiB | 1.5-1.7 ms | 1.7-1.9 ms | 9-14 ms | 7-8 ms |

Whole sessions (all 171 checkpoints of the seven sessions, per variant):

| variant | 4K wall | 4K cpu-s | 2M wall | 2M cpu-s | stored |
|---|---|---|---|---|---|
| full-dsa | 18.0 s | 23.8 | 19.4 s | 27.2 | 36.2 GB |
| diff-none | 8.0 s | 7.8 | 8.7 s | 8.6 | 1.75 GB |
| diff-cpu | 8.1 s | 7.9 | 8.9 s | 8.6 | 1.71 GB |
| diff-dsa | 7.5 s | 6.9 | 7.7 s | 6.5 | 1.71 GB |
| diff-dsa1 | 7.8 s | 7.6 | 8.1 s | 6.8 | 1.71 GB |

Restore: full 155-176 ms; diff chains 249-298 ms (DSA variants 248-256 ms).
(Whole-session totals include each variant's full base checkpoint, which
dominates diff storage: ~1.4 GB of the 1.7 GB.)

Reading:

- **The dirty log is the lever.** A turn dirties a median 1,380 pages
  (5.4 MiB) of a 4 GiB guest, so a diff checkpoint pauses the VM 3x shorter
  than the best full checkpoint, uses 4-7x less CPU and stores 240-320x
  less (0.6-0.8 MB vs 198 MB). Over the seven sessions storage drops from
  36 GB to 1.7 GB, most of it the base.
- **Comparing dirty pages pays in size, not time.** A third of dirty pages
  are false dirty (written with the same content); filtering them shrinks
  each diff by 27 % (835 to 610-622 KiB). It costs 0.5 ms per turn.
- **CPU vs DSA**: at 5 MiB per turn both are fast. Batched DSA compares as
  fast as memcmp (0.5 ms) and halves the copy (1.0 vs 1.9 ms, one DUALCAST
  replacing two memcpy); end to end DSA saves 1-3 ms per turn and 12-24 % of
  the session's daemon CPU (6.5-6.9 vs 7.9-8.6 core-s). The per-turn wall
  differences are within run-to-run noise.
- **Batching**: one descriptor per page is 3x slower to compare (1.5-1.7 vs
  0.5 ms) and 1.7x slower to copy than batches of 256, matching the
  microbenchmark (40 vs ~800 ns per page). Batching is what makes DSA viable
  at 4 KiB granularity at all.
- **What remains in a 33 ms diff checkpoint** is not compare or copy:
  compression of ~3.6 MiB (9-18 ms, IAA job-pool setup per process plus a few
  chunk round trips), preparation (7-8 ms: mapping both slots, the reference
  hole map, one populate call per dirty run), and ~7 ms of daemon start-up
  and protocol. Those are the next things to batch: one populate over the
  dirty span instead of one per run, reuse of IAA jobs across slots, and a
  resident daemon instead of one process per checkpoint.
- KVM dirty logging write-protects guest memory and forces 4 KiB mappings in
  the second-level page tables, which costs the guest on 2 MiB guests; guest
  time is tiny in these sessions (§7c.2e) so it does not show here.

### 7c.2g Resident daemon, shared IAA pool, minimal populate

What §7c.2f left in a 33 ms diff checkpoint was not compare or copy but
per-process fixed cost: process start, DTO and QPL initialisation, one IAA
job pool per slot thread, mapping the slots and the reference again, and one
`madvise` per dirty run. `offload_daemon serve` (df4599a42) keeps all of it:

- **resident**: one daemon per VM; each `send-migration` connection is a
  checkpoint in `<output-root>/ckpt-NNNNNN`, full first, then diffs chained
  to the previous one. Slot mappings, the reference mapping and its page
  bitmap, DTO's work queues and the job pool live across checkpoints.
- **one IAA job pool for all slots**: diff checkpoints compress every slot's
  gather buffer through the whole pool; full checkpoints split the pool's
  idle jobs between concurrent slot threads and take them back afterwards
  (the jobs are created once; a first version ran the slots one after
  another on the shared pool and lost 30 % on 2 MiB full checkpoints).
- **minimal populate**: a bitmap records which pages already have page-table
  entries in the persistent mapping, so a checkpoint only populates pages
  the guest allocated since: one call over the dirty span on hugetlbfs, and
  on shmem one call per run of unmapped pages, bridged across pages already
  mapped (bridging over a hole would allocate it). Median: 1-2 calls per
  checkpoint.

Verified as before on 4 KiB and 2 MiB guests (resident chains restore
identical to a raw dump except the one page a plain restore rewrites).
Lifecycle: `benchmark/agent/run_all_resident.sh 4k 2m`, the seven sessions,
six variants per checkpoint from the same paused image (one-shot vs
resident; full DSA, diff CPU, diff batched DSA), 2124 checkpoints, 0
failures, 84/84 restores resumed. Wall is the `send-migration` time until
the daemon has written the checkpoint (`ckpt_send.py`; `send-migration`
returns before completion); CPU is the daemon's own `getrusage` per
checkpoint (resident) or its rusage at exit (one-shot), both microsecond.

Per-turn checkpoint, median over 170 turns (p95 in brackets):

| variant | 4K pause | 4K CPU | 2M pause | 2M CPU |
|---|---|---|---|---|
| full, one-shot | 108 ms (136) | 145 ms | 153 ms (164) | 227 ms |
| full, resident | 80 ms (106) | 106 ms | 148 ms (157) | 223 ms |
| diff CPU, one-shot | 41 ms (78) | 39 ms | 42 ms (73) | 41 ms |
| diff DSA, one-shot | 40 ms (60) | 37 ms | 39 ms (51) | 36 ms |
| diff CPU, resident | 10 ms (38) | 4.9 ms | 11 ms (36) | 5.8 ms |
| diff DSA, resident | **9 ms (22)** | **3.7 ms** | **10 ms (22)** | **4.6 ms** |

Diff checkpoint phases, one-shot -> resident: prepare 8.6-10.2 -> 0.1 (4K)
/ 1.3-1.4 ms (2M); compress 15.6-16.2 -> 0.7-0.8 ms (job setup was the
cost, not compression of ~3.6 MiB); compare 0.4-0.8 ms and copy 1.2-2.6 ms
unchanged.

All checkpoints of the seven sessions:

| variant | 4K wall | 4K CPU | 2M wall | 2M CPU | stored |
|---|---|---|---|---|---|
| full, one-shot | 19.8 s | 26.2 s | 28.7 s | 42.1 s | 36.2 GB |
| full, resident | 15.0 s | 19.5 s | 25.6 s | 39.3 s | 36.2 GB |
| diff DSA, one-shot | 8.9 s | 9.2 s | 8.8 s | 9.5 s | 1.7 GB |
| diff DSA, resident | 3.3 s | 3.0 s | 3.5 s | 3.8 s | 1.7 GB |

Reading:

- **A resident diff checkpoint pauses the guest 9-10 ms and costs 4-5 ms of
  CPU**: 4x shorter and 7-10x cheaper than the one-shot daemon, and 10-16x
  shorter than a one-shot full checkpoint, for the same 600 KiB per turn.
- **CPU vs DSA, resident**: DSA uses 20-25 % less CPU (3.7 vs 4.9 ms, 4.6 vs
  5.8 ms) and has a much tighter tail (p95 22 ms vs 36-38 ms): the turns that
  dirty the most pages are where memcmp + two memcpy show, and where batched
  COMPARE + DUALCAST does not.
- **Full checkpoints gain less**: resident saves 26 % of the pause and 27 %
  of the CPU on 4 KiB guests; on 2 MiB guests, where every byte of the
  guest is classified (no holes), the work itself dominates and resident is
  at parity (148 vs 153 ms). The first checkpoint is where resident helps
  most on 2 MiB (130 vs 474 ms median), because populating the guest's
  reserved huge pages happens once.
- **Restore is unchanged**: 145-178 ms full, 232-266 ms for chains of up to
  60 diffs; restore is a separate one-shot process in both cases.
- Measurement note: a first pass summed the run time of the daemon's live
  threads, which misses the slot threads of a full checkpoint once they
  exit; the daemon now reports its own `getrusage` per checkpoint.

### 7c.2h Disk-block deduplication with a DSA CRC index

Pages an agent's guest reads from its root filesystem sit in the guest page
cache as byte-identical copies of 4 KiB disk blocks (§ dedup measurement:
8-22 % of a session's final data). `serve --dedup-image <template>`
(d2d5c36a3) indexes the immutable task image: one CRC32C per non-zero
block (batched DSA CRC generation, `dto_batch_add_crc`, or SSE4.2), sorted.
At each diff checkpoint the changed pages get a CRC in one batched pass;
index hits are confirmed with a batched DSA COMPARE against the image block
(memcmp on the CPU), so a CRC collision can never change a snapshot. Matches
are stored as (page, image block, CRC) runs and skip IAA; restore reads them
from the image and re-checks every CRC, refusing a changed image. Base
(full) checkpoints are not deduplicated: at the base only 2-6 % of data
matches the disk, the toolchain page cache arrives in diffs.

Verified on 4 KiB and 2 MiB guests (path-tracing, a diff spanning the
`gcc -static` turn): chains restore identical to a raw dump of the source
except the one page a plain raw restore rewrites. Lifecycle
(`VARIANTS="diff-cpu-res diff-dsa-res diff-cpu-res-dedup diff-dsa-res-dedup"
run_all_resident.sh 4k 2m`): 1416 checkpoints, 0 failures, 56/56 restores
(chains up to 60 diffs) resumed; results in
`docs/measurements/lifecycle-dedup/`.

Diff storage per session (all turns, DSA; 4 KiB, 2 MiB within 1 %):

| session | diffs | with dedup | saved | served from image |
|---|---|---|---|---|
| circuit-fibsqrt 7B | 32.1 MiB | 12.8 MiB | 60 % | 37 MiB |
| polyglot-rust-c (rustc, g++) | 86.6 MiB | 36.3 MiB | 58 % | 104 MiB |
| circuit-fibsqrt 30B | 43.3 MiB | 23.8 MiB | 45 % | 38 MiB |
| regex-chess (python, pip) | 70.4 MiB | 55.7 MiB | 21 % | 33 MiB |
| path-tracing (gcc) | 84.1 MiB | 68.5 MiB | 19 % | 31 MiB |
| write-compressor | 8.2 MiB | 7.4 MiB | 9 % | 1.5 MiB |
| winning-avg-corewars | 35.9 MiB | 34.9 MiB | 3 % | 2.1 MiB |
| **all seven** | **361 MiB** | **239 MiB** | **34 %** | **246 MiB** |

Per-turn cost, median (p95):

| variant | 4K pause | 4K CPU | 2M pause | 2M CPU | dedup step |
|---|---|---|---|---|---|
| diff, DSA | 9 ms (22) | 3.7 ms | 10 ms (24) | 4.5 ms | |
| diff, DSA + dedup | 10 ms (24) | 4.5 ms | 11 ms (27) | 4.9 ms | 0.6-0.7 ms |
| diff, CPU | 11 ms (40) | 4.9 ms | 11 ms (38) | 5.7 ms | |
| diff, CPU + dedup | 12 ms (52) | 6.4 ms | 12 ms (48) | 6.8 ms | 1.4-2.1 ms |

Index build, once per daemon (1.5-2 GB template images, 190-280 k
indexed blocks): DSA 40-215 ms, CPU 187-700 ms.

Reading:

- **Dedup removes a third of all diff storage, and up to 60 % per session**
  where the agent ran compilers or interpreters; sessions that only probed
  files gain little. It is content-based, so it catches whatever the guest
  read from disk without guest cooperation.
- **With DSA it is nearly free**: 0.6-0.7 ms and under 1 ms of CPU per turn
  at the median. The CPU path costs 2-3x as much per turn and far more on the
  turns that matter (after a compiler run, CPU dedup took 42-50 ms vs 9 ms
  on DSA), which is where its p95 grows from 40 to 52 ms.
- The base checkpoint (~1.4 GB of the 1.6 GB per session set) is unchanged
  here; deduplicating full checkpoints would need 4 KiB-granular records
  inside the 1 MiB chunks and would save only 2-6 % there.

### 7c.2i The iaa-integration benchmark on its original configuration

`benchmark/benchmark.sh` with `benchmark/benchmark_original_dsa.env`: the
branch's own `benchmark.env` unchanged (Silesia 1536 MiB working set in a
4 GiB, 4 KiB-page guest; codecs raw / lz4 / zstd / QPL static and dynamic
async; 1 MiB chunks; IAA depth 8 for snapshot and 32 for restore; daemon
pinned to one core by `AUTO_CPU_AFFINITY`; warm page cache), plus the four
daemon variants of the async QPL codecs and 20 measured iterations per cell
(1 warm-up) so p95 is meaningful. 231 snapshots and 231 restores, no
failures. Results: `docs/measurements/harness-original/`.

What is measured:
- **snapshot**: from `ch-remote send-migration` until the daemon process has
  written, fsynced and acknowledged every slot (`snapshot.sh`). The VM is
  paused for all of it. CPU % is the daemon's (`/usr/bin/time %P`).
- **restore**: from starting the restore daemon until CH's `receive-migration`
  returns with the VM resumed (`restore.sh`): read, decompress into memfds,
  hand them to CH. It ends when the VMM resumes the VM, not when the guest
  is responsive.

What DSA does here: on snapshot, the zero-chunk check of every 1 MiB chunk
(COMPARE against a zero buffer, ahead of IAA) and, for `dsacrc`, a CRC32C
per compressed chunk; on restore, only `dsacrc` uses DSA (CRC verification
of each decompressed chunk). Compression and decompression are IAA in every
QPL variant.

| snapshot | median | p95 | daemon CPU |
|---|---|---|---|
| QPL static, base daemon | 2771 ms | 3021 ms | 41.5 % (1.15 core-s) |
| QPL static, cpu classify | 2561 ms | 3078 ms | 15 % |
| QPL static, dsa classify | 2547 ms | 2999 ms | 15 % (0.38 core-s) |
| QPL static, dsa + crc | 2555 ms | 2787 ms | 14 % |
| QPL dynamic, base daemon | 2664 ms | 3345 ms | 45.5 % (1.21 core-s) |
| QPL dynamic, cpu classify | 2682 ms | 3009 ms | 21 % |
| QPL dynamic, dsa classify | 2581 ms | 2725 ms | 21 % (0.54 core-s) |
| QPL dynamic, dsa + crc | 2576 ms | 2862 ms | 21 % |
| raw / lz4 / zstd | 6156 / 8748 / 11390 ms | 6644 / 9016 / 11751 ms | 16 / 75 / 89 % |

| restore | median | p95 |
|---|---|---|
| QPL static: base / cpu / dsa / dsa + verify | 785 / 792 / 791 / 795 ms | 793 / 799 / 796 / 799 ms |
| QPL dynamic: base / cpu / dsa / dsa + verify | 766 / 767 / 772 / 776 ms | 776 / 775 / 777 / 781 ms |
| raw / lz4 / zstd | 1080 / 2304 / 3139 ms | 1086 / 2314 / 3147 ms |

Reading: on this workload the snapshot is bound by writing ~1 GiB of
output to the root disk (~430 MB/s with fsync, §7c.2j), not by IAA (the
compress phase is 0.3-0.4 s of the 2.6 s), so the classifier is not on
the critical path. Against the unmodified daemon, median snapshot time drops
3-8 % and daemon CPU 55-67 %; DSA against CPU classification inside our
daemon is 0.5-3.8 % on the median and 3-9 % on p95. Restore is unchanged
(within 1 %, the CRC verification included). The 10-20 % wall figure in
§7c.2c came from a run that mixed IAA depths between cells and is
superseded. DSA's larger effects are on mostly-zero guests (§7c.1, 9x) and
on dirty-log diff checkpoints (§7c.2f-h).

### 7c.2j One ready queue for all slots, IAA depth 8/16/32

`compress_slots` (default `--pipeline shared`) runs one DSA classifier per
memory slot and feeds every slot's verdicts into a single ready queue that
drains into one shared IAA job pool of `--workers` jobs. The classifiers
are fed round-robin, so IAA always has kept chunks from whichever slot is
ahead. `--pipeline per-slot` keeps the earlier layout, one thread and one
pool per slot with `--workers` split evenly, so both layouts keep the same
number of IAA jobs in flight. Harness variants `+cpuslot` / `+dsaslot` are
the per-slot layout.

The first sweep showed the shared layout slower. The cause was the disk,
not the queue: the harness writes snapshots to the root RAID volume
(Broadcom 9660-16i, `dd` 1 GiB with fsync = 2.4 s, ~430 MB/s), and the
shared path issued one large fsync per slot after compression ended. The
daemon now starts writeback every 64 MiB of output
(`sync_file_range(SYNC_FILE_RANGE_WRITE)`) and fsyncs all slots
concurrently; the completion log splits `compress_ms` from `sync_ms`.

Configuration: the original harness (§7c.2i: Silesia, 4 GiB 4 KiB-page
guest, 1 MiB chunks, daemon pinned to one core), 20 measured iterations per
cell, restore depth 32. `benchmark_pipeline_disk.env` writes to the root
disk at depth 8; `benchmark_pipeline_tmpfs.env` writes to tmpfs
(`SNAPSHOT_ROOT=/mnt/chsnap/harness`) at depths 8, 16 and 32 so that the
engines, not the disk, set the time. Results:
`docs/measurements/harness-pipeline-{disk,tmpfs}/`.

**Root disk, depth 8 (snapshot):**

| codec | base daemon | cpu shared | cpu per-slot | dsa shared | dsa per-slot |
|---|---|---|---|---|---|
| static, median | 2735 | 2383 | 2394 | 2387 | 2394 |
| static, p95 | 2805 | 2675 | 2800 | 2704 | 2678 |
| dynamic, median | 2504 | 2081 | 2106 | 2089 | 2103 |
| dynamic, p95 | 2927 | 2405 | 2269 | 2221 | 2365 |

The compress phase is 1.27-1.70 s and the tail fsync 0.63-1.14 s (shared
0.63-0.70, per-slot 1.00-1.14), yet wall is equal across layouts: the disk
drains ~1 GiB at its own rate either way. Against the unmodified daemon
our daemon is 13-17 % faster, from early writeback and mapped input; CPU vs
DSA classify is within 0.5 %.

**tmpfs (snapshot median / p95 ms):**

| codec, depth | base | cpu shared | cpu per-slot | dsa shared | dsa per-slot |
|---|---|---|---|---|---|
| static, 8 | 1236 / 1248 | 401 / 403 | 419 / 421 | **395 / 397** | 414 / 420 |
| static, 16 | 1243 / 1247 | 398 / 400 | 398 / 401 | **390 / 393** | 393 / 397 |
| static, 32 | 1264 / 1270 | 407 / 410 | 407 / 410 | 401 / 404 | 402 / 407 |
| dynamic, 8 | 1288 / 1341 | 545 / 548 | 589 / 593 | **539 / 542** | 579 / 584 |
| dynamic, 16 | 1223 / 1279 | 443 / 447 | 419 / 423 | 431 / 440 | **412 / 415** |
| dynamic, 32 | 1233 / 1238 | 408 / 417 | 389 / 392 | 401 / 408 | **381 / 384** |

Compress phase alone (median ms, from the daemon log):

| codec, depth | cpu shared | cpu per-slot | dsa shared | dsa per-slot |
|---|---|---|---|---|
| static, 8 / 16 / 32 | 274 / 269 / 276 | 306 / 283 / 290 | 268 / 262 / 270 | 300 / 279 / 286 |
| dynamic, 8 / 16 / 32 | 417 / 313 / 275 | 476 / 305 / 272 | 410 / 302 / 270 | 468 / 298 / 265 |

Restore (both sweeps): 760-811 ms median, p95 within 10 ms of the median,
identical across all layouts and the base daemon.

Reading:

- **The compress phase is max(DSA, IAA).** Swapping the CPU classifier
  for DSA changes the compress phase by 2-7 ms at every depth, while the
  IAA depth changes it by up to 150 ms. Classification is hidden behind
  IAA.
- **Static Huffman is saturated at depth 8.** 2.5 GiB of kept data
  compresses in ~265 ms (~9.5 GiB/s); depth 16 and 32 add nothing. Here
  the shared queue wins at depth 8 (395 vs 414 ms) because one pool keeps
  all 8 jobs busy while a 4+4 split idles the jobs of the slot that ran out
  of work first (slot 0 has 1.5 GiB kept, slot 1 1 GiB).
- **Dynamic Huffman wants depth.** 8 → 32 cuts the compress phase 410 →
  270 ms and the snapshot 539 → 401 ms. At 16 and 32 the per-slot layout
  is 4-5 % faster. QPL builds the dynamic Huffman table on the submitting
  thread, and the shared layout has one submitter while per-slot has two,
  so the CPU half of dynamic compression is serialized. Two submitter
  threads draining the shared queue would combine both advantages.
- **~130 ms of each tmpfs snapshot is outside compression** (migration
  handshake, memfd transfer, populate, manifest). That is the next floor.
- **Against the unmodified daemon on tmpfs** our daemon is 3.1× faster on
  wall (static 1236 → 395 ms; dynamic 1233 → 381 ms at depth 32).

**Host trap found on the way.** A first run of both sweeps on the same
boot showed restore at 1110-1240 ms for every variant, the base daemon and
raw included (the earlier §7c.2i run had 766-795 ms). Node 0 had 2 GB free
under 84 GB of page cache, so every restore's 4 GiB memfd had to reclaim
first. After `drop_caches` restore is back to 760-811 ms and tmpfs
snapshots are ~30 % faster. That run is discarded. The sweep script now
drops caches before each sweep and logs per-node free memory (node 0 stayed
above 50 GB).

### 7c.2k NVMe output and CPU baselines: what DSA itself contributes

The root disk hides the engines (§7c.2j), so these runs write snapshots to
the local NVMe drive (KCD6XLUL3T84, ext4 on `/dev/nvme0n1` mounted at
`/mnt/nvme`, `dd` 1 GiB with fsync = 1.4 GB/s). Same harness and guest as
§7c.2i/j, 20 iterations per cell, caches dropped before each sweep, node 0
never below 48 GB free. The harness now records the daemon's user + system
seconds (`%U %S`) next to `%P`. Results:
`docs/measurements/harness-attrib-nvme{,-crc3}/` and
`docs/measurements/harness-pipeline-nvme/`.

To attribute, each cell runs a ladder in which exactly one thing changes
per step:

- **base**: the unmodified `iaa-integration` daemon (byte-loop zero scan,
  pread input).
- **cpu**: our daemon with no DSA (mapped input, word-wide CPU zero scan,
  shared IAA queue, early writeback).
- **dsa**: the same with the zero check on DSA COMPARE.
- **cpucrc / dsacrc**: per-chunk CRC32C added, generated on snapshot and
  verified on restore, by the CPU or by DSA. Restore of `cpucrc` uses the
  new `--verify-engine cpu`, so only the engine differs.

The CPU CRC is a three-lane SSE4.2 implementation (24.7 GB/s on one core;
the earlier single chain did 8.4 GB/s and made CPU CRC look 2-3x worse
than it needs to be: first attribution run, `harness-attrib-nvme`, kept
for reference).

**Snapshot, static Huffman (median / p95 ms, daemon CPU seconds):**

| step | depth 8 | CPU s | depth 32 | CPU s |
|---|---|---|---|---|
| base | 1517 / 1631 | 1.22 | 1542 / 1551 | 1.25 |
| cpu | 625 / 641 | 0.39 | 639 / 648 | 0.39 |
| dsa | 633 / 642 | 0.38 | 639 / 648 | 0.38 |
| cpucrc | 725 / 759 | 0.54 | 795 / 806 | 0.59 |
| dsacrc | 637 / 651 | 0.40 | 651 / 663 | 0.40 |

**Snapshot, dynamic Huffman:**

| step | depth 8 | CPU s | depth 32 | CPU s |
|---|---|---|---|---|
| base | 1522 / 1533 | 1.27 | 1469 / 1485 | 1.22 |
| cpu | 573 / 591 | 0.53 | 572 / 583 | 0.40 |
| dsa | 592 / 602 | 0.53 | 575 / 589 | 0.39 |
| cpucrc | 772 / 788 | 0.73 | 690 / 705 | 0.55 |
| dsacrc | 599 / 605 | 0.53 | 585 / 596 | 0.39 |

**Restore, depth 32 (median / p95 ms, CPU s):**

| step | static | CPU s | dynamic | CPU s |
|---|---|---|---|---|
| base | 788 / 798 | 0.75 | 768 / 773 | 0.73 |
| cpu, dsa (no CRC) | 792-794 / 798 | 0.75 | 771-772 / 777-779 | 0.73 |
| cpucrc (verify on CPU) | 922 / 933 | 0.88 | 904 / 909 | 0.86 |
| dsacrc (verify on DSA) | 801 / 808 | 0.76 | 778 / 788 | 0.74 |

Attribution:

- **Software changes, not DSA, give the big snapshot win.** base → cpu is
  2.4-2.6x faster on wall and uses 3-4x less CPU (1.22-1.27 → 0.39-0.53
  core-s). That comes from mapping the memfd instead of pread, a word-wide
  zero scan instead of the byte loop, the shared IAA queue and early
  writeback.
- **The DSA zero check adds nothing measurable** once the CPU scan is
  word-wide: cpu vs dsa is within 1.5 % on wall and 0.01 core-s on CPU at
  every depth. The zero check is not on the critical path of this
  workload (IAA and the NVMe write are).
- **DSA's contribution is integrity for free.** With per-chunk CRC32C,
  DSA keeps the snapshot at the no-CRC time (dsacrc − dsa ≤ 12 ms) while
  the CPU pays 90-175 ms (12-22 % of wall) and 0.14-0.20 core-s (26-32 %
  of the daemon's CPU). On restore DSA verification costs 7-9 ms against
  126-134 ms on the CPU, and 0.01 against 0.13 core-s (14 % less wall,
  14 % less CPU).
- **The CPU CRC sits on the critical path** because the daemon is pinned
  to one core and the CRC runs on the thread that feeds IAA; DSA moves it
  off that thread entirely.
- Restore without CRC is identical across all variants including base:
  DSA is not used on restore unless verification is on.
- On NVMe the compress phase stays at ~420 ms across IAA depths 8/16/32
  (265 ms on tmpfs): the drive's write rate now paces it, and the shared
  queue and per-slot layouts finish within 3 % of each other.

### 7c.2l The same ladder on a 2 MiB hugetlb guest

Same as §7c.2k but the source guest is backed by 2 MiB hugetlbfs pages
(`benchmark_attrib_nvme_2m.env`, `HUGEPAGES=1`). Restore stays on 4 KiB
shmem for every variant so each step still changes one thing. Results:
`docs/measurements/harness-attrib-nvme-2m/`.

A hugetlbfs file has no holes, so nothing is skipped before classification:
1619 of the 4096 chunks are zero and every one of them (1.6 GiB) has to be
read to the end, against 82 on the 4 KiB guest. Prefault drops from 30-40
ms to 0.5-1.2 ms per slot.

**Snapshot (median / p95 ms, daemon CPU seconds):**

| step | static d8 | CPU s | static d32 | CPU s | dynamic d8 | CPU s | dynamic d32 | CPU s |
|---|---|---|---|---|---|---|---|---|
| base | 1441 / 1543 | 1.15 | 1464 / 1489 | 1.18 | 1619 / 1633 | 1.37 | 1562 / 1573 | 1.31 |
| cpu | 590 / 612 | 0.37 | 586 / 619 | 0.37 | 566 / 581 | 0.52 | 548 / 560 | 0.38 |
| dsa | 521 / 532 | 0.30 | 538 / 545 | 0.29 | 485 / 494 | 0.44 | 475 / 485 | 0.29 |
| cpucrc | 697 / 708 | 0.49 | 702 / 711 | 0.49 | 681 / 702 | 0.64 | 641 / 648 | 0.51 |
| dsacrc | 531 / 539 | 0.32 | 537 / 545 | 0.31 | 489 / 496 | 0.46 | 477 / 486 | 0.31 |

**Restore, depth 32 (median / p95 ms, CPU s):** base / cpu / dsa
783-786 / 791-793 (static) and 761-766 / 767-771 (dynamic), 0.72-0.75
core-s; cpucrc 913 / 921 and 890 / 897 at 0.85-0.87; dsacrc 787 / 799 and
768 / 775 at 0.73-0.75.

Attribution on this guest:

- **The DSA zero check now pays:** dsa vs cpu is 8-14 % less wall (48-81
  ms) and 0.07-0.09 core-s less CPU (15-24 %). The CPU scans 1.6 GiB of
  zeros on the thread that feeds IAA; DSA takes that off the critical path.
- **With CRC the gap widens:** dsacrc vs cpucrc is 23-28 % less wall
  (164-192 ms) and 28-39 % less daemon CPU (0.17-0.20 core-s). dsacrc is
  within 10 ms of dsa, so CRC on DSA stays free.
- **Software is still the largest step:** base → cpu is 2.4-2.9x on wall
  and 3.1-3.4x on CPU.
- **Restore** behaves as on the 4 KiB guest: CPU verification adds 125-127
  ms and 0.13 core-s, DSA verification 2-3 ms and ~0.01 core-s.

### 7c.2m 2 MiB guest restored into hugetlb

§7c.2l repeated with `RESTORE_HUGETLB=1`
(`benchmark_attrib_nvme_2m_hr.env`): our daemon restores into a 2 MiB
hugetlb memfd (`restore --hugetlb`) and decompresses straight into its
mapping. The unmodified daemon has no hugetlb option, so its row still
restores into 4 KiB shmem. Results:
`docs/measurements/harness-attrib-nvme-2m-hr/`.

**Restore, depth 32 (median / p95 ms, daemon CPU s):**

| step | static | CPU s | dynamic | CPU s |
|---|---|---|---|---|
| base (4 KiB shmem) | 791 / 797 | 0.75 | 770 / 780 | 0.73 |
| cpu | 648 / 657 | 0.61 | 630 / 639 | 0.59 |
| dsa | 650 / 654 | 0.61 | 631 / 636 | 0.59 |
| cpucrc (verify on CPU) | 772 / 782 | 0.73 | 747 / 765 | 0.71 |
| dsacrc (verify on DSA) | 655 / 661 | 0.62 | 633 / 643 | 0.59 |

Snapshot cells repeat §7c.2l within 3 %.

Reading:

- **hugetlb restore is 18 % faster and uses 19 % less CPU** than the 4 KiB
  restore of §7c.2l (cpu 786 → 648 ms, 0.75 → 0.61 core-s): 2048 huge page
  faults instead of ~1M small ones. It is software, not DSA (cpu = dsa).
- **CRC verification on DSA stays free** (2-5 ms, ≤0.01 core-s); on the
  CPU it adds 117-124 ms and 0.12 core-s. With hugetlb the restore is
  shorter, so the CPU verification is now 16 % of it.
- End to end on a 2 MiB guest with integrity, dsacrc against the unmodified
  daemon: snapshot 1428-1609 → 475-543 ms, restore 770-791 → 633-655 ms,
  daemon CPU 1.14-1.35 → 0.31-0.45 core-s per snapshot.

### 7c.2n What is in agent-guest memory, and constant chunks via DSA Compare Pattern

Raw memory dumps of every turn of the seven recorded agent sessions
(177 checkpoints, 4 GiB 4 KiB-page guests; `lifecycle.sh` with `CODEC=raw
KEEP_ALL=1 SKIP_RESTORE=1`), scanned page by page
(`docs/measurements/lifecycle-fill/agent_*.txt`, scripts alongside):

| per checkpoint, mean over 177 | |
|---|---|
| never-touched holes | 86-87 % of the guest |
| data extents the daemon classifies | 546 MiB |
| of which allocated zero pages | 97 MiB |
| non-zero data | 416-515 MiB |
| of which constant-filled pages (all 0xCC) | 88 MiB, identical in every dump |
| of which duplicates of another page | 55 MiB |
| zero detection at 4 KiB instead of 1 MiB | 5.4-6.0 % less IAA input |

The 0xCC pages are the kernel's `free_reserved_area` poison: Linux fills the
freed initramfs (73 MiB here, ending exactly at the top of low memory and
matching the 74 MiB initrd) and its init sections with 0xCC. With 3.9 GiB
free the guest never reuses them, they are allocated (so not holes), and
every full checkpoint compressed them. 90 % of them sit in whole 1 MiB
chunks, so chunk granularity is fine (as for zeros, §3) and a chunk-level
"constant" verdict catches them. They are never dirty, so diff checkpoints
(§7c.2f) never carried them; only full checkpoints pay.

**Implementation.** DSA Compare Pattern (COMPVAL, opcode 0x06) compares a
range against a repeating 8-byte pattern. DTO `ch-async-ops` @ 64b7dc0c adds
`dto_submit_compval` / `dto_batch_add_compval` (the pattern goes in the
descriptor's second address slot, `comp_pattern`; the first aliases
memfill's pattern and `src_addr`). The classifier now reads each chunk's
first qword and submits one COMPVAL with it in place of the COMPARE against
a zero buffer: a match means constant-filled (zero when the qword is 0),
so zero and poison detection cost the same single operation. The CPU
classifier does the same with `fill_pattern` (four u128 per branch: 36.9 vs
37.7 GB/s for the plain zero scan from cache, 18.5 vs 19.2 GB/s from DRAM).
A non-zero match becomes a manifest record with `fill: <pattern>` and no
data; restore writes those chunks from a pattern buffer on the CPU, in a
thread alongside the decompression. Verified byte-exact against a raw dump
with both classifiers (82 fill chunks found by each).

**Agent lifecycle, per-turn full checkpoint, all 7 sessions (170 turns),
daemon on 2 cores, default (shared) pipeline, median wall / daemon CPU
(`docs/measurements/lifecycle-fill/`):**

| variant | 4 KiB guest | 2 MiB hugetlb guest |
|---|---|---|
| base (unmodified daemon) | 1154 ms / 1.49 s | 1507 ms / 1.91 s |
| cpu (CPU zero+fill scan) | 132 ms / 0.12 s | 354 ms / 0.34 s |
| dsa (COMPVAL) | 109 ms / 0.10 s | 101 ms / 0.09 s |
| cpucrc | 159 ms / 0.14 s | 375 ms / 0.36 s |
| dsacrc | 109 ms / 0.10 s | 114 ms / 0.10 s |

DSA vs CPU: −17 % wall and −17 % CPU on 4 KiB, −71 % wall and −74 % CPU on
2 MiB (hugetlb has no holes, so the CPU scans 3.6 GiB of zeros per turn);
with per-chunk CRC the DSA variant costs nothing extra while the CPU one
pays 20-27 ms and 0.02 core-s more. Stored per turn 193 → 189-190 MiB.

**Same-day A/B of the fill change** (per-slot pipeline, 2 sessions, 23
turns; `lifecycle-prefill` = daemon e4ddc9c0b just before it):

| | 4 KiB cpu | 4 KiB dsa | 2 MiB cpu | 2 MiB dsa | MiB/turn |
|---|---|---|---|---|---|
| before fill | 97 / 0.13 | 98 / 0.13 | 257 / 0.35 | 127 / 0.20 | 199 |
| with fill | 96 / 0.13 | 96 / 0.11 | 255 / 0.35 | 88 / 0.11 | 189-190 |

The CPU variant is unchanged (it now scans the 80 MiB of poison to the end
instead of exiting at byte 0, and IAA no longer compresses it; the two
cancel), the DSA variant drops 31 % wall and 45 % CPU on 2 MiB and 15 % CPU
on 4 KiB, and 5 % less is stored. On 4 KiB the wall is set by the ~90 ms
outside compression (pause, memfd transfer, prefault, manifest), not IAA.

**Pipeline layout matters with two daemon cores.** The shared queue
(§7c.2j, now the default) runs both slots on one submitter thread; the
lifecycle harness pins the daemon to two cores, and there the per-slot
layout wins wall at a small CPU cost: 4 KiB cpu 121 → 96 ms, dsa 106 → 96;
2 MiB cpu 352 → 255, dsa 98 → 88 (`lifecycle-fill-shared` vs
`lifecycle-fill-per-slot2`; CPU 0.09 → 0.11 core-s for dsa). Section 7c.2o adds the
second submitter and per-thread output parts, after which the shared
pipeline is equal or faster than per-slot in every cell.

Restores are unchanged within noise (144-186 ms, CRC verification
included); one restore in roughly thirty takes ~500 ms with either daemon
generation (first-mapping cost, §7c.3), unrelated to this change. The
Oct 2 lifecycle figures (§7c.2e) were taken on the same host but read 25-30
ms lower on the 2 MiB cells than any run today, including the pre-fill
daemon's, so comparisons should stay within one day's runs.

### 7c.2o Two submitter threads for the shared pipeline

`--submitters N` (default 2) gives the shared pipeline N threads, each with
`--workers / N` IAA jobs. Getting it to beat the per-slot layout took four
steps, each measured on the lifecycle guest (4 GiB, 4 KiB pages, daemon on
two cores, `benchmark/agent/lifecycle.sh`, two sessions / 23 turns unless
noted; `docs/measurements/lifecycle-fill/submitters/`):

1. **Static slot ownership** (thread 0 owns the 3 GiB slot, thread 1 the
   1 GiB slot, one queue): 2 MiB CPU-classify 352 → 304 ms but daemon CPU
   0.34 → 0.54 core-s, and 4 KiB cells gained nothing for +50 % CPU. The
   thread with the small slot finished its scan early and spun.
2. **Work-stealing classification** (chunks of every slot claimed through a
   per-slot atomic cursor, one classifier per slot per thread): 2 MiB CPU
   229 ms / 0.39 core-s, now ahead of per-slot's 255, but 4 KiB stayed
   15-20 ms behind per-slot (120 vs 96 ms). Mapping the slots and creating
   the pools concurrently changed nothing; the A/B on one paused guest with
   rotated order reproduced the gap and per-thread accounting located it:
   the shared queue compressed 5.6 chunks/ms with 8, 16 or 32 jobs alike,
   while one per-slot thread with 8 jobs reached 6.9.
3. **One output part per thread.** Under `strace -c -w` the same 360 pwrites
   averaged 229-274 us in the shared layout against 132-137 us per-slot:
   two threads writing one file serialize on its inode lock, and a thread
   blocked in pwrite is not polling its IAA jobs. Each thread now writes
   `memory-N.partP.compressed` (manifest field `part`, default 0, so older
   manifests stay valid; restore opens the parts a manifest names; empty
   parts are removed). Compress phase 43 ms against per-slot's 54 and the
   single submitter's 66; checkpoint 80 vs 84 ms (CPU classify), 74 vs 83
   (DSA), performance governor.
4. **Writeback step 64 → 16 MiB.** On NVMe with one daemon core the final
   fsync of the shared layout took 81 ms against 20 for per-slot: with two
   parts per slot, twice as much dirty data was left for it.

**Agent lifecycle, all 7 sessions, 170 turns per page size, daemon on two
cores, on-demand governor** (median wall / daemon CPU per full checkpoint;
`lifecycle-fill6`; this morning's single-submitter run `lifecycle-fill` and
the per-slot layout, two sessions, `lifecycle-fill-per-slot2`, for scale):

| variant | 4 KiB: shared, 2 submitters | shared, 1 | per-slot | 2 MiB: shared, 2 | shared, 1 | per-slot |
|---|---|---|---|---|---|---|
| base daemon | 1153 / 1.49 | 1154 / 1.49 | | 1502 / 1.91 | 1507 / 1.91 | |
| cpu | **97 / 0.13** | 132 / 0.12 | 96 / 0.13 | **208 / 0.36** | 354 / 0.34 | 255 / 0.35 |
| dsa | **88 / 0.11** | 109 / 0.10 | 96 / 0.11 | **72 / 0.10** | 101 / 0.09 | 88 / 0.11 |
| cpucrc | **111 / 0.16** | 159 / 0.14 | | **218 / 0.38** | 375 / 0.36 | |
| dsacrc | **88 / 0.11** | 109 / 0.10 | | **84 / 0.12** | 114 / 0.10 | |

Against the single submitter the checkpoint is 19-27 % faster on 4 KiB and
26-41 % faster on 2 MiB at the same CPU; against per-slot it is equal or
faster everywhere (2 MiB: −18 %). DSA vs CPU: −9 % wall / −15 % CPU on
4 KiB, −65 % / −72 % on 2 MiB; per-chunk CRC free on DSA.

**NVMe harness, one daemon core** (`harness-pipeline-nvme-sub7`, depth 8,
median ms; `+cpu`/`+dsa` shared with two submitters, `+cpuslot`/`+dsaslot`
per-slot): dynamic Huffman 572 / 563 vs 629 / 619 (−9 %; the table build
now runs on two threads even with one core, because one thread's build
overlaps the other's waits); static Huffman 610 / 614 vs 608 / 599 (within
2 %; at depth 8 static output is written faster than the drive takes it, the
fsync tail is 62 ms, and both layouts sit on that floor). Restore unchanged
(761-786 ms).

Notes:
- The performance governor takes 8-10 ms off every checkpoint of this guest
  (per-slot 97 → 86 ms) without changing any ordering; all lifecycle numbers
  in this section are with the on-demand governor the host ran until
  section 7c.2p switched it.
- About one restore in thirty takes ~500 ms; the daemon's own span (connect
  to replay finished, now visible with microsecond log timestamps) is a
  normal 160-200 ms in those cases, so the time is outside the daemon.
- Idle submitter threads spin 200 rounds and then sleep 20 us per round.

### 7c.2p Submitters from the CPU affinity, performance governor

`--submitters` now defaults to the number of CPUs in the daemon's affinity
mask (`sched_getaffinity`), capped at `--workers`; an explicit value is
clamped the same way, and the daemon logs the choice
(`submitters=2 (affinity: 2 cpus, cap 8 = --workers)`). The lifecycle harness
pins the daemon to two cores and so gets two threads; the NVMe harness pins
it to one core and gets one. The host's CPU governor was switched from
on-demand to performance on every CPU at the same time (section 7c.2o had
measured it at 8-10 ms per checkpoint), and `lifecycle.sh` records the
governor of the daemon's cores in its header line.

**Agent lifecycle, all 7 sessions, 1770 checkpoints (0 failed), daemon on
two cores** (median wall ms / daemon CPU core-s per full checkpoint;
`lifecycle-perf7` against the morning's `lifecycle-fill6`, which ran the
same code with `--submitters 2` under the on-demand governor):

| variant | 4 KiB: performance | on-demand (7c.2o) | 2 MiB: performance | on-demand (7c.2o) |
|---|---|---|---|---|
| base daemon | 1129 / 1.46 | 1153 / 1.49 | 1392 / 1.77 | 1502 / 1.91 |
| cpu | **82 / 0.11** | 97 / 0.13 | **175 / 0.30** | 208 / 0.36 |
| dsa | **74 / 0.09** | 88 / 0.11 | **61 / 0.09** | 72 / 0.10 |
| cpucrc | **93 / 0.13** | 111 / 0.16 | **184 / 0.32** | 218 / 0.38 |
| dsacrc | **73 / 0.09** | 88 / 0.11 | **74 / 0.11** | 84 / 0.12 |

The governor is worth 13-16 % of wall and 15-20 % of daemon CPU on our
daemon (the same work at a higher clock takes fewer core-seconds) and 2-7 %
on the base daemon, whose checkpoint is dominated by the disk write. The
ordering is unchanged: DSA against CPU is −10 % wall / −18 % CPU on 4 KiB
and −65 % / −70 % on 2 MiB, and per-chunk CRC stays free on DSA. Restores
are 116-164 ms; 3 of 153 took more than 400 ms (the outliers of section
7c.2o, unchanged).

**NVMe harness, one daemon core** (`harness-pipeline-nvme-sub8`, depth 8,
median ms, performance governor, one submitter by affinity; `sub7` in
parentheses ran two submitters under on-demand): dynamic Huffman shared
`+cpu`/`+dsa` 548 / 544 (572 / 563) against per-slot 615 / 605 (629 / 619),
−11 %; static Huffman 602 / 597 (610 / 614) against 606 / 589 (608 / 599),
within 2 %. Restore 744-769 ms (769-794).

A control sweep (`harness-pipeline-nvme-sub9`: `--submitters 2` forced on
the one core, performance governor, `OFFLOAD_EXTRA_ARGS` in the env)
separates the two changes. Snapshot medians, one vs two submitters, both
under performance:

| codec | shared +cpu | +dsa | per-slot +cpuslot | +dsaslot |
|---|---|---|---|---|
| dynamic, 1 submitter (sub8) | 548 (62 % cpu) | 544 (63 %) | 615 | 605 |
| dynamic, 2 submitters (sub9) | 551 (70 %) | 549 (72 %) | 609 | 596 |
| static, 1 submitter (sub8) | 602 (53 %) | 597 (53 %) | 606 | 589 |
| static, 2 submitters (sub9) | 609 (57 %) | 603 (57 %) | 596 | 585 |

On one core a second submitter changes the wall by less than 1 % in every
shared cell (the per-slot cells, with identical code, scatter 1-2 % between
the two sweeps) and costs 4-9 points of daemon CPU: the thread that is not
running has nothing to overlap. The whole sub7 → sub8 move (572 / 563 →
548 / 544 dynamic) is the governor, as sub9 with two submitters lands on the
same 551 / 549. The affinity default (one thread per pinned core) is the
right choice here.

### 7c.2q Lifecycle stages: CPU only → DSA → IAA → DSA + IAA

Section 7b showed, for one snapshot, what each accelerator bought the
in-VMM chain stage by stage. This section does the same for the daemon
over the whole microVM lifecycle, against a **CPU-only baseline that runs
the same pipeline**. For that, `lz4` and `zstd` now go through the shared
full-checkpoint pipeline (3503c000a): the same mapped input, `SEEK_DATA`
hole map, CPU or DSA classification, per-thread output parts and 16 MiB
writeback as the IAA codecs, with a software engine that compresses each
chunk synchronously on the submitter thread. Only the compressor differs
between the cells.

Four cells, each with and without a per-chunk CRC, on the seven agent
sessions of section 7c.2e (170 turns, 177 checkpoints per cell including
the base), every variant checkpointing the same paused image, performance
governor, daemon pinned to two cores (so two submitter threads, which for
the software cells are the two compression threads), 1 MiB chunks,
`--workers 8`:

| cell | classification | compression | run |
|---|---|---|---|
| CPU only | CPU word scan | lz4 (or zstd −1) on the 2 daemon cores | `lifecycle-sw-lz4` / `-zstd`, `cpu` |
| DSA only | DSA COMPVAL | lz4 (or zstd −1) | same runs, `dsa` |
| IAA only | CPU word scan | IAA, static Huffman (`qpl-hardware-static-async`) | `lifecycle-perf7` (7c.2p), `cpu` |
| DSA + IAA | DSA COMPVAL | IAA, static Huffman | `lifecycle-perf7`, `dsa` |

The software runs took 1416 checkpoints each (0 failed), 112/112 restores
resumed the guest; lz4 and IAA static Huffman write the same amount
(193 vs 189 MiB per checkpoint, median), zstd −1 writes 162 MiB (−16 %) and
is the slower CPU codec, so lz4 is the CPU baseline below and zstd is given
where it differs. Results in `docs/measurements/lifecycle-fill/lifecycle-sw-*`;
`stages.py` and `totals.py` there produce every table.

**Full checkpoint per turn** (median over 170 turns; harness wall ms /
daemon core-s; the unmodified `iaa-integration` daemon from 7c.2p for scale):

| cell | 4 KiB | 4 KiB + CRC | 2 MiB | 2 MiB + CRC |
|---|---|---|---|---|
| CPU only, lz4 | 294 / 0.55 | 305 / 0.57 | 389 / 0.75 | 398 / 0.77 |
| CPU only, zstd | 398 / 0.76 | 411 / 0.79 | 496 / 0.97 | 508 / 0.99 |
| DSA only, lz4 | 284 / 0.53 | 284 / 0.53 | 266 / 0.51 | 271 / 0.52 |
| DSA only, zstd | 391 / 0.75 | 391 / 0.75 | 377 / 0.73 | 383 / 0.74 |
| IAA only | 82 / 0.11 | 93 / 0.13 | 175 / 0.30 | 184 / 0.32 |
| DSA + IAA | **74 / 0.09** | **73 / 0.09** | **61 / 0.09** | **74 / 0.11** |
| unmodified daemon | 1129 / 1.46 | | 1392 / 1.77 | |

**Stages of a full checkpoint** (last turn of each session, median over the
seven sessions, ms; the daemon's own timers, harness wall and CPU; lz4 for
the software cells, zstd in brackets where it matters). The pipeline
overlaps classification with compression, so the classification row is the
difference between the CPU and DSA columns of the same codec:

| stage | 4 KiB: CPU only | DSA only | IAA only | DSA + IAA | 2 MiB: CPU only | DSA only | IAA only | DSA + IAA |
|---|---|---|---|---|---|---|---|---|
| prepare (map, extents, prefault, job pools) | 12.6 | 12.8 | 20.5 | 19.8 | 1.4 | 1.5 | 8.8 | 9.1 |
| classify + compress + write parts | 327.5 (468) | 315.4 (457) | 50.1 | 41.6 | 433.9 (586) | 315.3 (470) | 160.6 | 50.7 |
| of which CPU classification | ~12 | — | ~9 | — | ~119 | — | ~110 | — |
| fsync | 0.3 | 0.2 | 0.1 | 0.2 | 0.2 | 0.2 | 0.2 | 0.2 |
| persist (manifest, snapshot files) | 10.6 | 9.8 | 7.5 | 7.5 | 0.5 | 0.4 | 0.9 | 0.9 |
| VMM + protocol | 4.7 | 5.1 | 6.7 | 6.2 | 5.0 | 4.2 | 5.9 | 5.7 |
| **harness wall** | **357 (494)** | **346 (482)** | **86** | **78** | **442 (593)** | **322 (477)** | **177** | **67** |
| daemon CPU, core-s | 0.68 (0.95) | 0.65 (0.93) | 0.12 | 0.10 | 0.85 (1.15) | 0.62 (0.93) | 0.31 | 0.09 |
| output, MiB | 216 (179) | 216 (179) | 210 | 210 | 214 (180) | 214 (180) | 212 | 212 |

Reading, stage by stage:

- **Classification is where DSA pays, and its value does not depend on the
  codec.** On 2 MiB hugetlb guests (no holes, all 4 GiB scanned) the CPU
  word scan costs 110-120 ms on two cores whether the compressor is lz4,
  zstd or IAA; DSA COMPVAL removes all of it from the critical path (the
  `dsa` column's compress time is the same as the 4 KiB column's, i.e. the
  scan of 4 GiB is fully hidden behind compression) and 0.23 core-s. As a
  share that is 27 % of a CPU-only checkpoint and 63 % of an IAA
  checkpoint. On 4 KiB guests `SEEK_DATA` already drops ~90 % of the guest
  before anything is read, the scan is ~10 ms, and DSA saves 3 %.
- **Compression is where IAA pays.** With classification on DSA, the kept
  chunks (0.45-1.1 GiB per checkpoint, by session) take 315 ms through lz4
  on two threads and 457-470 ms through zstd −1, against 42-51 ms on IAA at
  lz4's output size: 6-7× shorter and 0.62-0.65 → 0.09-0.10 core-s. The
  chain is multiplicative: IAA alone takes a 2 MiB checkpoint from 442 to
  177 ms, DSA alone to 322 ms, both together to 67 ms (6.6×; 5.3× on
  4 KiB where there is little to classify).
- **CRC is free on DSA** (the DSA cells with and without CRC are equal in
  every table) and costs 3-4 % on the CPU cells, 11-13 % on IAA-only.
- **Fixed costs** are 13-17 ms per checkpoint in the DSA + IAA cell, 20-25 %
  of it: IAA job-pool creation is ~8 ms of `prepare` (20.5 vs 12.6 ms,
  8.8 vs 1.4 ms against the software cells, which have no pools), the
  manifest `persist` 7.5-10 ms at 4 KiB, VMM and protocol 5-7 ms. The
  resident daemon (7c.2g) removes the pool setup.

**Restore and boot** (14 restores per cell: the last and the base
checkpoint of each session; harness wall / daemon span from connect to
replay / daemon core-s, ms):

| cell | 4 KiB | 2 MiB |
|---|---|---|
| CPU only, lz4 | 235 / 225 / 0.42 | 197 / 186 / 0.33 |
| CPU only, zstd | 239 / 229 / 0.43 | 203 / 193 / 0.35 |
| DSA only, lz4 | 230 / 219 / 0.40 | 192 / 181 / 0.32 |
| IAA only | 154 / 141 / 0.18 | 126 / 114 / 0.15 |
| DSA + IAA | 144 / 130 / 0.17 | 124 / 114 / 0.15 |
| boot to `AGENT_BASE_POINT` (median of 7) | 6190 | 5670 |

DSA has no part in restore (the ±10 ms between the CPU and DSA rows is
run-to-run scatter; the CRC verify of a software-codec checkpoint is a CPU
pass in every cell). IAA decompression saves 80-95 ms (4 KiB) and 70-80 ms
(2 MiB) and 2.2-2.5× daemon CPU against lz4. The 115-130 ms that remain are
the receive path and the copy into guest memory, which is the DSA
MEMMOVE / `uffd_dma` lever of 7c.2b, not yet in this branch. Restoring a
warm base instead of booting saves 5.5-6 s per session in every cell; it is
the largest stage lever and it is codec-independent.

**Diff checkpoints** (resident daemon, 7c.2g, on-demand governor): CPU
compare + copy 10 / 11 ms pause and 4.9 / 5.8 ms CPU (4 KiB / 2 MiB), DSA
COMPARE + DUALCAST 9 / 10 ms and 3.7 / 4.6 ms, IAA compressing ~3.6 MiB of
changed pages in 0.7-0.8 ms in both. A CPU-only diff cell cannot be run
(`serve` requires an async QPL codec); at lz4's rate on the gather buffer
it would add 3-7 ms per turn, i.e. 13-18 ms pause, so DSA + IAA is worth
about 1.5× on a diff checkpoint and 3-4× on its CPU, against 5-7× on a
full one.

**Whole sessions.** Replayed sessions have no LLM latency, so the session's
execution time is boot (or restore) + guest steps + checkpoint pauses +
the final restore. Over the seven sessions and both page sizes the guest
steps total 7.5-7.8 s (4 KiB) and 6.1-7.7 s (2 MiB) and seven boots
43.4 s / 39.7 s; the checkpoint column is the sum of the 177 harness walls
per cell:

| cell | 4 KiB: Σ checkpoints | session total | vs CPU only | daemon core-s | 2 MiB: Σ checkpoints | session total | vs CPU only | daemon core-s |
|---|---|---|---|---|---|---|---|---|
| CPU only, lz4 | 58.1 s | 110.6 s | — | 109 | 76.4 s | 125.2 s | — | 146 |
| CPU only, zstd | 80.4 s | 133.0 s | +20 % | 154 | 98.9 s | 147.7 s | +18 % | 191 |
| DSA only, lz4 | 56.1 s | 108.6 s | −2 % | 106 | 52.7 s | 101.4 s | −19 % | 101 |
| IAA only | 14.8 s | 66.8 s | −40 % | 19 | 34.2 s | 82.5 s | −34 % | 56 |
| DSA + IAA | **13.3 s** | **65.2 s** | **−41 %** | **17** | **11.1 s** | **59.4 s** | **−53 %** | **15.5** |
| unmodified daemon | 200.2 s | 252.0 s | +128 % | 258 | 244.3 s | 292.7 s | +134 % | 311 |

Estimates for the two configurations the branch is heading to, same
components (fork from the warm base = restore instead of boot; resident
diff from 7c.2g, 3.3 / 3.5 s of pauses and 0.25 s chain restores):

| configuration | 4 KiB | vs CPU-only cold | 2 MiB | vs CPU-only cold |
|---|---|---|---|---|
| CPU only lz4, cold boot, full checkpoints | 110.6 s | — | 125.2 s | — |
| DSA + IAA, cold boot, full checkpoints | 65.2 s | −41 % | 59.4 s | −53 % |
| CPU only lz4, fork from base, full checkpoints | 68.9 s | −38 % | 86.9 s | −31 % |
| DSA + IAA, fork from base, full checkpoints | 22.8 s | −79 % | 20.5 s | −84 % |
| DSA + IAA, fork from base, resident diffs | ~14 s | −88 % | ~14 s | −89 % |

Per turn the picture is: the agents' commands run 47 ms on average
(7.5 s / 158 steps). A CPU-only full checkpoint pauses the guest 6-8× that
(294-389 ms), DSA + IAA 1.3-1.6× (61-74 ms), a resident DSA diff 0.2×
(9-10 ms). With real LLM latency of seconds per turn the pause matters
less than its cost, and there the ladder is 109-146 → 15.5-17 core-s per
seven sessions (6.5-9.4× less CPU), i.e. ~0.09 core-s per full checkpoint
and ~0.004 per diff.

**Where gaps remain:**

- **No measured CPU-only diff cell** (at the time of writing): `serve`
  required an async QPL codec. Closed in 7c.2r: 10-11 ms measured against
  the 13-18 ms estimate above.
- **Resident full checkpoints were not re-measured** with the shared
  pipeline and the performance governor; the one-shot daemon pays ~8 ms of
  IAA job-pool setup per checkpoint. Closed in 7c.2r: 53 / 55 ms resident.
- **Restore gets nothing from DSA**, and after IAA decompression 115-130 ms
  remain, mostly the copy into guest memory; DSA MEMMOVE / `uffd_dma`
  populate (7c.2b) is the lever, and one in thirty restores takes >400 ms
  on the VMM side (7c.2o).
- **4 KiB guests leave DSA little to do** (3 %) because `SEEK_DATA` removes
  the holes first. Hugetlbfs reports no holes, so 2 MiB guests scan all
  4 GiB; a reserved-vs-faulted map of the huge pages (the resident daemon's
  populate bitmap is a start) would let the daemon skip untouched huge
  pages without any scan, CPU or DSA.
- **Fixed floor**: 13-17 ms of prepare, persist and VMM per checkpoint, and
  the harness's 50 ms socket poll before connect (outside the measured
  wall, but a real controller would see it unless it blocks on the socket).
- **Codec**: static Huffman matches lz4's size; zstd −1 is 16 % smaller.
  Dynamic Huffman beat static through the shared pipeline on NVMe (7c.2p)
  and has not been run in the lifecycle; it may give both the smaller
  output and a shorter pause.
- **The CPU-only cell has two compression threads** because the daemon is
  pinned to two cores. A CPU-only deployment spending more cores narrows
  the wall gap (lz4 scales) but not the core-seconds, which is what the
  tenant pays for.
- Operational, from 7c.4: the governor does not survive a reboot, an
  unpinned daemon gets one submitter per host CPU, the NVMe drive is not in
  fstab, COMPVAL is not used in the diff path, and KVM dirty logging's cost
  to a 2 MiB guest under diff checkpoints is unmeasured (guest time is tiny
  in these sessions).

### 7c.2r Resident diffs with software codecs, and the restore side

Two of section 7c.2q's gaps closed, one measured and left open.

**The resident daemon takes lz4/zstd.** `serve --compression lz4|zstd
[--zstd-level N]` builds a software engine per submitter thread instead of
the IAA job pools (`Engine::Software`, the same code path as the one-shot
daemon's); full checkpoints go through `compress_slots` and diffs through
`compress_gathers` with the same engine type. Verified as before (`verify_resident.sh` with
`lz4`: both chains restore identical to a raw dump except the one page a
plain restore rewrites). Lifecycle: `run_all_resident.sh 4k 2m` with
`VARIANTS="full-res diff-cpu-res diff-dsa-res"`, once with `CODEC=lz4` and
once with `qpl-hardware-static-async`, performance governor, daemon on two
cores: 1062 checkpoints per run, 0 failed, 42/42 restores resumed the
guest. Results in `docs/measurements/lifecycle-resident/lifecycle-res-{lz4,iaa}`.

Per-turn checkpoint, median over 170 turns (p95 in brackets) / daemon CPU
per checkpoint:

| cell | 4 KiB pause | 4 KiB CPU | 2 MiB pause | 2 MiB CPU |
|---|---|---|---|---|
| full, CPU only (lz4, DSA classify) | 275 ms (412) | 522 ms | 275 ms (396) | 525 ms |
| full, DSA + IAA | **53 ms (67)** | **73 ms** | **55 ms (64)** | **85 ms** |
| diff, CPU only (lz4 + CPU compare) | 10 ms (78) | 7.8 ms | 11 ms (77) | 8.3 ms |
| diff, DSA only (lz4 + DSA compare/gather) | 9 ms (71) | 6.3 ms | 10 ms (71) | 7.0 ms |
| diff, IAA only (IAA + CPU compare) | 6 ms (31) | 3.2 ms | 6 ms (36) | 3.6 ms |
| diff, DSA + IAA | **5 ms (21)** | **2.3 ms** | **5 ms (24)** | **2.7 ms** |

Diff phases (ms, 4 KiB / 2 MiB): compare 0.6 → 0.3 and gather 1.9 / 1.8 →
0.9 / 1.0 with DSA; compress of the ~600 KiB of changed pages 4.9 / 5.0 on
lz4 → 0.7 / 0.8 on IAA. Whole sessions (177 checkpoints × 7 sessions): diff
CPU only 7.3 / 8.1 s of pauses and 8.6 / 10.2 core-s; diff DSA + IAA 2.7 /
2.7 s and 2.4 / 2.5 core-s; full CPU only 53.7 / 56.4 s and 102 / 106
core-s; full DSA + IAA 9.9 / 12.1 s and 13.5 / 17.5 core-s.

Reading:

- **The CPU-only diff cell is 10-11 ms, not the 13-18 ms estimated in
  7c.2q**: lz4 compresses the diff in 4.7-5.0 ms, DSA + IAA takes the same
  checkpoint to 5 ms at 3-3.5× less CPU, and the tail is where the
  accelerators show most (p95 71-78 → 21-24 ms: the turns that dirty
  many pages are compress-bound on one lz4 thread).
- **Resident full checkpoints re-measured** with the shared pipeline and
  the governor: 53 / 55 ms against the one-shot daemon's 74 / 61 (7c.2q),
  i.e. the ~8 ms of IAA pool setup plus process start are gone; against
  7c.2g (80 / 148 ms) the 2 MiB cell gained most from the two submitter
  threads. The resident lz4 cell is at parity with one-shot lz4 (275 vs
  266-284 ms): compression-bound either way.
- With resident DSA + IAA diffs the fork-from-base session of 7c.2q
  becomes ~13 s (steps 7.5 + 7 base restores 1.2 + diffs 2.7 + 7 chain
  restores 1.8) against ~20 s with CPU-only resident diffs (lz4 base
  restores 0.27 s and chains 0.5 s each) and 69 / 87 s with CPU-only full
  checkpoints.
- Restores of lz4 chains take 450-512 ms (IAA 217-257): `apply_diff`
  decompresses each of up to 60 diffs on one thread, and the lz4 base
  restore is 218-270 ms against 137-171.

**The restore side.** The restore is a one-shot process with one thread
per memory slot: IAA (or software) decompresses `--workers`/slots chunks
in flight and each finished chunk is written to the destination memfd:
`pwrite` on a 4 KiB shmem memfd (the kernel allocates the page and copies),
a `memcpy` into a mapping on a hugetlb memfd (the kernel allocates and
clears the huge page on the fault, then the daemon copies). Fill chunks go
on a side thread, zero chunks stay holes. On the daemon's two cores it is
almost all system time (4 KiB: 0.01 user / 0.17 sys of a 0.14 s wall).
Profile of the daemon's samples (`perf record -e cpu-clock -C 4,5`,
`docs/measurements/restore-uffd/perf_*`):

| 4 KiB memfd | | 2 MiB hugetlb memfd | |
|---|---|---|---|
| `shmem_write_end` (page cache insert) | 23 % | `folio_zero_user` (clear on fault) | 33 % |
| `copy_folio_from_iter_atomic` (pwrite copy) | 17 % | libc `memcpy` into the mapping | 27 % |
| `_copy_to_iter` (reading the compressed parts) | 14 % | `_copy_to_iter` (reading the compressed parts) | 13 % |
| fault, page-cache, locks | ~10 % | fault | 3 % |

What the kernel on this host offloads (`7.2.0-mmoff-v1+`, `uffd_dma`
provider, `/sys/kernel/uffd_dma/offloading`): `UFFDIO_COPY` into
**anonymous private** VMAs in batches of ≥16 pages (`mfill_batch_applicable`
rejects `VM_SHARED`) and into **hugetlb** VMAs, shared included, through
`copy_user_large_folio`; shmem `mfill` and the shmem write path are not
hooked. Guest memory on the migration path is a shared memfd, so a 4 KiB
guest has no offloadable copy at all and a 2 MiB guest has one: the daemon
can `UFFDIO_COPY` into its own mapping of the hugetlb memfd. That is now
`restore --uffd-copy` (`Destination::Uffd`): map the memfd, register it
`MISSING`, install every decompressed chunk with one `UFFDIO_COPY`
(`EEXIST` falls back to a plain write), which needs whole huge pages, i.e. a
snapshot taken with `--chunk-size 2097152`, and `vm.unprivileged_userfaultfd=1`
(or access to `/dev/userfaultfd`). The provider refuses a source that is
not page aligned (the first run: `copies_refused=226`), so the IAA pool's
output buffers are now page aligned, 2 MiB aligned and `MADV_HUGEPAGE` from
2 MiB (`AlignedBuf` in `qpl.rs`; one physically contiguous source per copy).

Restore cells (`restore_cells2.sh`; write-compressor_30b at its last
turn, the same checkpoint restored 3× per cell into a fresh VMM over
`receive-migration`, daemon pinned to two cores unless noted, `--workers`
as given; harness wall ms, min-max of 3, and daemon user/sys s):

| snapshot | cell | wall | daemon CPU |
|---|---|---|---|
| 4 KiB, 1 MiB chunks | w32 (the lifecycle harness's setting) | 142-147 | 0.01 / 0.17 |
| | w32, 8 cores | 126-133 | 0.01 / 0.17 |
| | w16 | 125-161 | 0.00 / 0.15 |
| | **w8** | **111-127** | 0.00 / 0.15 |
| | w4 | 124-132 | 0.01 / 0.15 |
| | w32 `--verify-crc` (DSA) | 133-137 | 0.01 / 0.16 |
| 2 MiB, 1 MiB chunks | w32 `--hugetlb` | 117-122 | 0.04 / 0.11 |
| | w32, 8 cores | 117-121 | 0.05 / 0.10 |
| | w32 + kernel clear offload (`dcbm`) | 100 | 0.03 / 0.08 |
| | w8 | 102-120 | 0.03 / 0.09 |
| | **w8 + kernel clear offload** | **86-95** | 0.03 / 0.06 |
| 2 MiB, 2 MiB chunks | w32, mapping writes | 121-133 | 0.05 / 0.10 |
| | w32 + kernel clear offload | 119-128 | 0.04 / 0.10 |
| | w32 `--uffd-copy`, provider off (kernel CPU copy) | **107-113** | 0.01 / 0.12 |
| | w32 `--uffd-copy`, provider on (DSA) | 127-144 | 0.01 / 0.14 |
| | w32 `--uffd-copy`, provider on, 8 cores | 117-130 | 0.01 / 0.13 |

Reading:

- **4 KiB restores are bound by the kernel's shmem writes on the daemon's
  two cores**: 8 cores buy 11 %, and fewer IAA jobs in flight buy more
  (w8 is 12-22 % faster than w32: 8 MiB of output buffers instead of
  32 MiB in flight). There is nothing for DSA to do here until the kernel
  hooks shmem `mfill` (or the restore puts 4 KiB guests into hugetlb,
  which `--hugetlb` already does regardless of the source's page size,
  7c.2m).
- **2 MiB restores split between the clear and the copy.** The kernel
  clear offload (`dcbm`, already measured in 7c.2b) takes 15 % off, 25 %
  with w8 (86-95 ms). `UFFDIO_COPY` removes the clear and moves the copy
  into the kernel: 107-113 against 121-133 ms with the same 2 MiB chunks,
  the same gain as the clear offload, and the daemon's user time drops to
  0.01 s.
- **The DSA copy does not pay yet.** With the provider on, 189 of 226
  copies per restore go to DSA (396 MB, `striped_copies=189`) and the
  device time is small (`wait_ns_total` 8-12 ms per restore), but
  `map_us_total` is 44-72 ms: `uffd_dma` maps the 2 MiB source and
  destination for every copy in a DMA-FQ IOMMU domain with no mapping
  cache, 230-380 µs per copy, more than the copy itself. The wall lands
  at 127-144 ms. The 37 refused copies per run are the provider's
  admission gate, not alignment. A standing mapping of the daemon's
  (reused) output buffers, or the `dcbm` map cache applied to this
  provider, is what would make the DSA copy win; the copy itself is
  ~50 µs per 2 MiB.
- **2 MiB chunks cost ~10 ms on the plain path** (121-133 vs 117-122 ms
  with 1 MiB chunks: fewer, longer IAA jobs per slot thread), which the
  `UFFDIO_COPY` path wins back. A `UFFDIO_COPY` of two 1 MiB chunks would
  need a 2 MiB staging copy; chunk = huge page is the cleaner rule.
- **Reading the compressed input is 13-14 % of the daemon's time** in both
  cells (`pread` of ~200 MiB into the pool's input buffers); mapping the
  parts and feeding IAA from the page cache would remove it.

Restore follow-ups, in order of value: `--workers 8` as the restore default
(the harness runs 32); the kernel clear offload on for hugetlb restores
(per boot: `dcbm` `offloading=1`, kernel work queues from 7c.2b); a mapping
cache in `uffd_dma` (then `--uffd-copy` with the provider on, expected
≤95 ms); mmap'd compressed input; hugetlb destinations for 4 KiB guests or
a shmem `mfill` hook; and the ~60-80 ms that remain after all of that:
daemon start and connect (10-15 ms), IAA decompression of 450 MiB on one
thread per slot, and CH's state replay (4-10 ms).

### 7c.3 IOMMU first touch: measured, and the fix

`dto-async-test` (fresh process, 2 GiB memfd filled by another mapping, then
mapped afresh; 2048 × 1 MiB COMPAREs, 32 in flight):

| pages | no prefault | `MADV_POPULATE_READ` then compare | Translation Fetch then compare | warm pass |
|---|---|---|---|---|
| 2 MiB | 604 ms | 0.5 + 18 ms | 505 + 18 ms | 17 ms |
| 4 KiB | 1435 ms | 71 + 18 ms | 2022 + 18 ms | 17 ms |

The cost is page-table entries, not device translation caches: a page the
process has never touched has no PTE, the device's access becomes a
page-request-service fault (~300 µs each), and Translation Fetch takes the
same faults itself. `MADV_POPULATE_READ` creates the PTEs at kernel speed
(walk only, no data touch) and the first DSA pass is then as fast as a warm
one. The daemon does this per slot before classification (`prefault` on by
default: 0.6–2 ms per slot on hugetlb guests), and `--no-prefault` shows the
alternative (1357 ms). A caching-mappings approach is unnecessary once PTEs
exist, and would not survive the daemon's one-process-per-snapshot model
anyway.

One unexplained cost remains: the **first** mapping of a guest slot after
the VM is paused sometimes pays ~270 ms in the populate step (3 GiB hugetlb
slot; 2 ms on every later mapping of the same slot). It shows up as the
slower first cell in each sweep (`cpu` 492 vs 331 ms, `dsa` w8 473 vs 214 ms).

### 7c.4 Follow-ups

- The performance governor (section 7c.2p) is set with `scaling_governor`
  and does not survive a reboot; `lifecycle.sh` logs the daemon cores'
  governor in its header line so a result can be attributed either way.
- An unpinned daemon now gets one submitter per host CPU (capped at
  `--workers`, whose default is the same count); both harnesses pin the
  daemon, and a pinned run is the only one measured.
- The ~500 ms restore outliers (one in thirty) are outside the daemon
  (section 7c.2o): time the VMM side of receive-migration.
- The ~130 ms of a tmpfs snapshot spent outside compression (§7c.2j).
- Use DSA COMPVAL in the diff path too (pages without a reference are
  compared against a zero page; a COMPVAL with the page's first qword would
  also catch poison pages, §7c.2n).
- Mount the NVMe drive at boot (`/dev/nvme0n1` is not in fstab; it was
  mounted by hand for §7c.2k).
- Rebalance `--workers` across slots by kept-chunk count (slot 0 had 109
  kept chunks and 4 jobs; slot 1 had 997 and 4 jobs).
- Auto-select `--hugetlb` from the migration config's memory zones.
- Explain the first-mapping 270 ms.
- Run the lifecycle with dynamic Huffman through the shared pipeline
  (section 7c.2q).
- Restore (section 7c.2r): `--workers 8` default; kernel clear offload on
  for hugetlb restores; a mapping cache in `uffd_dma` so `--uffd-copy` with
  the DSA provider beats the kernel's CPU copy; mmap the compressed parts
  instead of `pread`; hugetlb destinations for 4 KiB guests or a shmem
  `mfill` hook in the kernel.
- `apply_diff` decompresses a chain one diff at a time on one thread
  (450-512 ms for 60 lz4 diffs, 217-257 ms on IAA; section 7c.2r).
- Classify granularity below the 1 MiB chunk is not needed (§3), so the
  in-VMM 64 KiB batching stays out.
- ~~Push `ch-async-ops` to a DTO fork~~: pushed to https://github.com/byrnedj/DTO branch
  `ch-async-ops` (53d3732); `build.rs` builds `libdto_explicit` from a checkout
  named by `DTO_SRC_DIR` and pins that revision.
- On-demand restore of compressed snapshots: decompress the faulting chunk
  into the memfd mapping with IAA and let CH `UFFDIO_CONTINUE` (§7c.2b).

## 7. Phases and validation

| phase | content | cell to run | pass criterion |
|---|---|---|---|
| 0 ✔ | mmap the slot memfd in the live async path, word-wide CPU scan | §7c.1 | 1120 → 331–492 ms |
| 1 ✔ | DTO `ch-async-ops` + `dto.rs` + two-stage ring, feature `dto` | §7c.1 | identical output/manifest; 115 ms / 0.20 core-s at w32 |
| 2 ✔ | per-chunk CRC32C via `dto_submit_crc`, manifest `crc32c`, DSA verify on restore | §7c.2 | corrupted manifest CRC refused; +14 ms on snapshot |
| 3 ✔/✘ | `--populate dsa` MEMFILL implemented and **measured slower** than `--populate cpu` on 4 KiB and 2 MiB pages (§7c.2); raw copy via DSA not done | §7c.2 | criterion not met; keep `cpu` |

The baseline measurement is in §7a: the CPU zero scan is 75–85 % of daemon
CPU and sets the wall on its own.

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
