/// A parsed AX.25 UI frame.
///
/// Addresses are decoded from the 7-byte-per-callsign wire format:
///   bytes 0..5: ASCII characters each shifted left 1 bit (>> 1 to recover)
///   byte 6: SSID in bits [4:1]; H-bit in bit 7; end-of-address marker in bit 0
///
/// Only UI frames are accepted (control = 0x03, PID = 0xF0).
#[derive(Debug, Clone)]
pub struct Ax25Frame {
    pub source: String,
    pub destination: String,
    /// Digipeater path entries; H-bit state is encoded in `via_heard`.
    pub via: Vec<String>,
    /// For each via entry: true if the H-bit (bit 7 of the 7th address byte) is set.
    /// H-bit = 1 means this digipeater has already repeated the packet.
    pub via_heard: Vec<bool>,
    /// Information field (everything after control + PID bytes).
    pub info: Vec<u8>,
}

impl Ax25Frame {
    /// Decode a raw AX.25 frame (FCS already stripped).
    ///
    /// Returns `None` if the frame is malformed, too short, or not a UI frame.
    pub fn parse(data: &[u8]) -> Option<Self> {
        // Minimum: destination(7) + source(7) + control(1) + pid(1) = 16 bytes.
        if data.len() < 16 {
            return None;
        }

        let mut pos = 0;

        // Destination address.
        if pos + 7 > data.len() {
            return None;
        }
        // A bad character in any address field means the frame is mis-decoded
        // (or an FCS false-positive from the bit-fixer / a divergent slicer):
        // reject the whole frame rather than forward garbage callsigns.
        let (dest, dest_end) = decode_address(&data[pos..pos + 7])?;
        pos += 7;
        // End-of-address bit should NOT be set on destination in normal frames,
        // but we don't enforce it — source or via entries end the address field.
        let _ = dest_end;

        // Source address.
        if pos + 7 > data.len() {
            return None;
        }
        let (src, src_end) = decode_address(&data[pos..pos + 7])?;
        pos += 7;

        // Optional digipeater addresses: continue while end-of-address bit not set.
        let mut via: Vec<String> = Vec::new();
        let mut via_heard: Vec<bool> = Vec::new();
        if !src_end {
            loop {
                if pos + 7 > data.len() {
                    return None; // truncated
                }
                let (call, end, h_bit) = decode_via(&data[pos..pos + 7])?;
                pos += 7;
                via.push(call);
                via_heard.push(h_bit);
                if end {
                    break;
                }
            }
        }

        // Control byte: must be 0x03 for UI frame.
        if pos >= data.len() {
            return None;
        }
        let control = data[pos];
        pos += 1;
        if control != 0x03 {
            return None;
        }

        // PID byte: must be 0xF0 for No Layer 3.
        if pos >= data.len() {
            return None;
        }
        let pid = data[pos];
        pos += 1;
        if pid != 0xF0 {
            return None;
        }

        // Information field: remainder of frame.
        let info = data[pos..].to_vec();

        Some(Ax25Frame {
            source: src,
            destination: dest,
            via,
            via_heard,
            info,
        })
    }

    /// True when the packet was received directly (no H-bits set in the via path).
    ///
    /// If every digipeater entry in the via path has its H-bit clear, the packet
    /// has not been repeated — we heard it directly from the originating station.
    pub fn heard_direct(&self) -> bool {
        self.via_heard.iter().all(|&h| !h)
    }

    /// The callsign most likely responsible for the RF signal we received.
    ///
    /// Walks the via path backward and returns the last entry with H-bit set
    /// whose callsign is a real station rather than a routing alias (WIDE,
    /// TRACE, etc.) or iGate annotation (TCPIP, NOGATE, etc.). If no such
    /// entry exists, falls back to the source callsign.
    ///
    /// The alias filtering matters because the New-N paradigm puts entries
    /// like `WIDE2*` in the path that any digipeater can claim — they're
    /// slots, not stations. The actual transmitter is the previous real
    /// callsign in the path.
    pub fn heard_from(&self) -> &str {
        for (i, &h) in self.via_heard.iter().enumerate().rev() {
            if h && !is_aprs_alias(&self.via[i]) {
                return &self.via[i];
            }
        }
        &self.source
    }
}

