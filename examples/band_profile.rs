//! Where does a signal actually live in frequency?
//!
//! Prints a log-spaced band energy profile for a WAV or FLAC file, plus the
//! band that holds the bulk of the energy once a constant floor is removed.
//! Used to size the display band from measurement rather than assumption.
//!
//! ```sh
//! cargo run --release --example band_profile -- reference.flac
//! ```

use ed_compass::analysis::statistics::power_to_dbfs;
use ed_compass::analysis::stft::Stft;
use ed_compass::audio::file_input;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: band_profile <file.wav|file.flac>");
        std::process::exit(2);
    };
    let mut source = match file_input::load(std::path::Path::new(&path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not load {path}: {e:#}");
            std::process::exit(1);
        }
    };

    let format = source.format().clone();
    let channels = format.channels;
    let sample_rate = format.sample_rate;
    println!(
        "{}  —  {:.1} s",
        format.describe(),
        source.duration_seconds()
    );

    let size = 4096;
    let hop = 2048;
    let mut stft = Stft::new(size, hop);
    let mut spectrum = stft.make_spectrum();
    let bins = spectrum.len();

    // Mean and peak power per bin across the whole file.
    let mut mean = vec![0.0f64; bins];
    let mut peak = vec![0.0f64; bins];
    let mut powers = vec![0.0f32; bins];
    let mut frames = 0usize;

    let mut interleaved = Vec::new();
    // Everything the file has; the cap is a guard against a pathological input.
    source.render(1 << 30, &mut interleaved);
    let mono: Vec<f32> = interleaved
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();

    let mut start = 0;
    while start + size <= mono.len() {
        stft.process(&mono[start..start + size], &mut spectrum);
        stft.powers(&spectrum, &mut powers);
        for (i, p) in powers.iter().enumerate() {
            mean[i] += *p as f64;
            peak[i] = peak[i].max(*p as f64);
        }
        frames += 1;
        start += hop;
    }
    if frames == 0 {
        eprintln!("file too short to analyze");
        std::process::exit(1);
    }
    for m in mean.iter_mut() {
        *m /= frames as f64;
    }

    // Log-spaced report bands.
    let nyquist = sample_rate as f32 / 2.0;
    let low_hz = 20.0f32;
    let bands = 28;
    let ratio = (nyquist / low_hz).powf(1.0 / bands as f32);
    let bin_hz = nyquist / (bins - 1) as f32;

    println!();
    println!(
        "{:>10}  {:>10}  {:>9}  {:>9}  peak level",
        "from", "to", "mean dB", "peak dB"
    );

    let mut rows = Vec::new();
    for b in 0..bands {
        let lo = low_hz * ratio.powi(b);
        let hi = low_hz * ratio.powi(b + 1);
        let lo_bin = ((lo / bin_hz).floor() as usize).min(bins - 1);
        let hi_bin = ((hi / bin_hz).ceil() as usize).clamp(lo_bin + 1, bins);

        let m: f64 = mean[lo_bin..hi_bin].iter().sum::<f64>() / (hi_bin - lo_bin) as f64;
        let p: f64 = mean[lo_bin..hi_bin].iter().copied().fold(0.0f64, f64::max);
        let pk: f64 = peak[lo_bin..hi_bin].iter().copied().fold(0.0f64, f64::max);
        rows.push((lo, hi, power_to_dbfs(m as f32), power_to_dbfs(pk as f32), p));
    }

    let loudest = rows.iter().map(|r| r.3).fold(f32::NEG_INFINITY, f32::max);

    for (lo, hi, mean_db, peak_db, _) in &rows {
        let bar = ((peak_db - (loudest - 60.0)) / 60.0 * 40.0).clamp(0.0, 40.0) as usize;
        println!(
            "{:>10.0}  {:>10.0}  {:>9.1}  {:>9.1}  {}",
            lo,
            hi,
            mean_db,
            peak_db,
            "#".repeat(bar)
        );
    }

    // The band holding the signal: contiguous bands within 25 dB of the loudest.
    let cutoff = loudest - 25.0;
    let first = rows.iter().position(|r| r.3 >= cutoff);
    let last = rows.iter().rposition(|r| r.3 >= cutoff);
    if let (Some(a), Some(b)) = (first, last) {
        println!();
        println!(
            "energy within 25 dB of the peak spans {:.0} Hz .. {:.0} Hz",
            rows[a].0, rows[b].1
        );
        println!(
            "suggested display band: spectrogram_min_hz = {:.0}, spectrogram_max_hz = {:.0}",
            (rows[a].0 * 0.8).max(20.0),
            (rows[b].1 * 1.25).min(nyquist)
        );
    }
}
