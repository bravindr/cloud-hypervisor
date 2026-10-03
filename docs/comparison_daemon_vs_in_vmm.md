# Side by side: `iaa-integration` (offload daemon) vs `djb-dsa-iaa` (in-VMM)

Two implementations of accelerated cloud-hypervisor snapshots exist in this
repository. This compares them row by row. Measurements are from gnr-qual1
(Granite Rapids, 4 DSA + 4 IAA devices per socket) on 2026-10-01 unless a row
says otherwise; the same-guest numbers are in §9.

| | `iaa-integration` | `djb-dsa-iaa` |
|---|---|---|
| **where** | `offload_daemon/` (separate process) + `benchmark/` | `vmm/src/{dsa,iaa,wait}.rs`, `memory_manager.rs` (inside the VMM) |
| **accelerators** | IAA via Intel QPL | DSA via raw ENQCMD + IAA via QPL |
| **what is offloaded** | deflate compression and decompression | zero classify, diff classify, gather, raw move, fill, deflate |
| **snapshot trigger** | `vm.send-migration` to the daemon's socket (`memory_mode=memfds`, `preserve_source=on`) | `vm.snapshot` API with extra body fields |
| **VMM changes** | none beyond upstream's existing offload/migration path | ~2.8 k lines, 1.5 k of them in `memory_manager.rs` |
| **added code** | 5.5 k lines: `compression.rs` 1060, `qpl_pipeline.rs` 637, `qpl.rs` 364, `main.rs` +242, `qpl_shim.c` 148, benchmark harness ~3 k | 2.8 k lines: `memory_manager.rs` +1506, `iaa.rs` 550, `dsa.rs` 476, `uffd.rs` +133, `wait.rs` 99, API/lib/vm glue ~85, plus `~/chiaa/iaashim.c` out of tree |
| **steady-state snapshot, warm 4 GiB guest** | 1.12 s, 1.35 core-s | 0.18 s, 0.15 core-s |
| **restore** | eager decompress into memfds; `--ondemand` uffd for uncompressed only | eager with IAA inflate + DSA raw move + DSA MEMFILL; on-demand via kernel mm-offload (separate series) |
| **diff snapshots** | no | yes (dirty log → DSA COMPARE against a base chain) |
| **maturity** | benchmark harness, unit tests, docs, multi-VM scaling runs | research prototype; validated on fio-style sweeps and real agent replays; no tests |

## 1. Process model and data flow

**Daemon.** Upstream cloud-hypervisor already lets a snapshot be handed to an
"offload daemon" that plays the live-migration peer: CH walks
`Start → MemoryFd×N → Config → State → CompletePaused`, passing one guest
memory backing fd per slot over `SCM_RIGHTS`. The `iaa-integration` work adds
codecs to the reference daemon (`--compression lz4|zstd|qpl-*`) and a
benchmark harness. Compression therefore runs in another process on a copy of
the fd, after the VM is paused, and CH blocks at `CompletePaused` until the
daemon has drained and fsynced every slot. Guest memory must be `shared=on`
or hugepage-backed (the migration precondition).

```
guest memfd ─SCM_RIGHTS─▶ daemon: per slot thread
                          ├─ pread 1 MiB chunk into pool buffer      (CPU copy)
                          ├─ iter().all(|b| b == 0)                  (CPU scan)
                          ├─ zero → manifest record, no payload
                          └─ else → QPL job (IAA) → pwrite → manifest
```

**In-VMM.** `vm.snapshot` gains `classify`, `dsa_gather`, `dsa_write`,
`zero_elide`, `iaa_compress`, `snapshot_type=diff`, `base_url`. The
MemoryManager drives DSA by ENQCMD into mmap'd shared-WQ portals and IAA
through QPL's job API, both from user space in the VMM, on the guest memory
it already maps. No copy, no second process, no protocol.

```
guest memory (mapped) ─▶ memory_manager
  ├─ DSA COMPARE vs zero (64 KiB, batched, 4 WQs)        classify
  ├─ (diff) DSA COMPARE vs base chain                    classify
  ├─ DSA MEMMOVE gather of kept runs                     gather
  ├─ IAA deflate, AsyncEngine, results land in mmap      compress
  └─ DSA MEMMOVE dense repack (optional)                 pack
```

