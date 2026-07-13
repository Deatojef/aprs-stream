use crate::ax25::frame::Ax25Frame;

/// Format an `Ax25Frame` as a TNC2 text line.
///
/// Format: `SOURCE>DESTINATION[,VIA...]:info`
///
/// Digipeater entries that have been repeated (H-bit set) are suffixed with `*`.
/// This matches the standard TNC2 monitor format used by APRS software.
pub fn to_tnc2(frame: &Ax25Frame) -> String {
    let mut s = String::with_capacity(128);
    s.push_str(&frame.source);
    s.push('>');
    s.push_str(&frame.destination);

    for (call, &heard) in frame.via.iter().zip(frame.via_heard.iter()) {
        s.push(',');
        s.push_str(call);
        if heard {
            s.push('*');
        }
    }

    s.push(':');

    // Append the info field. Strip non-printable control bytes (except tab),
    // but keep high bytes (>= 0x80) as raw bytes so legitimate UTF-8 content
    // (degree signs, accented characters, etc.) survives. `from_utf8_lossy`
    // then reassembles valid UTF-8 and replaces only genuinely invalid byte
    // sequences with U+FFFD — unlike `byte as char`, which double-encodes every
    // high byte into mojibake. This is a faithful *display* rendering: invalid
    // bytes show as U+FFFD (`�`) rather than being hidden, so the text mirrors
    // reality. It is NOT the right source for igating to APRS-IS — a `String`
    // cannot hold raw bytes like `0xFF`, so feeding this to APRS-IS re-encodes
    // each U+FFFD to `EF BF BD`. Consumers that need byte-exact data (igating,
    // FCS re-checks) should read the raw `Ax25Frame::info` field directly, and
    // can use `count_suspect_bytes` to decide whether to forward it at all.
    let info: Vec<u8> = frame
        .info
        .iter()
        .copied()
        .filter(|&b| b == b'\t' || (0x20..0x7F).contains(&b) || b >= 0x80)
        .collect();
    s.push_str(&String::from_utf8_lossy(&info));

    s
}

/// Count bytes in an info field that signal corrupt / non-text content.
///
/// A byte is "suspect" when it is either:
///   * a C0 control byte (`< 0x20`) other than tab, CR, or LF — tab is legal in
///     APRS text and CR/LF are common benign TNC line-ending artifacts; or
///   * part of a sequence that is not valid UTF-8 (e.g. the lone `0xFF` idle/fill
///     bytes some trackers append, as seen from KE0HXD-7).
///
/// The two categories never overlap (C0 controls are valid single-byte UTF-8),
/// so the result is a straight sum. A non-zero count flags a packet whose info
/// field contains bytes that are almost certainly not real APRS payload —
/// useful for downranking or refusing to igate a misbehaving station's frames.
/// The raw bytes are left untouched; this only reports.
pub fn count_suspect_bytes(info: &[u8]) -> usize {
    let control = info
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\r' | b'\n'))
        .count();
    control + count_invalid_utf8(info)
}

