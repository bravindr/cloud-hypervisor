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
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut crc = 0_u32;
    let (words, tail) = data.split_at(data.len() & !7);
    for word in words.chunks_exact(8) {
        crc = _mm_crc32_u64(u64::from(crc), u64::from_le_bytes(word.try_into().unwrap())) as u32;
    }
    for &byte in tail {
        crc = _mm_crc32_u8(crc, byte);
    }
    crc
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