/// True if `call` is an APRS routing alias or iGate annotation — i.e. a path
/// slot rather than a real station callsign.
///
/// Compared against the base callsign (any `-SSID` suffix is stripped first).
/// Covers the New-N WIDEn / TRACEn families (the literal word optionally
/// followed by a single digit — no real callsign has that shape), the legacy
/// RELAY/ECHO/GATE aliases, and the iGate annotations
/// TCPIP/TCPXX/NOGATE/RFONLY/IGATECALL.
fn is_aprs_alias(call: &str) -> bool {
    let base = match call.find('-') {
        Some(pos) => &call[..pos],
        None => call,
    };
    is_wide_trace_alias(base)
        || matches!(
            base,
            "RELAY" | "ECHO" | "GATE" | "NOGATE" | "RFONLY" | "TCPIP" | "TCPXX" | "IGATECALL"
        )
}

/// True if `base` is a WIDEn / TRACEn routing alias: the literal `WIDE` or
/// `TRACE` on its own, or followed by a single digit (`WIDE1`, `TRACE7`, …).
/// No valid amateur callsign has this shape, so the digit check is safe.
fn is_wide_trace_alias(base: &str) -> bool {
    let suffix = base
        .strip_prefix("WIDE")
        .or_else(|| base.strip_prefix("TRACE"));
    match suffix {
        Some("") => true,
        Some(s) => s.len() == 1 && s.as_bytes()[0].is_ascii_digit(),
        None => false,
    }
}

/// Decode a 7-byte AX.25 address field into a callsign string.
///
/// Returns `Some((callsign, end_of_address_bit))`, or `None` if the field is
/// not a valid callsign. Valid AX.25 address characters are uppercase `A`–`Z`
/// and digits `0`–`9`, left-justified and space-padded to six bytes. Anything
/// else — control characters, lowercase, punctuation, an embedded space
/// followed by more data, or an empty callsign — indicates a mis-decoded
/// frame, so we reject it rather than emit odd characters downstream.
fn decode_address(bytes: &[u8]) -> Option<(String, bool)> {
    debug_assert_eq!(bytes.len(), 7);
    let mut call = String::with_capacity(9);
    let mut seen_pad = false;
    for &b in &bytes[0..6] {
        let ch = (b >> 1) as char;
        if ch == ' ' {
            // Space marks the end of the callsign; the remaining bytes must all
            // be space padding. A non-space after a space is a corrupt field.
            seen_pad = true;
            continue;
        }
        if seen_pad {
            return None; // data after padding → invalid
        }
        if !ch.is_ascii_uppercase() && !ch.is_ascii_digit() {
            return None; // non-alphanumeric callsign character → invalid
        }
        call.push(ch);
    }
    if call.is_empty() {
        return None; // empty callsign → invalid
    }
    let ssid_byte = bytes[6];
    let ssid = (ssid_byte >> 1) & 0x0F;
    if ssid != 0 {
        call.push('-');
        call.push_str(&ssid.to_string());
    }
    let end_bit = (ssid_byte & 0x01) != 0;
    Some((call, end_bit))
}

