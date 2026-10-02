// Copyright © 2026 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Intel DSA through DTO's explicit asynchronous API (`libdto_explicit`).
//!
//! DTO owns the work queues (`DTO_WQ_LIST`, `DTO_IS_NUMA_AWARE`, ...), the
//! ENQCMD path and the fallback policy; this module only wraps the
//! `dto_async_op` lifecycle in Rust and provides the three operations the
//! snapshot pipeline needs: COMPARE against a zero buffer (classify), CRC
//! generation (per-chunk integrity) and MEMFILL (restore populate). Every
//! submit may legitimately fall back (`Submit::Fallback`): the caller then
//! does the same work on the CPU.

use std::error::Error as StdError;
use std::hint;
use std::os::raw::{c_int, c_void};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fmt, io};

/// Opaque DTO operation state (`dto_async_op`): a 64-byte descriptor plus the
/// device-written completion record. Must stay at this address while in flight.
#[repr(C, align(64))]
pub(crate) struct Op([u8; 192]);

impl Default for Op {
    fn default() -> Self {
        Self([0; 192])
    }
}

const DTO_ASYNC_SUBMITTED: c_int = 0;
const DTO_ASYNC_PENDING: c_int = 0;
const DTO_ASYNC_DONE: c_int = 1;

unsafe extern "C" {
    fn dto_submit_compare(op: *mut Op, src1: *const c_void, src2: *const c_void, n: usize)
    -> c_int;
    fn dto_submit_memfill(
        op: *mut Op,
        dest: *mut c_void,
        pattern: u64,
        n: usize,
        cache_control: c_int,
    ) -> c_int;
    fn dto_submit_crc(op: *mut Op, src: *const c_void, n: usize) -> c_int;
    fn dto_async_poll(op: *mut Op) -> c_int;
    fn dto_async_result(op: *const Op) -> c_int;
    fn dto_async_status(op: *const Op) -> c_int;
    fn dto_async_crc_val(op: *const Op) -> u64;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Submit {
    Submitted,
    /// DTO did nothing (no usable work queue, below its size gate, queue full):
    /// the caller performs the operation on the CPU.
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Poll {
    Pending,
    Done,
    /// The device completed with an error status (`status` is the DSA
    /// completion status byte). The operation must be redone on the CPU.
    Failed {
        status: u8,
    },
}

#[derive(Debug)]
pub(crate) struct Failed {
    pub status: u8,
}

impl fmt::Display for Failed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "DSA operation failed with status {:#x}",
            self.status
        )
    }
}

impl StdError for Failed {}

impl From<Failed> for io::Error {
    fn from(failed: Failed) -> Self {
        io::Error::other(failed)
    }
}

/// Zero reference buffer for COMPARE-based classification: one chunk long,
/// populated, huge-page backed when the kernel allows.
pub(crate) struct ZeroBuffer {
    address: NonNull<u8>,
    length: usize,
}

impl ZeroBuffer {
    pub(crate) fn new(length: usize) -> io::Result<Self> {
        // SAFETY: anonymous private mapping; populated so the device never
        // faults on the reference side.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
                -1,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: freshly mapped region of `length` bytes.
        unsafe { libc::madvise(address, length, libc::MADV_HUGEPAGE) };
        Ok(Self {
            address: NonNull::new(address.cast()).expect("mmap returned null"),
            length,
        })
    }

    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.address.as_ptr()
    }
}

impl Drop for ZeroBuffer {
    fn drop(&mut self) {
        // SAFETY: unmapping the region created in `new`.
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
    }
}

// SAFETY: the buffer is never written after creation.
unsafe impl Send for ZeroBuffer {}
// SAFETY: as above.
unsafe impl Sync for ZeroBuffer {}

/// Counters for the log line at the end of a slot.
#[derive(Default)]
pub(crate) struct Stats {
    pub submitted: AtomicU64,
    pub fallback: AtomicU64,
    pub failed: AtomicU64,
}

