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

- Two submitter threads draining the shared ready queue, so dynamic
  Huffman table builds run in parallel (§7c.2j: per-slot is 4-5 % faster
  at depth 16/32 for that reason).
- The ~130 ms of a tmpfs snapshot spent outside compression (§7c.2j).
- Rebalance `--workers` across slots by kept-chunk count (slot 0 had 109
  kept chunks and 4 jobs; slot 1 had 997 and 4 jobs).
- Auto-select `--hugetlb` from the migration config's memory zones.
- Explain the first-mapping 270 ms.
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
