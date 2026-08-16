//! Measures how much CPU and memory the analysis chain actually costs.
//!
//! Reports a realtime factor: how many seconds of audio are analyzed per second
//! of wall clock on one core. A factor of 100x means the detector uses roughly
//! 1% of a core to keep up with a live stream.
//!
//! ```sh
//! cargo run --release --example throughput
//! cargo run --release --example throughput -- 8 48000 300
//! ```

use std::time::Instant;

use ed_compass::audio::format::{MASK_7_1, MASK_STEREO};
use ed_compass::audio::synthetic::{SyntheticSource, TestSignal};
use ed_compass::audio::{SampleFormat, StreamFormat};
use ed_compass::config::Config;
use ed_compass::pipeline::AnalysisEngine;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let channels: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(8);
    let rate: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(48_000);
    let seconds: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300.0);
    let snapshot_hz: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10.0);

    let mask = match channels {
        2 => MASK_STEREO,
        8 => MASK_7_1,
        _ => 0,
    };
    let format = StreamFormat::new(rate, channels, mask, SampleFormat::F32);
    let mut cfg = Config::default();
    cfg.analysis_update_hz = snapshot_hz;

    println!("stream:  {}", format.describe());
    println!("audio:   {seconds:.0} s");
    println!();

    let mut engine = AnalysisEngine::new(cfg.clone(), format.clone());
    let mut source = SyntheticSource::new(TestSignal::Landscape, format.clone(), -55.0);

    // 50 ms blocks, matching what a shared-mode endpoint delivers.
    let block = (rate as f32 * 0.05) as usize;
    let blocks = (seconds / 0.05) as usize;
    // Snapshots at the configured display rate, since that work is real too.
    let blocks_per_snapshot = ((1.0 / cfg.analysis_update_hz) / 0.05).max(1.0) as usize;

    let mut buf = Vec::with_capacity(block * channels);
    let mut detections = 0usize;
    let mut snapshots = 0usize;

    // Render first so signal generation is not counted as analysis cost.
    let mut rendered: Vec<Vec<f32>> = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        buf.clear();
        source.render(block, &mut buf);
        rendered.push(buf.clone());
    }

    let start = Instant::now();
    for (i, chunk) in rendered.iter().enumerate() {
        detections += engine.push_interleaved(chunk).len();
        if i % blocks_per_snapshot == 0 {
            let _ = engine.snapshot();
            snapshots += 1;
        }
    }
    let elapsed = start.elapsed();

    let ring_mb = engine.ring().bytes() as f64 / 1_048_576.0;
    let waterfall_mb = engine.waterfall().bytes() as f64 / 1_048_576.0;
    let longterm_mb = engine.long_term().bytes() as f64 / 1_048_576.0;
    let realtime = seconds as f64 / elapsed.as_secs_f64();

    println!("wall clock:      {:.2} s", elapsed.as_secs_f64());
    println!(
        "realtime factor: {realtime:.0}x  ({:.2}% of one core)",
        100.0 / realtime
    );
    println!(
        "per audio second: {:.2} ms",
        elapsed.as_secs_f64() * 1000.0 / seconds as f64
    );
    println!();
    println!("pcm ring:        {ring_mb:>8.1} MB");
    println!("waterfall tier:  {waterfall_mb:>8.1} MB");
    println!("long-term tier:  {longterm_mb:>8.1} MB");
    println!(
        "resident total:  {:>8.1} MB",
        ring_mb + waterfall_mb + longterm_mb
    );
    println!();
    println!("detections: {detections}, snapshots: {snapshots}");
}