/// Decode a 7-byte AX.25 via (digipeater) address field.
///
/// Returns `Some((callsign, end_of_address_bit, h_bit))`, or `None` if the
/// callsign is invalid (see [`decode_address`]).
fn decode_via(bytes: &[u8]) -> Option<(String, bool, bool)> {
    debug_assert_eq!(bytes.len(), 7);
    let (call, end) = decode_address(bytes)?;
    let h_bit = (bytes[6] & 0x80) != 0;
    Some((call, end, h_bit))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a callsign into 7-byte AX.25 address format.
    fn encode_address(call: &str, ssid: u8, end_bit: bool, h_bit: bool) -> [u8; 7] {
        let mut out = [b' ' << 1; 7]; // pad with space<<1
        let base: &str = if let Some(pos) = call.find('-') {
            &call[..pos]
        } else {
            call
        };
        for (i, ch) in base.chars().enumerate().take(6) {
            out[i] = (ch as u8) << 1;
        }
        let ssid_byte = (ssid << 1)
            | if end_bit { 0x01 } else { 0x00 }
            | if h_bit { 0x80 } else { 0x00 }
            | 0x60; // reserved bits set per AX.25 spec
        out[6] = ssid_byte;
        out
    }

    fn build_ui_frame(
        dest: &str,
        src: &str,
        via: &[(&str, bool)], // (callsign, h_bit)
        info: &[u8],
    ) -> Vec<u8> {
        let mut frame = Vec::new();
        // Destination never carries the end-of-address bit: the source (and any
        // via entries) always follow it, so its end_bit is always false.
        frame.extend_from_slice(&encode_address(dest, 0, false, false));

        let src_end = via.is_empty();
        // Extract SSID from src if present.
        let (src_base, src_ssid) = parse_ssid(src);
        let mut src_bytes = encode_address(src_base, src_ssid, src_end, false);
        // Remove reserved bits override — just use what encode_address gave us, but fix end.
        if src_end {
            src_bytes[6] |= 0x01;
        } else {
            src_bytes[6] &= !0x01;
        }
        frame.extend_from_slice(&src_bytes);

        for (i, &(call, h)) in via.iter().enumerate() {
            let end = i == via.len() - 1;
            let (base, ssid) = parse_ssid(call);
            let mut via_bytes = encode_address(base, ssid, end, h);
            if end {
                via_bytes[6] |= 0x01;
            } else {
                via_bytes[6] &= !0x01;
            }
            frame.extend_from_slice(&via_bytes);
        }

        frame.push(0x03); // control: UI
        frame.push(0xF0); // PID: No Layer 3
        frame.extend_from_slice(info);
        frame
    }

    fn parse_ssid(call: &str) -> (&str, u8) {
        if let Some(pos) = call.find('-') {
            let base = &call[..pos];
            let ssid: u8 = call[pos + 1..].parse().unwrap_or(0);
            (base, ssid)
        } else {
            (call, 0)
        }
    }

    #[test]
    fn parse_basic_ui_frame() {
        let info = b"/position data";
        let frame = build_ui_frame("APDR15", "KA9Q-1", &[], info);
        let parsed = Ax25Frame::parse(&frame).expect("should parse");
        assert_eq!(parsed.destination, "APDR15");
        assert_eq!(parsed.source, "KA9Q-1");
        assert!(parsed.via.is_empty());
        assert_eq!(parsed.info, info);
    }

    #[test]
    fn parse_via_with_h_bit() {
        let info = b"test info";
        // Via: WIDE1-1 not heard (H=0), KD9PDP-3 heard (H=1).
        let frame = build_ui_frame(
            "APDR15",
            "KA9Q-1",
            &[("WIDE1-1", false), ("KD9PDP-3", true)],
            info,
        );
        let parsed = Ax25Frame::parse(&frame).expect("should parse");
        assert_eq!(parsed.via.len(), 2);
        assert_eq!(parsed.via_heard, vec![false, true]);
        assert_eq!(parsed.heard_from(), "KD9PDP-3");
        assert!(!parsed.heard_direct());
    }

    #[test]
    fn heard_direct_no_via() {
        let frame = build_ui_frame("APDR15", "KA9Q-1", &[], b"direct");
        let parsed = Ax25Frame::parse(&frame).unwrap();
        assert!(parsed.heard_direct());
        assert_eq!(parsed.heard_from(), "KA9Q-1");
    }

    #[test]
    fn heard_direct_with_unheared_via() {
        // Via exists but H-bit not set → still heard directly.
        let frame = build_ui_frame("APDR15", "KA9Q-1", &[("WIDE1-1", false)], b"direct");
        let parsed = Ax25Frame::parse(&frame).unwrap();
        assert!(parsed.heard_direct());
        assert_eq!(parsed.heard_from(), "KA9Q-1");
    }

    #[test]
    fn heard_from_returns_last_h_bit_set() {
        // Two repeated digipeaters — heard_from must be the last one, not the first.
        let frame = build_ui_frame(
            "APDR15",
            "KA9Q-1",
            &[("W0NED", true), ("KD9PDP-3", true)],
            b"info",
        );
        let parsed = Ax25Frame::parse(&frame).unwrap();
        assert!(!parsed.heard_direct());
        assert_eq!(parsed.heard_from(), "KD9PDP-3");
    }

    #[test]
    fn heard_from_first_repeated_not_last() {
        // Only the first via has H=1 (unusual but legal: e.g., a fill-in digi
        // marked itself before forwarding to a wide that hasn't yet acted).
        let frame = build_ui_frame(
            "APDR15",
            "KA9Q-1",
            &[("W0NED", true), ("WIDE2-1", false)],
            b"info",
        );
        let parsed = Ax25Frame::parse(&frame).unwrap();
        assert!(!parsed.heard_direct());
        assert_eq!(parsed.heard_from(), "W0NED");
    }

    #[test]
    fn heard_from_skips_trailing_wide_alias() {
        // The real-world case from the user's example:
        //   N7UW-1>APMI06,NCFPD*,SIMLA*,WIDE2*:...
        // The trailing WIDE2* is a routing slot, not a station — the last
        // real digipeater is SIMLA.
        let frame = build_ui_frame(
            "APMI06",
            "N7UW-1",
            &[("NCFPD", true), ("SIMLA", true), ("WIDE2", true)],
            b"info",
        );
        let parsed = Ax25Frame::parse(&frame).unwrap();
        assert_eq!(parsed.heard_from(), "SIMLA");
    }

    #[test]
    fn heard_from_skips_wide_with_ssid() {
        // WIDE2-1 (with SSID) is still an alias and must be skipped.
        let frame = build_ui_frame(
            "APMI06",
            "N7UW-1",
            &[("KD9PDP-3", true), ("WIDE2-1", true)],
            b"info",
        );
        let parsed = Ax25Frame::parse(&frame).unwrap();
        assert_eq!(parsed.heard_from(), "KD9PDP-3");
    }

    #[test]
    fn heard_from_skips_tcpip_annotation() {
        // iGate annotations (TCPIP*, NOGATE, RFONLY) also count as aliases.
        let frame = build_ui_frame(
            "APMI06",
            "N7UW-1",
            &[("W0SCA-10", true), ("TCPIP", true)],
            b"info",
        );
        let parsed = Ax25Frame::parse(&frame).unwrap();
        assert_eq!(parsed.heard_from(), "W0SCA-10");
    }

    #[test]
    fn heard_from_falls_back_to_source_when_only_aliases_repeated() {
        // If every H-bit-set entry is an alias, fall back to source — we
        // can't determine which station actually transmitted.
        let frame = build_ui_frame(
            "APMI06",
            "N7UW-1",
            &[("WIDE1-1", true), ("WIDE2-1", true)],
            b"info",
        );
        let parsed = Ax25Frame::parse(&frame).unwrap();
        assert!(!parsed.heard_direct()); // direct stays false — H-bits ARE set
        assert_eq!(parsed.heard_from(), "N7UW-1");
    }

    #[test]
    fn rejects_non_ui_frame() {
        let mut frame = build_ui_frame("APDR15", "KA9Q-1", &[], b"test");
        // Change control byte from 0x03 to 0x05 (not UI).
        let ctrl_pos = 14; // 7 (dest) + 7 (src) = 14
        frame[ctrl_pos] = 0x05;
        assert!(Ax25Frame::parse(&frame).is_none());
    }

    #[test]
    fn rejects_wrong_pid() {
        let mut frame = build_ui_frame("APDR15", "KA9Q-1", &[], b"test");
        let pid_pos = 15;
        frame[pid_pos] = 0xCF; // not 0xF0
        assert!(Ax25Frame::parse(&frame).is_none());
    }

    #[test]
    fn rejects_too_short() {
        assert!(Ax25Frame::parse(&[]).is_none());
        assert!(Ax25Frame::parse(&[0u8; 10]).is_none());
    }

    /// Overwrite one byte of an address field (pre-SSID) with a raw character,
    /// AX.25-shifted, so we can inject mis-decoded callsign bytes.
    fn corrupt_addr_char(frame: &mut [u8], addr_index: usize, byte_in_addr: usize, ch: u8) {
        frame[addr_index * 7 + byte_in_addr] = ch << 1;
    }

    #[test]
    fn rejects_lowercase_in_callsign() {
        // Destination is address 0; replace its 2nd char with lowercase 'a'.
        let mut frame = build_ui_frame("APDR15", "KA9Q-1", &[], b"test");
        corrupt_addr_char(&mut frame, 0, 1, b'a');
        assert!(Ax25Frame::parse(&frame).is_none());
    }

    #[test]
    fn rejects_control_char_in_callsign() {
        // A 0x14 address byte decodes (>>1) to 0x0A '\n' — the worst case, since
        // a newline in a callsign would inject a line into an APRS-IS stream.
        let mut frame = build_ui_frame("APDR15", "KA9Q-1", &[], b"test");
        frame[7 + 1] = 0x14; // source address (index 1), 2nd byte, raw 0x14
        assert!(Ax25Frame::parse(&frame).is_none());
    }

    #[test]
    fn rejects_punctuation_in_callsign() {
        // ':' would break the TNC2 SRC>DST:info delimiter.
        let mut frame = build_ui_frame("APDR15", "KA9Q-1", &[], b"test");
        corrupt_addr_char(&mut frame, 1, 2, b':');
        assert!(Ax25Frame::parse(&frame).is_none());
    }

    #[test]
    fn rejects_data_after_embedded_space() {
        // "AB C  " — a space followed by more data is a corrupt field, not a
        // callsign that should be silently squeezed to "ABC".
        let mut frame = build_ui_frame("ABCDEF", "KA9Q-1", &[], b"test");
        corrupt_addr_char(&mut frame, 0, 2, b' '); // dest becomes "AB DEF"
        assert!(Ax25Frame::parse(&frame).is_none());
    }

    #[test]
    fn rejects_empty_callsign() {
        // All-space source address with SSID 0 → empty callsign.
        let mut frame = build_ui_frame("APDR15", "KA9Q-1", &[], b"test");
        for i in 0..6 {
            frame[7 + i] = b' ' << 1; // blank out source base callsign
        }
        frame[7 + 6] &= !0x1E; // clear SSID bits (keep end-of-address bit)
        assert!(Ax25Frame::parse(&frame).is_none());
    }

    #[test]
    fn rejects_bad_char_in_via() {
        // Garbage in a digipeater callsign must drop the whole frame too.
        let mut frame = build_ui_frame("APDR15", "KA9Q-1", &[("WIDE1-1", false)], b"test");
        // Via is address index 2 (dest=0, src=1, via=2).
        corrupt_addr_char(&mut frame, 2, 0, 0x01); // decodes to a control char
        assert!(Ax25Frame::parse(&frame).is_none());
    }

    #[test]
    fn accepts_valid_alphanumeric_callsigns() {
        // Sanity: a clean frame with digits and letters still parses.
        let frame = build_ui_frame("APN382", "N0CALL-9", &[("W0ABC-2", true)], b"!data");
        let parsed = Ax25Frame::parse(&frame).expect("valid frame should parse");
        assert_eq!(parsed.destination, "APN382");
        assert_eq!(parsed.source, "N0CALL-9");
        assert_eq!(parsed.via, vec!["W0ABC-2"]);
    }

    #[test]
    fn wide_trace_alias_matching() {
        // The WIDEn/TRACEn families are aliases whether bare, digit-suffixed, or
        // SSID-suffixed — no real callsign has the `WORD` or `WORD<digit>` shape.
        for alias in [
            "WIDE", "WIDE1", "WIDE2-1", "WIDE7", "TRACE", "TRACE3", "RELAY", "TCPIP",
        ] {
            assert!(is_aprs_alias(alias), "{alias} should be an alias");
        }
        // Real stations (including ones that merely start with the alias letters)
        // must not be misclassified.
        for call in ["W0NED", "KD9PDP-3", "WIDEN", "WIDE12", "TRACEY", "N7UW-1"] {
            assert!(!is_aprs_alias(call), "{call} should NOT be an alias");
        }
    }
}
