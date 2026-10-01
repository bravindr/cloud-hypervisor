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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_is_cache_line_aligned_and_sized() {
        assert_eq!(size_of::<Op>(), 192);
        assert_eq!(align_of::<Op>(), 64);
    }
}
