use crate::afsk::dpll::DemodBit;

// Minimum valid AX.25 frame: 14 bytes address + 1 control + 1 PID + 2 FCS = 18 bytes.
// direwolf uses AX25_MIN_PACKET_LEN (without FCS) + 2 for the FCS itself.
const MIN_FRAME_LEN: usize = 18;

// Maximum frame length: 7*10 address bytes + 256 info + 2 FCS = 330; cap generously.
const MAX_FRAME_LEN: usize = 512;

/// Output of a completed HDLC frame decode attempt.
#[derive(Debug, Clone)]
pub struct RawFrame {
    /// Frame bytes including the 2-byte FCS (not stripped here).
    pub data: Vec<u8>,
}

/// Per-slicer HDLC framing state machine.
///
/// Ported from `hdlc_rec_bit_new` in direwolf's `hdlc_rec.c`.
///
/// Each call to `push_bit` advances the state by one demodulated bit.
/// Completed frames (flag-delimited, minimum-length) are returned as
/// `RawFrame` values; CRC validation is performed by the FEC layer.
#[derive(Debug, Clone)]
pub struct HdlcDecoder {
    /// Previous raw (pre-NRZI) bit, for NRZI decode.
    prev_raw: bool,
    /// 8-bit pattern detector shift register (MSB = oldest received bit).
    pat_det: u8,
    /// Current output accumulator: bits are loaded LSB-first until 8 are ready.
    oacc: u8,
    /// Number of bits accumulated in `oacc` (0–7).
    olen: u8,
    /// Frame buffer accumulating bytes between flags.
    frame_buf: Vec<u8>,
    /// True when we are between two flags and accumulating frame data.
    in_frame: bool,
}

impl HdlcDecoder {
    pub fn new(_slice: usize) -> Self {
        Self {
            prev_raw: false,
            pat_det: 0,
            oacc: 0,
            olen: 0,
            frame_buf: Vec::new(),
            in_frame: false,
        }
    }

    /// Feed one demodulated bit; returns a completed `RawFrame` if one was just closed.
    ///
    /// Bit is the logical NRZI-decoded bit: the caller supplies the raw `DemodBit.bit`
    /// (mark/space) and NRZI decoding is performed here.
    #[inline]
    pub fn push_bit(&mut self, dbit_raw: bool) -> Option<RawFrame> {
        // NRZI: no transition = 1, transition = 0.
        let dbit = dbit_raw == self.prev_raw;
        self.prev_raw = dbit_raw;

        // Shift pattern detector: move existing bits right, insert new bit at MSB.
        self.pat_det >>= 1;
        if dbit {
            self.pat_det |= 0x80;
        }

        // Seven consecutive 1s → abort frame.
        if self.pat_det == 0xFE {
            self.oacc = 0;
            self.olen = 0;
            self.frame_buf.clear();
            self.in_frame = false;
            return None;
        }

        // 0x7E = 01111110 → flag byte.
        if self.pat_det == 0x7E {
            let result = if self.in_frame && self.frame_buf.len() >= MIN_FRAME_LEN {
                Some(RawFrame {
                    data: self.frame_buf.clone(),
                })
            } else {
                None
            };
            // Reset for next frame; flag also starts accumulation.
            self.oacc = 0;
            self.olen = 0;
            self.frame_buf.clear();
            self.in_frame = true;
            return result;
        }

        // Five consecutive 1s followed by a 0: bit stuffing — discard this bit.
        // pat_det & 0xFC == 0x7C means: bits [7:2] are 0,1,1,1,1,1 and bit[1] = 0.
        // In our LSB-oldest register that means top bits 7..2 are 01111110>>1 = 0b0111110x
        // Let's verify: after NRZI decode the last 8 bits in pat_det (MSB=oldest):
        //   five 1s followed by 0: the 0 is newest (at bit 0 after >>= 1, so not set),
        //   the five 1s occupy bits 1..5; bits 6..7 can be anything.
        // direwolf checks: (pat_det & 0xFC) == 0x7C
        //   0x7C = 0111_1100: bits 7..2 must be 0,1,1,1,1,1 — i.e. the oldest bit in
        //   our window is 0, then five 1s, then the newest (stuffed) 0 at bits 1..0.
        //   Wait — let me re-examine: MSB is oldest. After >>= 1 with new bit not set:
        //     bit 0 = 0 (current, unstuffed zero)
        //     bits 1..5 would be the five 1s
        //     bit 6 could be anything (bit before the run)
        //     bit 7 is oldest
        //   (pat_det & 0xFC) == 0x7C: mask out bits 1..0, compare to 0111_1100.
        //   That requires bits 7..2 = 0,1,1,1,1,1 exactly.  ✓ matches.
        if (self.pat_det & 0xFC) == 0x7C {
            // Stuffed bit — skip, don't accumulate.
            return None;
        }

        // Normal data bit: accumulate LSB-first into oacc.
        if !self.in_frame {
            return None;
        }

        self.oacc >>= 1;
        if dbit {
            self.oacc |= 0x80;
        }
        self.olen += 1;

        if self.olen == 8 {
            if self.frame_buf.len() < MAX_FRAME_LEN {
                self.frame_buf.push(self.oacc);
            } else {
                // Overlong frame — abort.
                self.frame_buf.clear();
                self.in_frame = false;
            }
            self.oacc = 0;
            self.olen = 0;
        }

        None
    }

