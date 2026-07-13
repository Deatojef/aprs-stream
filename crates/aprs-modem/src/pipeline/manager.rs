use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::{
    AprsPacket,
    audio::AudioBlock,
    config::DecoderConfig,
    pipeline::stream_decoder,
};

/// Receive `AudioBlock`s from any source and dispatch each to a per-SSRC
/// `StreamDecoder` blocking task, creating new decoders on first sight of an SSRC.
///
/// Runs as an async tokio task; blocks are forwarded synchronously to the
/// blocking DSP threads via bounded `std::sync::mpsc::SyncSender` channels.
///
/// This is the source-agnostic core: it consumes an `AudioBlock` receiver rather
/// than owning a socket. Ported from `aprs-rtp`'s `pipeline::manager::run`, with
/// the RTP `listener::spawn(source)` replaced by the caller-supplied `audio_rx`.
pub async fn run_blocks(
    mut audio_rx: mpsc::Receiver<AudioBlock>,
    decoder: DecoderConfig,
    aprs_tx: mpsc::Sender<AprsPacket>,
) {
    // Map SSRC → sender to the per-SSRC blocking DSP thread.
    let mut decoders: HashMap<u32, std::sync::mpsc::SyncSender<AudioBlock>> = HashMap::new();

    while let Some(block) = audio_rx.recv().await {
        let ssrc = block.ssrc;
        let sample_rate = block.sample_rate;

        let entry = decoders.entry(ssrc).or_insert_with(|| {
            tracing::info!(ssrc, sample_rate, "new SSRC — spawning stream decoder");
            stream_decoder::spawn(ssrc, decoder.clone(), sample_rate, aprs_tx.clone())
        });

        // If the blocking thread died (channel disconnected), remove and respawn
        // on the next block for this SSRC.
        if entry.try_send(block).is_err() {
            tracing::warn!(ssrc, "stream decoder stalled or closed; respawning");
            decoders.remove(&ssrc);
        }
    }

    tracing::info!("audio source closed; manager exiting");
}
