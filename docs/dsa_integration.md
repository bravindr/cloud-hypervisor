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
| build | feature `dto`, `DTO_LIB_DIR` | links `libdto_explicit` dynamically with an rpath |

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

- Rebalance `--workers` across slots by kept-chunk count (slot 0 had 109
  kept chunks and 4 jobs; slot 1 had 997 and 4 jobs).
- Auto-select `--hugetlb` from the migration config's memory zones.
- Explain the first-mapping 270 ms.
- Classify granularity below the 1 MiB chunk is not needed (§3), so the
  in-VMM 64 KiB batching stays out.
- Push `ch-async-ops` to a DTO fork; the daemon depends on it.

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
