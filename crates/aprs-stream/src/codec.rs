//! CBOR (de)serialization helpers plus a `cbor -> json` debug aid.
//!
//! Encoding is CBOR via `ciborium` (decision #3): native byte strings, real
//! binary floats, compact, clean cross-language support. The JSON helper exists
//! only to recover human-inspectability — it is not a wire format.

use crate::proto::AprsFrame;

/// Errors from encoding, decoding, or JSON conversion.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("cbor encode error: {0}")]
    Encode(String),
    #[error("cbor decode error: {0}")]
    Decode(String),
    #[error("json conversion error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encode a frame to its CBOR datagram payload.
pub fn encode(frame: &AprsFrame) -> Result<Vec<u8>, CodecError> {
    let mut buf = Vec::new();
    ciborium::into_writer(frame, &mut buf).map_err(|e| CodecError::Encode(e.to_string()))?;
    Ok(buf)
}

/// Decode a CBOR datagram payload back into a typed frame.
pub fn decode(bytes: &[u8]) -> Result<AprsFrame, CodecError> {
    ciborium::from_reader(bytes).map_err(|e| CodecError::Decode(e.to_string()))
}

/// Convert raw CBOR bytes to a pretty JSON string for human inspection.
///
/// This decodes via the generic CBOR value model (not the typed schema), so it
/// works even on version-mismatched or partially-understood frames. Note: CBOR
/// byte strings (e.g. raw AX.25) render as JSON arrays of integers, since JSON
/// has no byte-string type.
pub fn to_json(bytes: &[u8]) -> Result<String, CodecError> {
    let value: ciborium::value::Value =
        ciborium::from_reader(bytes).map_err(|e| CodecError::Decode(e.to_string()))?;
    Ok(serde_json::to_string_pretty(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::*;

    /// Build a frame whose `parsed` packet comes from a real TNC2 line, with a
    /// matching raw AX.25 body derived from the same packet so the two agree.
    fn frame_from_tnc2(tnc2: &[u8]) -> AprsFrame {
        let packet = aprs_decode::AprsPacket::decode_textual(tnc2).expect("parse tnc2");
        let ax25 = packet.encode_ax25().expect("encode ax25");
        AprsFrame::new(
            CaptureMeta {
                received_at_ms: 1_719_580_800_123,
                receiver: Some("packet.local".into()),
                decoder: Some("aprs-rtp/0.2.0".into()),
                ssrc: Some(144_390),
            },
            RfMeta {
                frequency_hz: Some(144_390_000),
                snr_db: None,
                audio_level: Some(AudioLevel {
                    rec: 52,
                    mark: 24,
                    space: 21,
                }),
                slicer_hits: Some(6),
                slicer_mask: Some(0b0011_1110),
                slicer_gains: Some(vec![0.5, 0.7, 1.0, 1.4, 2.0]),
            },
            true,
            ax25,
            None,
            Some(packet),
        )
    }

    #[test]
    fn round_trip_position() {
        let frame = frame_from_tnc2(b"W1AW-9>APRS,WIDE1-1,WIDE2-2:!4903.50N/07201.75W-EOSS chase");
        let bytes = encode(&frame).expect("encode");
        let back = decode(&bytes).expect("decode");
        assert_eq!(frame, back);
    }

    #[test]
    fn round_trip_various_payloads() {
        let tnc2: &[&[u8]] = &[
            b"W1AW>APRS:>net control online",                 // status
            b"KD9ABC>APDR15,qAR,KD9XYZ::W1AW-9   :Hello{001", // message
            b"W1AW>APRS:T#005,10,20,30,40,50,10101010",       // telemetry
            b"W1AW>APRS:~totally custom payload",             // unknown DTI
        ];
        for line in tnc2 {
            let frame = frame_from_tnc2(line);
            let bytes = encode(&frame).expect("encode");
            let back = decode(&bytes).expect("decode");
            assert_eq!(frame, back, "round-trip mismatch for {line:?}");
        }
    }

    #[test]
    fn round_trip_unparsed_frame() {
        // A FCS-valid frame the parser couldn't type: `parsed` is None but the
        // raw AX.25 still round-trips losslessly.
        let frame = AprsFrame::new(
            CaptureMeta {
                received_at_ms: 42,
                receiver: None,
                decoder: None,
                ssrc: None,
            },
            RfMeta::default(),
            true,
            vec![0x82, 0xa0, 0xb4, 0x00, 0xff, 0x03, 0xf0],
            None,
            None,
        );
        let bytes = encode(&frame).expect("encode");
        let back = decode(&bytes).expect("decode");
        assert_eq!(frame, back);
        assert!(back.parsed.is_none());
    }

    #[test]
    fn ax25_encodes_as_cbor_byte_string() {
        // Decision #3: raw bytes ride as a true CBOR byte string, not an array of
        // integers. A 7-byte string is `0x47` (major type 2, length 7) followed
        // by the 7 bytes.
        let ax25 = vec![0x82, 0xa0, 0xb4, 0x00, 0xff, 0x03, 0xf0];
        let frame = AprsFrame::new(
            CaptureMeta {
                received_at_ms: 0,
                receiver: None,
                decoder: None,
                ssrc: None,
            },
            RfMeta::default(),
            true,
            ax25.clone(),
            None,
            None,
        );
        let bytes = encode(&frame).expect("encode");
        assert!(
            bytes.windows(8).any(|w| w[0] == 0x47 && w[1..] == ax25[..]),
            "ax25 should serialize as a CBOR byte string"
        );
    }

    #[test]
    fn version_is_stamped() {
        let frame = frame_from_tnc2(b"W1AW>APRS:>hi");
        assert_eq!(frame.version, PROTOCOL_VERSION);
    }

    #[test]
    fn json_debug_helper_works() {
        let frame = frame_from_tnc2(b"W1AW>APRS:>hi");
        let bytes = encode(&frame).expect("encode");
        let json = to_json(&bytes).expect("to_json");
        assert!(json.contains("\"version\""));
        assert!(json.contains("\"parsed\""));
    }

    #[test]
    fn ax25_meta_round_trips_with_heard_bits() {
        let meta = Ax25Meta {
            source: "W1AW-9".into(),
            destination: "APRS".into(),
            via: vec![
                ViaHop {
                    call: "W1XYZ-1".into(),
                    heard: true,
                },
                ViaHop {
                    call: "WIDE2-1".into(),
                    heard: false,
                },
            ],
            heard_direct: false,
            heard_from: "W1XYZ-1".into(),
            dti: Some(b'!'),
            info_offset: Some(16),
            info_invalid_bytes: 1,
        };
        let frame = AprsFrame::new(
            CaptureMeta {
                received_at_ms: 7,
                receiver: None,
                decoder: None,
                ssrc: None,
            },
            RfMeta::default(),
            true,
            vec![0u8; 24],
            Some(meta.clone()),
            None,
        );
        let back = decode(&encode(&frame).expect("encode")).expect("decode");
        assert_eq!(back.ax25_meta.as_ref(), Some(&meta));
    }

    #[test]
    fn info_offset_slices_verbatim_info() {
        // A frame whose info field includes a non-UTF-8 byte (0xff): the whole
        // point of carrying the offset is byte-faithful info extraction. Header is
        // an arbitrary address+control+PID stand-in; `ax25[info_offset..]` must be
        // the exact info bytes.
        let header = [0x82, 0xa0, 0xb4, 0x00, 0xff, 0x03, 0xf0];
        let info: &[u8] = b"!4903.50N/07201.75W-\xff";
        let mut ax25 = header.to_vec();
        ax25.extend_from_slice(info);
        let meta = Ax25Meta {
            info_offset: Some(header.len() as u32),
            ..Default::default()
        };
        let frame = AprsFrame::new(
            CaptureMeta {
                received_at_ms: 0,
                receiver: None,
                decoder: None,
                ssrc: None,
            },
            RfMeta::default(),
            true,
            ax25,
            Some(meta),
            None,
        );
        let back = decode(&encode(&frame).expect("encode")).expect("decode");
        let off = back.ax25_meta.unwrap().info_offset.unwrap() as usize;
        assert_eq!(&back.ax25[off..], info);
    }

    #[test]
    fn v1_era_frame_decodes_with_defaults() {
        // Backward-compat guard: a producer predating the v2 fields emits a frame
        // with neither `ax25_meta` nor the new RfMeta fields. Simulate it with a
        // struct carrying only the original subset and confirm the missing fields
        // deserialize to their defaults (`None`) rather than failing.
        #[derive(serde::Serialize)]
        struct OldRfMeta {
            frequency_hz: Option<u64>,
        }
        #[derive(serde::Serialize)]
        struct OldFrame {
            version: u8,
            capture: CaptureMeta,
            rf: OldRfMeta,
            crc_ok: bool,
            #[serde(with = "serde_bytes")]
            ax25: Vec<u8>,
            parsed: Option<aprs_decode::AprsPacket>,
        }

        let old = OldFrame {
            version: 1,
            capture: CaptureMeta {
                received_at_ms: 123,
                receiver: None,
                decoder: None,
                ssrc: None,
            },
            rf: OldRfMeta {
                frequency_hz: Some(144_390_000),
            },
            crc_ok: true,
            ax25: vec![0x82, 0xa0, 0xb4, 0x00, 0xff, 0x03, 0xf0],
            parsed: None,
        };

        let mut buf = Vec::new();
        ciborium::into_writer(&old, &mut buf).expect("encode old frame");
        let back = decode(&buf).expect("decode old frame");

        assert_eq!(back.version, 1);
        assert!(back.ax25_meta.is_none());
        assert!(back.rf.slicer_gains.is_none());
        assert!(back.rf.slicer_mask.is_none());
        assert_eq!(back.rf.frequency_hz, Some(144_390_000));
    }
}
