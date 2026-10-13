//! Minimal userfaultfd client for the restore side: the daemon registers its
//! own mapping of the destination memfd for missing-page faults and installs
//! each decompressed huge page with `UFFDIO_COPY`. The kernel allocates the
//! page and copies the data itself, and with a copy offload provider
//! (`mm_offload` `copy_user_pages`, the `uffd_dma` module) that copy runs on
//! DSA instead of the daemon's CPU. Nothing else may touch a registered page
//! before it is copied in: a plain access would block on a fault this process
//! never serves.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const UFFD_API: u64 = 0xAA;
// _IOWR(UFFDIO, nr, struct) for the three ioctls used here.
const UFFDIO_API: libc::c_ulong = 0xc018_aa3f;
const UFFDIO_REGISTER: libc::c_ulong = 0xc020_aa00;
const UFFDIO_COPY: libc::c_ulong = 0xc028_aa03;
// _IO(USERFAULTFD_IOC, 0x00): new fd from /dev/userfaultfd.
const USERFAULTFD_IOC_NEW: libc::c_ulong = 0xaa00;
const UFFDIO_REGISTER_MODE_MISSING: u64 = 1;
const UFFD_USER_MODE_ONLY: libc::c_int = 1;

#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

pub(crate) struct Uffd {
    fd: OwnedFd,
}

impl Uffd {
    /// `/dev/userfaultfd` when it is accessible, else the syscall (which
    /// needs `vm.unprivileged_userfaultfd=1` or CAP_SYS_PTRACE).
    pub(crate) fn new() -> io::Result<Self> {
        let raw = match OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/userfaultfd")
        {
            Ok(dev) => {
                // SAFETY: ioctl on an open descriptor with an integer argument.
                let fd = unsafe {
                    libc::ioctl(
                        dev.as_raw_fd(),
                        USERFAULTFD_IOC_NEW,
                        libc::O_CLOEXEC | libc::O_NONBLOCK,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                fd
            }
            Err(_) => {
                // SAFETY: plain syscall with integer flags.
                let fd = unsafe {
                    libc::syscall(
                        libc::SYS_userfaultfd,
                        libc::O_CLOEXEC | libc::O_NONBLOCK | UFFD_USER_MODE_ONLY,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                fd as libc::c_int
            }
        };
        // SAFETY: a descriptor this process just created and owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut api = UffdioApi {
            api: UFFD_API,
            features: 0,
            ioctls: 0,
        };
        // SAFETY: valid fd; the argument is a correctly laid out struct.
        if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_API, &mut api) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Register `len` bytes at `addr` (page aligned) for missing-page faults.
    pub(crate) fn register_missing(&self, addr: *const u8, len: usize) -> io::Result<()> {
        let mut reg = UffdioRegister {
            range: UffdioRange {
                start: addr as u64,
                len: len as u64,
            },
            mode: UFFDIO_REGISTER_MODE_MISSING,
            ioctls: 0,
        };
        // SAFETY: valid fd; the argument is a correctly laid out struct.
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), UFFDIO_REGISTER, &mut reg) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Install `len` bytes from `src` at the missing page(s) at `dst`.
    /// Returns `Ok(false)` when a page is already present there (nothing
    /// was copied, the caller writes through the mapping instead).
    pub(crate) fn copy(&self, dst: *mut u8, src: *const u8, len: usize) -> io::Result<bool> {
        let mut cp = UffdioCopy {
            dst: dst as u64,
            src: src as u64,
            len: len as u64,
            mode: 0,
            copy: 0,
        };
        loop {
            // SAFETY: valid fd; the argument is a correctly laid out struct and
            // the kernel validates both address ranges.
            if unsafe { libc::ioctl(self.fd.as_raw_fd(), UFFDIO_COPY, &mut cp) } == 0 {
                return Ok(true);
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EEXIST) if cp.copy <= 0 => return Ok(false),
                // The mapping was changing under the call: continue after
                // whatever was copied.
                Some(libc::EAGAIN) => {
                    let done = cp.copy.max(0) as u64;
                    cp.dst += done;
                    cp.src += done;
                    cp.len -= done;
                    cp.copy = 0;
                    if cp.len == 0 {
                        return Ok(true);
                    }
                }
                _ => return Err(error),
            }
        }
    }
}