    /// Feed a `DemodBit` (convenience wrapper around `push_bit`).
    #[inline]
    pub fn push(&mut self, b: &DemodBit) -> Option<RawFrame> {
        self.push_bit(b.bit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits_from_bytes(bytes: &[u8]) -> Vec<bool> {
        // Returns bits LSB-first per byte (same order HDLC transmits them).
        bytes
            .iter()
            .flat_map(|&b| (0..8).map(move |i| (b >> i) & 1 == 1))
            .collect()
    }

    fn nrzi_encode(bits: &[bool]) -> Vec<bool> {
        // NRZI: 0 → transition, 1 → no transition; start from false.
        let mut raw = Vec::with_capacity(bits.len());
        let mut prev = false;
        for &b in bits {
            if !b {
                prev = !prev; // transition
            }
            raw.push(prev);
        }
        raw
    }

    fn stuff_bits(bits: &[bool]) -> Vec<bool> {
        let mut out = Vec::new();
        let mut ones = 0u8;
        for &b in bits {
            out.push(b);
            if b {
                ones += 1;
                if ones == 5 {
                    out.push(false); // stuffed zero
                    ones = 0;
                }
            } else {
                ones = 0;
            }
        }
        out
    }

    fn build_hdlc_frame(payload: &[u8]) -> Vec<bool> {
        // Build: flag | bitstuff(payload) | flag
        let flag_bits: Vec<bool> = bits_from_bytes(&[0x7E]);
        let payload_bits = bits_from_bytes(payload);
        let stuffed = stuff_bits(&payload_bits);
        let mut frame = flag_bits.clone();
        frame.extend_from_slice(&stuffed);
        frame.extend_from_slice(&flag_bits);
        nrzi_encode(&frame)
    }

    /// Compute CRC-16-CCITT (HDLC FCS) — matches direwolf's implementation.
    fn crc16(data: &[u8]) -> u16 {
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

    fn valid_ax25_frame(content: &[u8]) -> Vec<u8> {
        // Append 2-byte FCS (little-endian) to make a valid frame.
        let fcs = crc16(content);
        let mut frame = content.to_vec();
        frame.push((fcs & 0xFF) as u8);
        frame.push((fcs >> 8) as u8);
        frame
    }

    #[test]
    fn decodes_minimal_valid_frame() {
        // Construct a minimal valid HDLC frame: 18 bytes payload (with FCS).
        // Use 18 bytes of zeroes + correct FCS.
        let content = vec![0u8; 16]; // 16 bytes data → 18 with FCS
        let frame_bytes = valid_ax25_frame(&content);
        assert_eq!(frame_bytes.len(), 18);

        let raw_nrzi = build_hdlc_frame(&frame_bytes);
        let mut dec = HdlcDecoder::new(0);
        let mut decoded = None;
        for raw_bit in raw_nrzi {
            if let Some(f) = dec.push_bit(raw_bit) {
                decoded = Some(f);
            }
        }
        let f = decoded.expect("should have decoded a frame");
        assert_eq!(f.data, frame_bytes);
    }

    #[test]
    fn rejects_short_frame() {
        // 17-byte frames are below MIN_FRAME_LEN; should not be emitted.
        let content = vec![0u8; 15];
        let frame_bytes = valid_ax25_frame(&content); // 17 bytes
        assert_eq!(frame_bytes.len(), 17);

        let raw_nrzi = build_hdlc_frame(&frame_bytes);
        let mut dec = HdlcDecoder::new(0);
        let mut got_frame = false;
        for raw_bit in raw_nrzi {
            if dec.push_bit(raw_bit).is_some() {
                got_frame = true;
            }
        }
        assert!(!got_frame, "short frame should be suppressed");
    }

    #[test]
    fn abort_clears_state() {
        // Insert 7 consecutive 1s (abort sequence) mid-frame; should clear state.
        // Build a flag then 7 raw 1-bits then a flag; no frame should be emitted.
        let flag_bits: Vec<bool> = bits_from_bytes(&[0x7E]);
        let abort_bits: Vec<bool> = vec![true; 7];

        let mut frame_bits = flag_bits.clone();
        frame_bits.extend_from_slice(&abort_bits);
        frame_bits.extend_from_slice(&flag_bits);

        let nrzi = nrzi_encode(&frame_bits);
        let mut dec = HdlcDecoder::new(0);
        let mut got_frame = false;
        for b in nrzi {
            if dec.push_bit(b).is_some() {
                got_frame = true;
            }
        }
        assert!(!got_frame, "abort sequence should prevent frame emission");
    }
}