impl Stats {
    fn count(&self, submit: Submit) {
        match submit {
            Submit::Submitted => self.submitted.fetch_add(1, Ordering::Relaxed),
            Submit::Fallback => self.fallback.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub(crate) fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.submitted.load(Ordering::Relaxed),
            self.fallback.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }
}

fn submit_result(rc: c_int) -> Submit {
    if rc == DTO_ASYNC_SUBMITTED {
        Submit::Submitted
    } else {
        Submit::Fallback
    }
}

impl Op {
    /// COMPARE `n` bytes at `src1` against `src2`.
    ///
    /// # Safety
    /// Both ranges must stay mapped and unchanged until the operation
    /// completes, and `self` must not move while in flight.
    pub(crate) unsafe fn submit_compare(
        &mut self,
        src1: *const u8,
        src2: *const u8,
        n: usize,
        stats: &Stats,
    ) -> Submit {
        // SAFETY: forwarded to DTO under the caller's guarantees.
        let submit =
            submit_result(unsafe { dto_submit_compare(self, src1.cast(), src2.cast(), n) });
        stats.count(submit);
        submit
    }

    /// CRC generation over `n` bytes at `src` (DSA CRC: CRC32C).
    ///
    /// # Safety
    /// As for [`Op::submit_compare`].
    pub(crate) unsafe fn submit_crc(&mut self, src: *const u8, n: usize, stats: &Stats) -> Submit {
        // SAFETY: forwarded to DTO under the caller's guarantees.
        let submit = submit_result(unsafe { dto_submit_crc(self, src.cast(), n) });
        stats.count(submit);
        submit
    }

    /// MEMFILL `n` bytes at `dest` with zeroes.
    ///
    /// # Safety
    /// `dest` must stay mapped and exclusively owned until completion.
    pub(crate) unsafe fn submit_memfill_zero(
        &mut self,
        dest: *mut u8,
        n: usize,
        stats: &Stats,
    ) -> Submit {
        // SAFETY: forwarded to DTO under the caller's guarantees.
        let submit = submit_result(unsafe { dto_submit_memfill(self, dest.cast(), 0, n, 1) });
        stats.count(submit);
        submit
    }

    pub(crate) fn poll(&mut self, stats: &Stats) -> Poll {
        // SAFETY: `self` holds a submitted operation.
        match unsafe { dto_async_poll(self) } {
            DTO_ASYNC_PENDING => Poll::Pending,
            DTO_ASYNC_DONE => Poll::Done,
            _ => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                // SAFETY: completion record is valid after a non-pending poll.
                let status = unsafe { dto_async_status(self) } as u8;
                Poll::Failed { status }
            }
        }
    }

    /// COMPARE result after `Poll::Done`: true when the ranges differ.
    pub(crate) fn differs(&self) -> bool {
        // SAFETY: completion record of a finished operation.
        unsafe { dto_async_result(self) != 0 }
    }

    /// CRC value after `Poll::Done`.
    pub(crate) fn crc(&self) -> u32 {
        // SAFETY: completion record of a finished operation.
        unsafe { dto_async_crc_val(self) as u32 }
    }
}

#[repr(C)]
struct RawBatch {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn dto_batch_create(capacity: c_int) -> *mut RawBatch;
    fn dto_batch_destroy(batch: *mut RawBatch);
    fn dto_batch_reset(batch: *mut RawBatch);
    fn dto_batch_add_compare(
        batch: *mut RawBatch,
        src1: *const c_void,
        src2: *const c_void,
        n: usize,
    ) -> c_int;
    fn dto_batch_add_memmove(
        batch: *mut RawBatch,
        dst: *mut c_void,
        src: *const c_void,
        n: usize,
        cache_control: c_int,
    ) -> c_int;
    fn dto_batch_add_dualcast(
        batch: *mut RawBatch,
        dst1: *mut c_void,
        dst2: *mut c_void,
        src: *const c_void,
        n: usize,
        cache_control: c_int,
    ) -> c_int;
    fn dto_batch_submit(batch: *mut RawBatch) -> c_int;
    fn dto_batch_poll(batch: *mut RawBatch) -> c_int;
    fn dto_batch_status(batch: *const RawBatch, i: c_int) -> c_int;
    fn dto_batch_result(batch: *const RawBatch, i: c_int) -> c_int;
}

/// One DSA batch descriptor (DTO `dto_batch`): up to `capacity` operations
/// submitted with a single ENQCMD and completed with a single poll.
pub(crate) struct Batch {
    raw: NonNull<RawBatch>,
    submitted: bool,
}

