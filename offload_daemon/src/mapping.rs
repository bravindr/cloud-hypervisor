// Copyright © 2026 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! A read-only or read-write mapping of a file range (guest memfd slot), so
//! the compression pipeline and the accelerators work on the memory in place
//! instead of copying every chunk through `pread`.

use std::alloc::{self, Layout};
use std::fs::File;
use std::io;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::ptr::{self, NonNull};
#[cfg(feature = "qpl")]
use std::slice;
use std::time::{Duration, Instant};

// Not yet in the libc crate version pinned by the workspace.
const MADV_POPULATE_READ: libc::c_int = 22;
const MADV_POPULATE_WRITE: libc::c_int = 23;

/// Data extents of `file` within `[offset, offset + length)`, relative to
/// `offset`, found with `SEEK_DATA`/`SEEK_HOLE`. Holes in a shmem memfd are
/// guest pages that were never written: they read as zero, and touching them
/// through a mapping makes shmem allocate a zeroed page for each, which
/// inflates the guest's host memory to its full size. Filesystems without
/// hole support (hugetlbfs) report the whole range as data.
#[cfg(feature = "qpl")]
pub(crate) fn data_extents(
    file: &File,
    offset: u64,
    length: u64,
) -> io::Result<Vec<(usize, usize)>> {
    let end = offset + length;
    let fd = file.as_raw_fd();
    let mut extents = Vec::new();
    let mut position = offset;
    while position < end {
        let start =
            libc::off_t::try_from(position).map_err(|_| io::Error::other("offset too large"))?;
        // SAFETY: lseek on an owned fd; the result is checked.
        let data = unsafe { libc::lseek(fd, start, libc::SEEK_DATA) };
        if data < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break; // no data past `position`
            }
            return Err(error);
        }
        let data = data as u64;
        if data >= end {
            break;
        }
        // SAFETY: as above.
        let hole = unsafe { libc::lseek(fd, data as libc::off_t, libc::SEEK_HOLE) };
        if hole < 0 {
            return Err(io::Error::last_os_error());
        }
        let hole = (hole as u64).min(end);
        extents.push(((data - offset) as usize, (hole - offset) as usize));
        position = hole;
    }
    Ok(extents)
}

pub(crate) struct Mapping {
    address: NonNull<u8>,
    length: usize,
    /// Backing page size: hugetlbfs page tables can only be populated in
    /// whole huge pages, so populate ranges are rounded out to it.
    page: usize,
}

const HUGETLBFS_MAGIC: i64 = 0x9584_58f6;

fn backing_page_size(file: &File) -> usize {
    // SAFETY: zeroed statfs buffer filled by the kernel; the result is checked.
    let mut stat: libc::statfs = unsafe { mem::zeroed() };
    // SAFETY: valid fd and out pointer.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } == 0 && stat.f_type == HUGETLBFS_MAGIC
    {
        stat.f_bsize as usize
    } else {
        4096
    }
}

impl Mapping {
    pub(crate) fn map_file(
        file: &File,
        offset: u64,
        length: u64,
        writable: bool,
    ) -> io::Result<Self> {
        let length = usize::try_from(length).map_err(|_| io::Error::other("range too large"))?;
        if length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot map an empty range",
            ));
        }
        let offset =
            libc::off_t::try_from(offset).map_err(|_| io::Error::other("offset too large"))?;
        let protection = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        // SAFETY: shared mapping of a file the caller owns; the kernel validates
        // the offset alignment and range, and we check the return value.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                protection,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                offset,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            address: NonNull::new(address.cast()).expect("mmap returned a null address"),
            length,
            page: backing_page_size(file),
        })
    }

    /// Create page-table entries for the whole mapping without touching the
    /// data (`MADV_POPULATE_READ`, or `_WRITE` to also allocate holes). A
    /// device accessing a mapping with no page-table entries takes one
    /// page-request fault per page: measured 604 ms (2 MiB pages) / 1435 ms
    /// (4 KiB) for 2 GiB versus 0.5 / 71 ms for this call.
    pub(crate) fn populate(&self, write: bool) -> io::Result<Duration> {
        let started = Instant::now();
        let advice = if write {
            MADV_POPULATE_WRITE
        } else {
            MADV_POPULATE_READ
        };
        // SAFETY: the range is this live mapping.
        if unsafe { libc::madvise(self.address.as_ptr().cast(), self.length, advice) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(started.elapsed())
    }

    /// Backing page size (2 MiB etc. on hugetlbfs, else 4 KiB).
    pub(crate) fn page_size(&self) -> usize {
        self.page
    }

    pub(crate) fn len(&self) -> usize {
        self.length
    }

    pub(crate) fn populate_range(
        &self,
        offset: usize,
        length: usize,
        write: bool,
    ) -> io::Result<()> {
        self.check_range(offset, length)?;
        let advice = if write {
            MADV_POPULATE_WRITE
        } else {
            MADV_POPULATE_READ
        };
        let start = offset / self.page * self.page;
        let end = (offset + length).div_ceil(self.page) * self.page;
        let length = end.min(self.length) - start;
        // SAFETY: the range lies inside this live mapping and starts on a
        // backing-page boundary, as madvise requires.
        if unsafe { libc::madvise(self.address.as_ptr().add(start).cast(), length, advice) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn check_range(&self, offset: usize, length: usize) -> io::Result<()> {
        if offset
            .checked_add(length)
            .is_none_or(|end| end > self.length)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "range {offset}+{length} exceeds mapping of {} bytes",
                    self.length
                ),
            ));
        }
        Ok(())
    }

    /// Pointer to `offset` within the mapping, range-checked.
    pub(crate) fn ptr(&self, offset: usize, length: usize) -> io::Result<*const u8> {
        self.check_range(offset, length)?;
        // SAFETY: offset is within the mapping.
        Ok(unsafe { self.address.as_ptr().add(offset) })
    }

    pub(crate) fn mut_ptr(&self, offset: usize, length: usize) -> io::Result<*mut u8> {
        self.ptr(offset, length).map(|pointer| pointer.cast_mut())
    }

    /// Copy `data` to `offset` in a writable mapping. Callers write each
    /// range once (manifest chunks are disjoint), so concurrent writers on
    /// one mapping never overlap.
    pub(crate) fn write(&self, offset: usize, data: &[u8]) -> io::Result<()> {
        let destination = self.mut_ptr(offset, data.len())?;
        // SAFETY: range-checked destination inside a live writable mapping;
        // the source is a distinct buffer.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), destination, data.len()) };
        Ok(())
    }

    /// Borrow `length` bytes at `offset`.
    #[cfg(feature = "qpl")]
    ///
    /// The mapping is shared with the VMM (snapshot source) or is the daemon's
    /// own memfd (restore destination); while a slice is live the contents
    /// are expected not to change. The snapshot protocol guarantees this: the
    /// VM is paused until the daemon acknowledges completion.
    pub(crate) fn slice(&self, offset: usize, length: usize) -> io::Result<&[u8]> {
        let pointer = self.ptr(offset, length)?;
        // SAFETY: range-checked pointer into a live mapping (see above).
        Ok(unsafe { slice::from_raw_parts(pointer, length) })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping the region created in `map_file`.
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.length) };
    }
}