| | daemon | in-VMM |
|---|---|---|
| source access | `read_exact_at` copy of every chunk (12 % of CPU, `hugetlbfs_read_iter`); an mmap variant exists in `qpl_pipeline.rs` but has no callers | guest mapping, zero copy |
| zero detection | CPU, byte-at-a-time (`opt-level = "s"`), ~2.6 GiB/s, one thread per slot | DSA COMPARE against a 2 MiB zero buffer, 64 KiB granules, OP_BATCH ×1024, 4 WQs: 32 ms per 4 GiB |
| parallelism | one thread per memory slot; `--workers` IAA jobs split across slots | one submitter thread; `CH_DSA_DEPTH` batches × WQs; `CH_IAA_ASYNC` IAA jobs in flight |
| compression unit | 1 MiB chunk (`--chunk-size`), whole chunk or nothing | 64 KiB chunk (`CH_SNAP_CHUNK_KB`), kept runs streamed to IAA as classify batches complete (`CH_SNAP_STREAM`) |
| crossing processes | fd passing + migration state machine; snapshot consistency guaranteed by the `CompletePaused` ACK | none |
| failure isolation | daemon crash cannot take the VMM down | engine or QPL fault is inside the VMM |

## 2. Accelerator access

| | daemon | in-VMM |
|---|---|---|
| DSA | not used | `dsa.rs`: `Dsa::open(CH_DSA_WQS)` mmaps portals; ops COMPARE, MEMMOVE, MEMFILL, OP_BATCH; flags BOF/CRAV/RCR/CC; completion polled through `wait.rs` |
| IAA | `qpl_shim.c` (create / submit_compress / check / wait / destroy) → `qpl.rs` `Job`/`JobPool`; static (`QPL_FLAG_...`) or dynamic Huffman; `-async` codecs keep a rolling pool, retry `QUEUES_ARE_BUSY`, 60 s bound | `iaashim.c` → `iaa.rs` `AsyncEngine` (submit / reap / run_all / profile / drain), `QUEUES_ARE_BUSY` retry, `warm_up()` on a helper thread, `numa_policy()` |
| wait policy | QPL `qpl_check_job` loop | `CH_WAIT = spin \| tpause[:ns] \| umwait[:us] \| sleep[:us]`; umwait monitors the DSA completion record (needs MSR 0x123 = 0) |
| NUMA | QPL `numa_id` any (their finding: all nodes allowed, "+46 %" matches ours) | `CH_IAA_NUMA`; DSA WQ list chosen per node by the caller |
| device configuration | `benchmark/configure_iaa_user`, `enable_iax_user_4` | `chsnap_dsa.sh`, `~/chiaa/chsnap_iaa_2g.sh` (shared WQs, BOF, PRS, 2 GiB mts) |
| CPU fallback | `lz4` / `zstd` codecs; QPL software path (`qpl-auto`) | `CH_SNAP_CPU_CLASSIFY/COMPRESS/COPY/GATHER` twins of every engine step; per-op fallback on engine refusal |
| verification | manifest checks on restore (zero chunk with payload, non-zero without) | `CH_DSA_VERIFY` re-checks every engine result on CPU; CRC per z-index entry |

## 3. On-disk format

| | daemon | in-VMM |
|---|---|---|
| raw mode | `memory-<slot>` sparse file (holes preserved via `vmm::sparse::copy_region`) | `memory-ranges` dense dump; with `zero_elide`, all-zero chunks become holes |
| compressed | `memory-<slot>.compressed` (chunks back to back) + `memory-<slot>.index.json` manifest `{codec, chunk_size, chunks:[{uncompressed_offset/length, compressed_offset/length, zero}]}` | `memory-ranges.z` slot file (64-byte header + chunk + 4 KiB slack per kept chunk, IAA writes into the mmap) + `memory-ranges.zidx` `{off, slot, orig_len, comp_len, raw, crc}` + `memory-ranges.idx` range table; optional `.zd` dense repack |
| zero chunks | manifest record, no payload | file hole + absent from `.zidx` |
| per-chunk integrity | none | CRC32 per entry |
| diff chains | none | `.idx` partition-point lookups across a `base_url` chain |
| config/state | upstream `config.json`/`state.json` via migration `Config`/`State` | upstream `config.json`/`state.json` |

