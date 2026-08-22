//! Spectral analysis, anomaly detection, and direction finding.
//!
//! The primary instrument is the STFT. Everything in `statistics` exists to
//! confirm the capture is healthy, not to find signals.

pub mod direction;
pub mod fold;
pub mod keying;
pub mod kurtosis;
pub mod morse;
pub mod novelty;
pub mod periodicity;
pub mod spectrogram;
pub mod statistics;
pub mod stft;
pub mod structure;
pub mod trace;
