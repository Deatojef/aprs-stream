use crate::config::FixBits;
use crate::hdlc::framer::RawFrame;

/// Compute CRC-16-CCITT (also called CRC-HDLC / IBM-SDLC polynomial 0x8408 reversed).
///
/// This matches direwolf's `fcs_calc` in `hdlc_rec2.c`: bit-by-bit, initial value 0xFFFF,
/// final XOR 0xFFFF (i.e. complement). The result is compared to the two-byte FCS stored
/// little-endian at the end of the frame.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        let mut b = byte;
        for _ in 0..8 {
            let bit = (crc ^ b as u16) & 1;
            crc >>= 1;
            if bit != 0 {
                crc ^= 0x8408;
            }
            b >>= 1;
        }
    }
    !crc
}

/// Validated frame data (FCS stripped).
#[derive(Debug, Clone)]
pub struct ValidFrame {
    /// Frame bytes with the 2-byte FCS removed.
    pub data: Vec<u8>,
}

/// Attempt to validate a raw HDLC frame and, if configured, recover from bit errors.
///
/// `raw.data` is expected to include the 2-byte FCS at the end.
/// On success returns `Some(ValidFrame)` with FCS stripped.
///
/// Ported from `try_decode` and `try_to_fix_quick_now` in direwolf's `hdlc_rec2.c`.
pub fn try_validate(raw: &RawFrame, fix_bits: FixBits) -> Option<ValidFrame> {
    let data = &raw.data;
    if data.len() < 2 {
        return None;
    }

    // Check clean decode first.
    if check_crc(data) {
        return Some(ValidFrame {
            data: data[..data.len() - 2].to_vec(),
        });
    }

    match fix_bits {
        FixBits::None => None,
        FixBits::Single => fix_single_bit(data),
        FixBits::Double => fix_single_bit(data).or_else(|| fix_double_adjacent(data)),
    }
}

/// Return true if the frame (including FCS) has a correct CRC.
fn check_crc(frame: &[u8]) -> bool {
    let n = frame.len();
    let payload = &frame[..n - 2];
    let stored_fcs = (frame[n - 2] as u16) | ((frame[n - 1] as u16) << 8);
    crc16(payload) == stored_fcs
}

/// Try flipping each bit in `frame` one at a time; return first valid result.
fn fix_single_bit(frame: &[u8]) -> Option<ValidFrame> {
    let n = frame.len();
    let mut buf = frame.to_vec();
    for byte_idx in 0..n {
        for bit_idx in 0..8u8 {
            buf[byte_idx] ^= 1 << bit_idx;
            if check_crc(&buf) {
                return Some(ValidFrame {
                    data: buf[..n - 2].to_vec(),
                });
            }
            buf[byte_idx] ^= 1 << bit_idx; // restore
        }
    }
    None
}

/// Try flipping each pair of adjacent bits; return first valid result.
///
/// "Adjacent" means two bits that are next to each other in the bit stream:
/// either within the same byte or spanning the boundary between consecutive bytes.
fn fix_double_adjacent(frame: &[u8]) -> Option<ValidFrame> {
    let n = frame.len();
    let total_bits = n * 8;
    let mut buf = frame.to_vec();

    for first_bit in 0..total_bits - 1 {
        let second_bit = first_bit + 1;

        let (b0, bit0) = (first_bit / 8, first_bit % 8);
        let (b1, bit1) = (second_bit / 8, second_bit % 8);

        buf[b0] ^= 1 << bit0;
        buf[b1] ^= 1 << bit1;
        if check_crc(&buf) {
            return Some(ValidFrame {
                data: buf[..n - 2].to_vec(),
            });
        }
        buf[b0] ^= 1 << bit0;
        buf[b1] ^= 1 << bit1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hdlc::framer::RawFrame;

    fn make_raw(data: Vec<u8>) -> RawFrame {
        RawFrame { data }
    }

    fn frame_with_fcs(payload: &[u8]) -> Vec<u8> {
        let fcs = crc16(payload);
        let mut v = payload.to_vec();
        v.push((fcs & 0xFF) as u8);
        v.push((fcs >> 8) as u8);
        v
    }

    #[test]
    fn crc16_known_vector() {
        // AX.25 FCS of all-zero 16-byte payload:
        // We just verify self-consistency: compute, then check.
        let payload = vec![0u8; 16];
        let fcs = crc16(&payload);
        let mut frame = payload.clone();
        frame.push((fcs & 0xFF) as u8);
        frame.push((fcs >> 8) as u8);
        assert!(check_crc(&frame));
    }

    #[test]
    fn clean_frame_validates() {
        let payload = b"KA9Q-1>APDR15,WIDE1-1:test";
        let frame = frame_with_fcs(payload);
        let raw = make_raw(frame);
        let result = try_validate(&raw, FixBits::None);
        assert!(result.is_some());
        assert_eq!(result.unwrap().data, payload);
    }

    #[test]
    fn single_bit_error_recovered() {
        let payload = b"KA9Q-1>APDR15:test packet";
        let mut frame = frame_with_fcs(payload);
        // Flip bit 3 of byte 0.
        frame[0] ^= 0x08;
        let raw = make_raw(frame);
        let result = try_validate(&raw, FixBits::Single);
        assert!(result.is_some(), "should recover single-bit error");
        assert_eq!(result.unwrap().data, payload);
    }

    #[test]
    fn double_adjacent_error_recovered() {
        let payload = b"KA9Q-1>APDR15:double bit test";
        let mut frame = frame_with_fcs(payload);
        // Flip bits 2 and 3 of byte 1 (adjacent within byte).
        frame[1] ^= 0x0C;
        let raw = make_raw(frame);
        // Single-bit should fail, double-adjacent should succeed.
        assert!(try_validate(&raw, FixBits::Single).is_none());
        let result = try_validate(&raw, FixBits::Double);
        assert!(result.is_some(), "should recover double-adjacent error");
        assert_eq!(result.unwrap().data, payload);
    }

    #[test]
    fn corrupt_frame_none_mode_returns_none() {
        let payload = b"test";
        let mut frame = frame_with_fcs(payload);
        frame[0] ^= 0xFF; // corrupt heavily
        let raw = make_raw(frame);
        assert!(try_validate(&raw, FixBits::None).is_none());
    }
}