// SAFETY: the batch is owned by one thread at a time; DTO keeps no
// thread-local references to it.
unsafe impl Send for Batch {}

impl Batch {
    pub(crate) fn new(capacity: usize) -> io::Result<Self> {
        let capacity = c_int::try_from(capacity.clamp(1, 1024)).unwrap_or(1024);
        // SAFETY: plain allocation call; NULL is checked.
        let raw = unsafe { dto_batch_create(capacity) };
        let raw = NonNull::new(raw).ok_or_else(|| io::Error::other("dto_batch_create failed"))?;
        Ok(Self {
            raw,
            submitted: false,
        })
    }

    pub(crate) fn reset(&mut self) {
        debug_assert!(!self.submitted || self.poll() != BatchPoll::Pending);
        // SAFETY: valid batch with no operation in flight.
        unsafe { dto_batch_reset(self.raw.as_ptr()) };
        self.submitted = false;
    }

    /// # Safety
    /// Both ranges must stay mapped and unchanged until the batch completes.
    pub(crate) unsafe fn add_compare(
        &mut self,
        src1: *const u8,
        src2: *const u8,
        n: usize,
    ) -> bool {
        // SAFETY: forwarded under the caller's guarantees.
        unsafe { dto_batch_add_compare(self.raw.as_ptr(), src1.cast(), src2.cast(), n) >= 0 }
    }

    /// # Safety
    /// `src` must stay mapped and `dst` exclusively owned until completion.
    pub(crate) unsafe fn add_memmove(&mut self, dst: *mut u8, src: *const u8, n: usize) -> bool {
        // SAFETY: forwarded under the caller's guarantees.
        unsafe { dto_batch_add_memmove(self.raw.as_ptr(), dst.cast(), src.cast(), n, 0) >= 0 }
    }

    /// Copy `src` to both destinations; bits 11:0 of `dst1` and `dst2` must
    /// match (DSA rule). Returns false when the batch is full or they do not.
    ///
    /// # Safety
    /// As for [`Batch::add_memmove`], for both destinations.
    pub(crate) unsafe fn add_dualcast(
        &mut self,
        dst1: *mut u8,
        dst2: *mut u8,
        src: *const u8,
        n: usize,
    ) -> bool {
        // SAFETY: forwarded under the caller's guarantees.
        unsafe {
            dto_batch_add_dualcast(
                self.raw.as_ptr(),
                dst1.cast(),
                dst2.cast(),
                src.cast(),
                n,
                0,
            ) >= 0
        }
    }

    pub(crate) fn submit(&mut self) -> Submit {
        // SAFETY: valid batch; the operands are covered by the add_* contracts.
        let submit = submit_result(unsafe { dto_batch_submit(self.raw.as_ptr()) });
        self.submitted = submit == Submit::Submitted;
        submit
    }

    pub(crate) fn poll(&mut self) -> BatchPoll {
        // SAFETY: valid batch.
        match unsafe { dto_batch_poll(self.raw.as_ptr()) } {
            DTO_ASYNC_PENDING => BatchPoll::Pending,
            DTO_ASYNC_DONE => BatchPoll::Done,
            _ => BatchPoll::Failed,
        }
    }

    /// DSA completion status of operation `i` (1 = success).
    pub(crate) fn status(&self, i: usize) -> u8 {
        // SAFETY: valid batch and index below len().
        unsafe { dto_batch_status(self.raw.as_ptr(), i as c_int) as u8 }
    }

    /// COMPARE result of operation `i`: true when the ranges differ.
    pub(crate) fn differs(&self, i: usize) -> bool {
        // SAFETY: as above.
        unsafe { dto_batch_result(self.raw.as_ptr(), i as c_int) != 0 }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BatchPoll {
    Pending,
    Done,
    /// The batch reported an error: read each operation's status and redo
    /// the ones that did not succeed on the CPU.
    Failed,
}

impl Drop for Batch {
    fn drop(&mut self) {
        if self.submitted {
            while self.poll() == BatchPoll::Pending {
                hint::spin_loop();
            }
        }
        // SAFETY: created by dto_batch_create, no operation in flight.
        unsafe { dto_batch_destroy(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_is_cache_line_aligned_and_sized() {
        assert_eq!(size_of::<Op>(), 192);
        assert_eq!(align_of::<Op>(), 64);
    }
}
