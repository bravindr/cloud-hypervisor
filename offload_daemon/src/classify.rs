// Copyright © 2026 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Stage one of the snapshot pipeline: decide per chunk whether it is all
//! zero (manifest record, no payload) or must be compressed, and optionally
//! compute its CRC32C. On DSA this is a ring of asynchronous COMPARE (+ CRC)
//! operations through DTO that runs ahead of the IAA job pool; on the CPU it
//! is a word-wide scan. Either way the output is the same `Verdict`.

#[cfg(feature = "qpl")]
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::str::FromStr;

use thiserror::Error;

#[cfg(feature = "qpl")]
use crate::crc::{crc32c, is_zero};
#[cfg(feature = "qpl")]
use crate::mapping::Mapping;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Classify {
    Cpu,
    Dsa,
}

impl fmt::Display for Classify {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cpu => "cpu",
            Self::Dsa => "dsa",
        })
    }
}

#[derive(Debug, Error)]
#[error("Unknown classify mode {0:?} (expected cpu or dsa)")]
pub(crate) struct ParseClassifyError(String);

impl FromStr for Classify {
    type Err = ParseClassifyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cpu" => Ok(Self::Cpu),
            "dsa" => Ok(Self::Dsa),
            _ => Err(ParseClassifyError(value.to_owned())),
        }
    }
}

/// Accelerator-related options for the snapshot side.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(feature = "qpl"), allow(dead_code))]
pub(crate) struct AccelOptions {
    pub classify: Classify,
    /// COMPARE operations kept in flight per slot thread.
    pub dsa_depth: usize,
    /// Record a CRC32C per compressed chunk.
    pub crc: bool,
    /// `MADV_POPULATE_READ` the source mapping before the engines touch it.
    pub prefault: bool,
}

impl Default for AccelOptions {
    fn default() -> Self {
        Self {
            classify: Classify::Cpu,
            dsa_depth: 32,
            crc: false,
            prefault: true,
        }
    }
}

#[derive(Debug, Error)]
#[cfg_attr(not(feature = "qpl"), allow(dead_code))]
pub(crate) enum Error {
    #[cfg_attr(feature = "dto", allow(dead_code))]
    #[error("DSA classification requires the dto feature")]
    DsaUnavailable,
    #[error("Allocating the zero reference buffer")]
    #[cfg_attr(not(feature = "dto"), allow(dead_code))]
    ZeroBuffer(#[source] io::Error),
    #[error("Source mapping")]
    Mapping(#[source] io::Error),
}

#[cfg(feature = "qpl")]
/// A classified chunk.
pub(crate) struct Verdict {
    pub offset: usize,
    pub length: usize,
    pub zero: bool,
    pub crc32c: Option<u32>,
}

#[cfg(feature = "qpl")]
/// Counters reported at the end of a slot.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Report {
    pub dsa_submitted: u64,
    pub dsa_fallback: u64,
    pub dsa_failed: u64,
    pub cpu_scans: u64,
}

#[cfg(feature = "qpl")]
/// Feeds chunks in, hands verdicts out. `push` is non-blocking on DSA (it
/// returns false when the ring is full); `drain` collects what has completed.
pub(crate) enum Classifier {
    Cpu {
        want_crc: bool,
        done: VecDeque<Verdict>,
        report: Report,
    },
    #[cfg(feature = "dto")]
    Dsa(dsa::Ring),
}

#[cfg(feature = "qpl")]
impl Classifier {
    pub(crate) fn new(options: AccelOptions, chunk_size: usize) -> Result<Self, Error> {
        match options.classify {
            Classify::Cpu => Ok(Self::Cpu {
                want_crc: options.crc,
                done: VecDeque::new(),
                report: Report::default(),
            }),
            #[cfg(feature = "dto")]
            Classify::Dsa => Ok(Self::Dsa(dsa::Ring::new(options, chunk_size)?)),
            #[cfg(not(feature = "dto"))]
            Classify::Dsa => {
                let _ = chunk_size;
                Err(Error::DsaUnavailable)
            }
        }
    }

    /// Submit one chunk. Returns false (and does nothing) when no slot is free.
    pub(crate) fn push(
        &mut self,
        mapping: &Mapping,
        offset: usize,
        length: usize,
    ) -> Result<bool, Error> {
        match self {
            Self::Cpu {
                want_crc,
                done,
                report,
            } => {
                let data = mapping.slice(offset, length).map_err(Error::Mapping)?;
                report.cpu_scans += 1;
                let zero = is_zero(data);
                done.push_back(Verdict {
                    offset,
                    length,
                    zero,
                    crc32c: (*want_crc && !zero).then(|| crc32c(data)),
                });
                Ok(true)
            }
            #[cfg(feature = "dto")]
            Self::Dsa(ring) => ring.push(mapping, offset, length),
        }
    }

    /// Poll the engine and return every verdict that has become available.
    pub(crate) fn drain(
        &mut self,
        mapping: &Mapping,
        into: &mut VecDeque<Verdict>,
    ) -> Result<(), Error> {
        match self {
            Self::Cpu { done, .. } => {
                let _ = mapping;
                into.append(done);
                Ok(())
            }
            #[cfg(feature = "dto")]
            Self::Dsa(ring) => ring.drain(mapping, into),
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        match self {
            Self::Cpu { done, .. } => done.is_empty(),
            #[cfg(feature = "dto")]
            Self::Dsa(ring) => ring.is_idle(),
        }
    }

    pub(crate) fn report(&self) -> Report {
        match self {
            Self::Cpu { report, .. } => *report,
            #[cfg(feature = "dto")]
            Self::Dsa(ring) => ring.report(),
        }
    }
}

#[cfg(all(feature = "dto", feature = "qpl"))]
mod dsa {
    use std::collections::VecDeque;