## 4. Restore

| | daemon | in-VMM |
|---|---|---|
| eager | daemon creates one memfd per slot, decompresses chunks into it (`--workers` jobs), zero chunks stay holes, hands fds to CH over `receive-migration`; `--resume` optional | `fill_saved_regions*`: `CH_RESTORE_SRC = populate \| prs \| read \| odirect` feeds the engine, IAA inflate, `CH_RESTORE_RAW=dsa` MEMMOVEs raw chunks, `CH_RESTORE_FILL=dsa` MEMFILLs elided ranges |
| on-demand | `--ondemand`: empty memfds + uffd page serving from the daemon; **uncompressed snapshots only** | kernel-side: upstream `restore_by_uffd` + mm-offload `copy_user_pages` DSA (kernel `7.2.0-mmoff+`) with a CH fix to mmap the source and prefault in chunks |
| measured, cold 4 GiB from NVMe | not measured here; their harness reports restore p95 per codec | upstream 2.19 s / 0.90 core-s → IAA O_DIRECT + dense + DSA raw **0.49 s / 0.46 core-s**; decompress phase 17 ms; MEMFILL of 3.1 GiB elided 16 ms vs 0.36 s populate |
| what remains | CPU decompress-copy into memfd; guest first-touch of holes | guest first-touch page zeroing (0.35 s), the user-space floor |

## 5. Snapshot pipeline cost on the same guest

Warm synthetic 4 GiB guest at `AGENT_BASE_POINT`, node 0, output on the same
tmpfs. Daemon: `qpl-hardware-static-async --chunk-size 1 MiB --workers 8`.
In-VMM: `fullziaa`, 64 KiB, `CH_IAA_ASYNC=128`, spin, STREAM, 4 DSA WQs.

| | daemon | in-VMM 1st snapshot | in-VMM steady |
|---|---|---|---|
| wall | 1.12 s | 0.52 s | **0.18 s** |
| CPU | 1.35 core-s | 0.50 core-s | **0.15 core-s** |
| classify | ~1.0 core-s CPU scan, serial per slot | DSA 341 ms (336 ms first-touch IOMMU faults) | DSA 32 ms |
| kept | 1143 MiB (1 MiB granules) | 1076 MiB (64 KiB granules) | same |
| compressed | 441 MiB (0.39) | 437 MiB (0.397) | same |
| raw (uncompressed) path | 0.60 s / 0.80 s sys, hole-skipping | 0.84 s / 0.82 core-s dense `write()` | same |

On the `iaa-integration` harness (Silesia guest, 4 KiB pages, daemon pinned to
one core, `dsa_integration.md` §7c.2c) the snapshot is bound by the root disk (~430 MB/s with fsync,
§7c.2j), not IAA: the daemon variants cut snapshot wall 3-8 % (13-17 %
with early writeback) and CPU by half
(1.24–1.46 → 0.46–0.64 core-s) against the unmodified daemon, restore at
parity after a pwrite fix, and lz4/zstd 2.7–3.7× slower at 5–8× the CPU.

After the `dsa-integration` work (same guest, `--classify dsa --workers 32`,
see `dsa_integration.md` §7c): daemon **0.12 s / 0.20 core-s** with
byte-identical output, i.e. faster on wall than the in-VMM chain and within
0.05 core-s of it, while keeping the daemon's process model. DSA restore
populate was implemented and measured slower than `MADV_POPULATE_WRITE` on
both page sizes (the in-VMM 16 ms figure was on pre-populated memory).