/// Count bytes that are not part of a valid UTF-8 sequence.
fn count_invalid_utf8(mut bytes: &[u8]) -> usize {
    let mut count = 0;
    loop {
        match std::str::from_utf8(bytes) {
            Ok(_) => return count,
            Err(e) => match e.error_len() {
                // A defined-length invalid sequence: count it and continue past it.
                Some(len) => {
                    count += len;
                    bytes = &bytes[e.valid_up_to() + len..];
                }
                // An incomplete sequence at the very end: the remainder is invalid.
                None => return count + (bytes.len() - e.valid_up_to()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax25::frame::Ax25Frame;

    fn make_frame(src: &str, dst: &str, via: Vec<(String, bool)>, info: &[u8]) -> Ax25Frame {
        let (via_calls, via_heard): (Vec<String>, Vec<bool>) = via.into_iter().unzip();
        Ax25Frame {
            source: src.to_string(),
            destination: dst.to_string(),
            via: via_calls,
            via_heard,
            info: info.to_vec(),
        }
    }

    #[test]
    fn direct_packet_no_via() {
        let f = make_frame("KA9Q-1", "APDR15", vec![], b"/test info");
        assert_eq!(to_tnc2(&f), "KA9Q-1>APDR15:/test info");
    }

    #[test]
    fn via_with_h_bit_gets_star() {
        let f = make_frame(
            "KA9Q-1",
            "APDR15",
            vec![
                ("WIDE1-1".to_string(), false),
                ("KD9PDP-3".to_string(), true),
            ],
            b"!data",
        );
        assert_eq!(to_tnc2(&f), "KA9Q-1>APDR15,WIDE1-1,KD9PDP-3*:!data");
    }

    #[test]
    fn all_via_heard() {
        let f = make_frame(
            "W9XYZ",
            "APRS",
            vec![("RELAY".to_string(), true), ("WIDE".to_string(), true)],
            b">status",
        );
        assert_eq!(to_tnc2(&f), "W9XYZ>APRS,RELAY*,WIDE*:>status");
    }

    #[test]
    fn ssid_preserved_in_output() {
        let f = make_frame("N0CALL-9", "APDW16", vec![], b"=position");
        assert_eq!(to_tnc2(&f), "N0CALL-9>APDW16:=position");
    }

    #[test]
    fn control_bytes_stripped_tab_kept() {
        // NUL, CR, LF dropped; tab retained; printable ASCII passes through.
        let f = make_frame("KA9Q-1", "APDR15", vec![], b"a\x00b\rc\nd\te");
        assert_eq!(to_tnc2(&f), "KA9Q-1>APDR15:abcd\te");
    }

    #[test]
    fn valid_utf8_preserved_not_double_encoded() {
        // "23°C" — the degree sign is U+00B0 (UTF-8: 0xC2 0xB0). It must round-trip
        // intact, not become two mojibake characters as `byte as char` produced.
        let f = make_frame("KA9Q-1", "APDR15", vec![], "23°C".as_bytes());
        assert_eq!(to_tnc2(&f), "KA9Q-1>APDR15:23°C");
    }

    #[test]
    fn invalid_high_bytes_become_replacement_char() {
        // A lone 0xFF is not valid UTF-8 → replaced with U+FFFD rather than
        // emitted as raw garbage. The text rendering is a faithful display:
        // invalid bytes are shown, not hidden. (Igate consumers read raw bytes.)
        let f = make_frame("KA9Q-1", "APDR15", vec![], &[b'x', 0xFF, b'y']);
        assert_eq!(to_tnc2(&f), "KA9Q-1>APDR15:x\u{FFFD}y");
    }

    #[test]
    fn suspect_bytes_counts_ff_run_and_control_not_clean_text() {
        // Clean ASCII APRS payload → zero suspect bytes.
        assert_eq!(count_suspect_bytes(b"`pEBoA?k/]\"HS}145.190MHz"), 0);
        // Tab, CR, LF are allowed and don't count.
        assert_eq!(count_suspect_bytes(b"a\tb\r\n"), 0);
        // Valid UTF-8 (degree sign) doesn't count.
        assert_eq!(count_suspect_bytes("23°C".as_bytes()), 0);

        // Real-world KE0HXD-7 tail: 18 invalid 0xFF bytes, then '=' and CR.
        // The 0xFF run counts (18); '=' and CR do not.
        let mut info = b"`pEBoA?k/]\"HS}145.190MHz".to_vec();
        info.extend(std::iter::repeat_n(0xFF, 18));
        info.push(b'=');
        info.push(b'\r');
        assert_eq!(count_suspect_bytes(&info), 18);

        // Embedded C0 control bytes (NUL, 0x0F) from the earlier capture count too.
        assert_eq!(count_suspect_bytes(b"4P\x00\x0f4T"), 2);
    }
}
