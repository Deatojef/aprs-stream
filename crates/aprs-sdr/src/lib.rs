//! `aprs-sdr` — self-contained SDR front-end for APRS.
//!
//! RTL-SDR I/Q → overlap-save fast-convolution channelizer (one shared forward
//! FFT, N narrowband outputs) → FM demod → 24 kHz audio → `aprs-modem` decode.
//! Replaces ka9q-radio for the APRS use case. This is a feasibility spike: the
//! channelizer supports many channels, but the binary wires a configurable few.

pub mod channelize;
pub mod device;
pub mod fm;
pub mod source;

pub use device::Gain;
pub use source::{spawn, FmParams, SdrHandles, SdrSourceConfig};