    use super::{AccelOptions, Error, Report, Verdict};
    use crate::crc::{crc32c, is_zero};
    use crate::dto::{Op, Poll, Stats, Submit, ZeroBuffer};
    use crate::mapping::Mapping;

    enum Pending<T> {
        Device,
        Cpu,
        Done(T),
    }

    struct Slot {
        compare: Op,
        crc: Op,
        offset: usize,
        length: usize,
        live: bool,
        differs: Pending<bool>,
        checksum: Pending<Option<u32>>,
    }

    pub(crate) struct Ring {
        slots: Vec<Slot>,
        zero: ZeroBuffer,
        want_crc: bool,
        stats: Stats,
        live: usize,
        cpu_scans: u64,
    }

    impl Ring {
        pub(super) fn new(options: AccelOptions, chunk_size: usize) -> Result<Self, Error> {
            let zero = ZeroBuffer::new(chunk_size).map_err(Error::ZeroBuffer)?;
            let slots = (0..options.dsa_depth.max(1))
                .map(|_| Slot {
                    compare: Op::default(),
                    crc: Op::default(),
                    offset: 0,
                    length: 0,
                    live: false,
                    differs: Pending::Done(false),
                    checksum: Pending::Done(None),
                })
                .collect();
            Ok(Self {
                slots,
                zero,
                want_crc: options.crc,
                stats: Stats::default(),
                live: 0,
                cpu_scans: 0,
            })
        }

        pub(super) fn push(
            &mut self,
            mapping: &Mapping,
            offset: usize,
            length: usize,
        ) -> Result<bool, Error> {
            let Some(index) = self.slots.iter().position(|slot| !slot.live) else {
                return Ok(false);
            };
            let source = mapping.ptr(offset, length).map_err(Error::Mapping)?;
            let slot = &mut self.slots[index];
            slot.offset = offset;
            slot.length = length;
            slot.live = true;
            // SAFETY: the source mapping outlives the ring and is not written
            // while the VM is paused; the zero buffer is immutable; the slot
            // (and its ops) stay at a fixed address in `self.slots`.
            slot.differs = match unsafe {
                slot.compare
                    .submit_compare(source, self.zero.as_ptr(), length, &self.stats)
            } {
                Submit::Submitted => Pending::Device,
                Submit::Fallback => Pending::Cpu,
            };
            slot.checksum = if self.want_crc {
                // SAFETY: as above.
                match unsafe { slot.crc.submit_crc(source, length, &self.stats) } {
                    Submit::Submitted => Pending::Device,
                    Submit::Fallback => Pending::Cpu,
                }
            } else {
                Pending::Done(None)
            };
            self.live += 1;
            Ok(true)
        }

        pub(super) fn drain(
            &mut self,
            mapping: &Mapping,
            into: &mut VecDeque<Verdict>,
        ) -> Result<(), Error> {
            for slot in &mut self.slots {
                if !slot.live {
                    continue;
                }
                if let Pending::Device = slot.differs {
                    match slot.compare.poll(&self.stats) {
                        Poll::Pending => {}
                        Poll::Done => slot.differs = Pending::Done(slot.compare.differs()),
                        Poll::Failed { .. } => slot.differs = Pending::Cpu,
                    }
                }
                if let Pending::Device = slot.checksum {
                    match slot.crc.poll(&self.stats) {
                        Poll::Pending => {}
                        Poll::Done => slot.checksum = Pending::Done(Some(slot.crc.crc())),
                        Poll::Failed { .. } => slot.checksum = Pending::Cpu,
                    }
                }
                // Software fallbacks run only once the device side has
                // nothing more to say about this chunk.
                let waiting = matches!(slot.differs, Pending::Device)
                    || matches!(slot.checksum, Pending::Device);
                if waiting {
                    continue;
                }
                if let Pending::Cpu = slot.differs {
                    let data = mapping
                        .slice(slot.offset, slot.length)
                        .map_err(Error::Mapping)?;
                    self.cpu_scans += 1;
                    slot.differs = Pending::Done(!is_zero(data));
                }
                let Pending::Done(differs) = slot.differs else {
                    unreachable!()
                };
                if let Pending::Cpu = slot.checksum {
                    let checksum = if differs {
                        let data = mapping
                            .slice(slot.offset, slot.length)
                            .map_err(Error::Mapping)?;
                        Some(crc32c(data))
                    } else {
                        None
                    };
                    slot.checksum = Pending::Done(checksum);
                }
                let Pending::Done(checksum) = slot.checksum else {
                    unreachable!()
                };
                into.push_back(Verdict {
                    offset: slot.offset,
                    length: slot.length,
                    zero: !differs,
                    crc32c: if differs { checksum } else { None },
                });
                slot.live = false;
                self.live -= 1;
            }
            Ok(())
        }

        pub(super) fn is_idle(&self) -> bool {
            self.live == 0
        }

        pub(super) fn report(&self) -> Report {
            let (dsa_submitted, dsa_fallback, dsa_failed) = self.stats.snapshot();
            Report {
                dsa_submitted,
                dsa_fallback,
                dsa_failed,
                cpu_scans: self.cpu_scans,
            }
        }
    }
}
