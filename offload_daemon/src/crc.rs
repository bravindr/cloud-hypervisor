// Copyright © 2026 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! CRC32C in the convention of the DSA CRC generation operation as DTO
//! surfaces it: seed 0, no final inversion (verified against the device by
//! `dto-async-test` in the DTO tree). Used for the software fallback on
//! snapshot and for verification on restore when DSA is unavailable.

pub(crate) fn crc32c(data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("sse4.2") {
            // SAFETY: SSE4.2 presence was just checked.
            return unsafe { crc32c_sse42(data) };
        }
    }
    crc32c_table(data)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(data: &[u8]) -> u32 {
    // One crc32 chain is latency-bound (3 cycles per 8 bytes); three
    // independent lanes keep the unit busy every cycle. With seed 0 and no
    // final xor the CRC is linear, so the lanes combine as
    // crc(A || B) = crc(A) * x^(8|B|) mod P  xor  crc(B).
    const LANES_FROM: usize = 3 * 1024;
    if data.len() < LANES_FROM {
        return unsafe { crc32c_sse42_seeded(0, data) };
    }
    let lane = (data.len() / 3) & !7;
    let (a, rest) = data.split_at(lane);
    let (b, rest) = rest.split_at(lane);
    let (c, tail) = rest.split_at(lane);
    let (mut ca, mut cb, mut cc) = (0_u64, 0_u64, 0_u64);
    let (pa, pb, pc) = (a.as_ptr(), b.as_ptr(), c.as_ptr());
    let mut offset = 0;
    while offset < lane {
        use std::arch::x86_64::_mm_crc32_u64;
        // SAFETY: offset + 8 <= lane, the length of each lane.
        unsafe {
            ca = _mm_crc32_u64(ca, pa.add(offset).cast::<u64>().read_unaligned());
            cb = _mm_crc32_u64(cb, pb.add(offset).cast::<u64>().read_unaligned());
            cc = _mm_crc32_u64(cc, pc.add(offset).cast::<u64>().read_unaligned());
        }
        offset += 8;
    }
    let shift = xpow8n(lane);
    let mut crc = multmodp(shift, ca as u32) ^ cb as u32;
    crc = multmodp(shift, crc) ^ cc as u32;
    unsafe { crc32c_sse42_seeded(crc, tail) }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42_seeded(mut crc: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let (words, tail) = data.split_at(data.len() & !7);
    for word in words.chunks_exact(8) {
        crc = _mm_crc32_u64(u64::from(crc), u64::from_le_bytes(word.try_into().unwrap())) as u32;
    }
    for &byte in tail {
        crc = _mm_crc32_u8(crc, byte);
    }
    crc
}

const POLY: u32 = 0x82f6_3b78;

/// a * b mod P, reflected (bit 31 is x^0), as in zlib's multmodp.
fn multmodp(a: u32, mut b: u32) -> u32 {
    let mut m = 1_u32 << 31;
    let mut p = 0_u32;
    loop {
        if a & m != 0 {
            p ^= b;
            if a & (m - 1) == 0 {
                break;
            }
        }
        m >>= 1;
        if m == 0 {
            break;
        }
        b = if b & 1 != 0 { (b >> 1) ^ POLY } else { b >> 1 };
    }
    p
}

/// x^(8 * bytes) mod P, reflected.
fn xpow8n(bytes: usize) -> u32 {
    let mut result = 1_u32 << 31; // x^0
    let mut square = 1_u32 << 23; // x^8
    let mut n = bytes;
    while n != 0 {
        if n & 1 != 0 {
            result = multmodp(square, result);
        }
        square = multmodp(square, square);
        n >>= 1;
    }
    result
}

fn crc32c_table(data: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    crc
}

/// True when every byte is zero. Scans a machine word at a time; the byte
/// iterator the original code used compiled to a one-byte-per-cycle loop
/// under the workspace's `opt-level = "s"`.
pub(crate) fn is_zero(data: &[u8]) -> bool {
    // SAFETY: u128 has no invalid bit patterns; align_to only splits the slice.
    let (head, body, tail) = unsafe { data.align_to::<u128>() };
    head.iter().all(|&byte| byte == 0)
        && body.iter().all(|&word| word == 0)
        && tail.iter().all(|&byte| byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_and_table_agree() {
        let data: Vec<u8> = (0..100_003u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        assert_eq!(crc32c(&data), crc32c_table(&data));
    }

    #[test]
    fn lanes_match_single_chain() {
        let data: Vec<u8> = (0..(3 << 20) + 77u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        for len in [0, 1, 3071, 3072, 3079, 4096, 65_536 + 5, 1 << 20, data.len()] {
            let single = unsafe { crc32c_sse42_seeded(0, &data[..len]) };
            assert_eq!(crc32c(&data[..len]), single, "len {len}");
        }
        assert_eq!(crc32c(&data[..5000]), crc32c_table(&data[..5000]));
    }

    #[test]
    fn known_vector() {
        // CRC32C("123456789") with seed 0 and no final xor (the DSA/DTO
        // convention); the usual CRC-32C check value 0xe3069283 uses seed ~0
        // and a final inversion.
        assert_eq!(crc32c_table(b"123456789"), 0x58e3fa20);
        assert_eq!(crc32c(b"123456789"), 0x58e3fa20);
    }

    #[test]
    fn zero_scan() {
        let mut buffer = vec![0_u8; 70_000];
        assert!(is_zero(&buffer));
        assert!(is_zero(&buffer[3..]));
        buffer[69_999] = 1;
        assert!(!is_zero(&buffer));
        assert!(is_zero(&buffer[..69_999]));
    }
}

#[cfg(test)]
mod throughput {
    #[test]
    #[ignore = "timing only: cargo test -- --ignored --nocapture crc_rate"]
    fn crc_rate() {
        let data: Vec<u8> = (0..1u32 << 20).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        for (name, f) in [
            ("one chain", (|d: &[u8]| unsafe { super::crc32c_sse42_seeded(0, d) }) as fn(&[u8]) -> u32),
            ("three lanes", super::crc32c as fn(&[u8]) -> u32),
        ] {
            let started = std::time::Instant::now();
            let mut acc = 0;
            for _ in 0..2048 {
                acc ^= f(std::hint::black_box(&data));
            }
            let secs = started.elapsed().as_secs_f64();
            println!("{name}: {:.1} GB/s ({acc:x})", 2048.0 * data.len() as f64 / secs / 1e9);
        }
    }
}