Where the daemon's cycles go (perf): 75–85 % zero scan, 8–12 % pread copy,
3–5 % `iov_iter_zero` (pread of unbacked pages), 4 % pwrite, < 2 % QPL. The
wall is flat between a warm (455 MiB out) and an idle (161 MiB out) guest
because the 3 GiB slot's scan alone takes ~1.1 s. Compressed sizes agree
within 1 %, so IAA is doing the same work in both; the gap is entirely the
CPU stages in front of it.

On real Terminal-Bench agent guests (85–90 % zero at 64 KiB) the in-VMM chain
measured 57 ms per snapshot at 0.05 core-s (classify 37 ms, IAA 12 ms).

## 6. Knobs and operation

| | daemon | in-VMM |
|---|---|---|
| CLI / API | `--compression`, `--chunk-size`, `--workers`, `--zstd-level`, `--ondemand`, `--resume`, `--output-dir`/`--input-dir`, `--socket` | API body fields + ~30 `CH_*` env vars (`CH_DSA_*`, `CH_IAA_*`, `CH_WAIT`, `CH_SNAP_*`, `CH_RESTORE_*`) |
| build | feature `qpl`, static `libqpl.a` via `QPL_INCLUDE_DIR`/`QPL_LIB_DIR`, `cc` shim | `vmm/build.rs` links `~/chiaa/libiaashim.a`, `~/qpl/build/sources/libqpl.a`, accel-config, stdc++ (paths hard-wired to this box) |
| privileges | daemon needs the IAA WQ device nodes | CH needs DSA + IAA portals; run under `sudo` → root-owned API socket |
| host prerequisites | shared or hugepage guest memory; IAA user WQs | the same plus DSA shared WQs with BOF/PRS, `iommu=sm_on`, idxd `sva=Y`; `umwait` needs MSR 0x123 = 0; nothing persists across reboot |
| guest memory backing | `shared=on` default (4 KiB pages) | 2 MiB hugetlb in every measured cell |
| multi-VM | harness runs N VMs and daemons concurrently, SLA-qualified density | one VMM per guest, DSA/IAA WQs shared across VMMs by the hardware |
| upstream fit | codec plumbing is contained in the reference daemon, which upstream describes as "a template, not a production backend" | deep changes in the memory manager; would need splitting into a series |

## 7. What each should take from the other

For `dsa-integration` (the daemon):

1. DSA zero classify in front of the IAA ring (design in `dsa_integration.md`);
   it removes ~1.0 core-s and the serial scan from a 4 GiB snapshot.
2. Map the slot memfd instead of preading it; the mmap pipeline already exists.
3. DSA MEMFILL on restore when populated memory is wanted (16 ms vs 0.36 s).
4. Per-chunk CRC in the manifest (free from DSA COPY_CRC or IAA's CRC32).
5. The cold-translation question: a fresh process per snapshot faults every
   page through the IOMMU on first DSA touch (336 ms for 2048 hugetlb pages
   in our measurement; a 4 KiB-backed 4 GiB memfd is ~1 M faults). Resident
   daemon, hugepage guests or `MADV_POPULATE_READ` on the mapping are the
   candidates; this must be measured before any wall prediction holds.

For `djb-dsa-iaa` (the VMM):

1. Hole-skipping raw copy (their `copy_region`): 0.60 vs our 0.84 s.
2. Process isolation and the protocol-based trigger: no VMM code on the
   snapshot path, codecs swappable, a daemon crash is not a VMM crash.
3. A benchmark harness with SLA-qualified multi-VM density runs, unit tests,
   and `--chunk-size`/`--workers` as CLI rather than ~30 env vars.
4. A JSON manifest with explicit zero records instead of file holes; it is
   portable across filesystems that lack `SEEK_HOLE`.

## 8. Bottom line

The daemon is the better *shape*: contained, testable, and already the
upstream story for offloaded snapshots. The in-VMM chain is the better
*pipeline*: it is 6× faster and 9× cheaper on the same guest because it never
copies or scans guest memory on the CPU, and it has a restore side the daemon
lacks. The `dsa-integration` branch is the attempt to put the second inside
the first; its open risk is the per-process IOMMU warm-up that the in-VMM
chain only pays once per VMM lifetime.