// SAFETY: the mapping is plain memory; concurrent use is governed by the
// callers (one pipeline thread per slot).
unsafe impl Send for Mapping {}
// SAFETY: as above.
unsafe impl Sync for Mapping {}

/// A buffer that is page aligned, and 2 MiB aligned (and `MADV_HUGEPAGE`, so
/// one THP) once it holds 2 MiB or more: the IAA pool's output buffers and the
/// fill-chunk pattern. A chunk handed to `UFFDIO_COPY` must start on a page
/// boundary for the kernel's copy offload to take it (a `Vec` from malloc
/// starts 16 bytes into a page), and a single physically contiguous source
/// lets the device copy it in one go.
pub(crate) struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    cap: usize,
    align: usize,
    /// A view into another buffer's allocation: never freed or regrown.
    view: bool,
}

const BUF_PAGE: usize = 4096;
const BUF_HUGE: usize = 2 << 20;

impl AlignedBuf {
    pub(crate) const fn new() -> Self {
        Self {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
            align: 0,
            view: false,
        }
    }

    /// `cap` bytes at `ptr` inside an allocation someone else owns (and
    /// keeps alive for as long as this view): resizes stay within `cap`.
    pub(crate) fn view(ptr: *mut u8, cap: usize) -> Self {
        Self {
            ptr,
            len: 0,
            cap,
            align: 0,
            view: true,
        }
    }

    pub(crate) fn as_mut_ptr_at(&self, offset: usize) -> *mut u8 {
        assert!(offset <= self.cap);
        // SAFETY: within the allocation.
        unsafe { self.ptr.add(offset) }
    }

    /// Make the buffer `len` bytes long; contents are unspecified (the job
    /// writes them), zeroed on a fresh allocation.
    pub(crate) fn resize(&mut self, len: usize) {
        if len > self.cap {
            assert!(!self.view, "a buffer view cannot grow past its allocation");
            self.release();
            let align = if len >= BUF_HUGE { BUF_HUGE } else { BUF_PAGE };
            let cap = len.div_ceil(align) * align;
            let layout = Layout::from_size_align(cap, align).expect("valid buffer layout");
            // SAFETY: non-zero-sized layout.
            let p = unsafe { alloc::alloc_zeroed(layout) };
            if p.is_null() {
                alloc::handle_alloc_error(layout);
            }
            if align == BUF_HUGE {
                // SAFETY: advice on memory this buffer owns; failure is harmless.
                unsafe { libc::madvise(p.cast(), cap, libc::MADV_HUGEPAGE) };
            }
            self.ptr = p;
            self.cap = cap;
            self.align = align;
        }
        self.len = len;
    }

    /// The whole allocation, for registering it with the kernel.
    pub(crate) fn allocation(&self) -> Option<(*const u8, usize)> {
        (self.cap != 0).then(|| (self.ptr.cast_const(), self.cap))
    }

    fn release(&mut self) {
        if self.view {
            self.cap = 0;
            self.len = 0;
            return;
        }
        if self.cap != 0 {
            // SAFETY: allocated in `resize` with exactly this layout.
            unsafe {
                alloc::dealloc(
                    self.ptr,
                    Layout::from_size_align(self.cap, self.align).expect("valid buffer layout"),
                );
            };
            self.cap = 0;
            self.len = 0;
            self.ptr = ptr::null_mut();
        }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `len` <= `cap` bytes of zero-initialised or job-written memory.
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        if self.len == 0 {
            return &mut [];
        }
        // SAFETY: as above; exclusive through `&mut self`.
        unsafe { slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        self.release();
    }
}
