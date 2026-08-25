const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

/// Format a u64 as lowercase hexadecimal ASCII bytes into a fixed 16-byte buffer with zero heap allocations.
/// Returns the slice of valid ASCII bytes.
#[inline(always)]
pub fn fast_hex_u64(mut val: u64, buf: &mut [u8; 16]) -> &[u8] {
    if val == 0 {
        buf[15] = b'0';
        return &buf[15..16];
    }

    let mut idx = 16;
    while val > 0 && idx > 0 {
        idx -= 1;
        buf[idx] = HEX_CHARS[(val & 0x0F) as usize];
        val >>= 4;
    }

    &buf[idx..16]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fast_hex_u64() {
        let mut buf = [0u8; 16];

        let val = 0x8828308281fffffu64;
        let hex_slice = fast_hex_u64(val, &mut buf);
        let s = std::str::from_utf8(hex_slice).unwrap();
        assert_eq!(s, format!("{:x}", val));

        let zero_slice = fast_hex_u64(0, &mut buf);
        let s_zero = std::str::from_utf8(zero_slice).unwrap();
        assert_eq!(s_zero, "0");

        let max_slice = fast_hex_u64(u64::MAX, &mut buf);
        let s_max = std::str::from_utf8(max_slice).unwrap();
        assert_eq!(s_max, "ffffffffffffffff");
    }
}
