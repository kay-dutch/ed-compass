//! ED Compass — spectrogram anomaly detector and audio direction finder.
//!
//! The library half of the application, so the analysis chain can be exercised
//! by tests and by the synthetic test modes on any platform. Only `audio::capture`
//! and `audio::device` are Windows-specific.

pub mod analysis;
pub mod app;
pub mod audio;
pub mod capture_writer;
pub mod config;
pub mod game_window;
pub mod journal;
pub mod pipeline;
pub mod retention;
pub mod single_instance;
pub mod ui;
