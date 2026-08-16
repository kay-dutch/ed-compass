//! Run the structure detector over an existing spectrogram image.
//!
//! Useful for judging someone else's screenshot with the same metrics the live
//! detector uses, instead of by eye. Handles both palettes: dark-background
//! images (loud is bright) and light-background ones (loud is dark), which is
//! what Audacity and Sonic Visualiser produce by default.
//!
//! ```sh
//! cargo run --release --example analyze_image -- other/other_spectre.png
//! cargo run --release --example analyze_image -- shot.png --invert
//! ```

use ed_compass::analysis::structure::{StructureScanner, analyze};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: analyze_image <image.png> [--invert] [--tile N]");
        std::process::exit(2);
    };
    let force_invert = args.iter().any(|a| a == "--invert");
    let tile: usize = args
        .iter()
        .position(|a| a == "--tile")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(96);

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("could not open {path}: {e}");
            std::process::exit(1);
        }
    };
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = match decoder.read_info() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("not a readable PNG: {e}");
            std::process::exit(1);
        }
    };
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = match reader.next_frame(&mut buf) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("could not decode: {e}");
            std::process::exit(1);
        }
    };

    let (w, h) = (info.width as usize, info.height as usize);
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => {
            eprintln!("indexed PNGs are not supported; re-save as RGB");
            std::process::exit(1);
        }
    };

    // Luminance, then decide which way up the palette runs.
    let mut gray = vec![0u8; w * h];
    for i in 0..w * h {
        let p = &buf[i * channels..];
        gray[i] = if channels >= 3 {
            ((p[0] as u32 * 30 + p[1] as u32 * 59 + p[2] as u32 * 11) / 100) as u8
        } else {
            p[0]
        };
    }

    // A light-background spectrogram has a high mean; a dark one is mostly floor.
    let mean = gray.iter().map(|v| *v as u64).sum::<u64>() / (w * h) as u64;
    let invert = force_invert || mean > 128;
    if invert {
        for v in gray.iter_mut() {
            *v = 255 - *v;
        }
    }

    println!("{path}");
    println!(
        "  {w} x {h}, palette: {}",
        if invert {
            "light background (inverted)"
        } else {
            "dark background"
        }
    );
    println!();

    let whole = analyze(&gray, w, h);
    println!("whole image:");
    report(&whole);

    let scanner = StructureScanner {
        tile_width: tile,
        tile_height: tile,
    };
    let (best, x, y) = scanner.scan(&gray, w, h);
    println!();
    println!("best {tile}x{tile} tile, at ({x}, {y}):");
    report(&best);

    println!();
    println!("reading:");
    println!("  diagonality  > 0.6  swept strokes — drawn structure");
    println!("               < 0.35 verticals (clicks) or horizontals (drones)");
    println!("  sparsity     > 0.8  thin lines on a quiet ground");
    println!("  coherence    > 0.5  locally linear rather than mush");
}

fn report(s: &ed_compass::analysis::structure::StructureScore) {
    println!("  score                 {:.3}", s.score);
    println!("  coherence             {:.3}", s.coherence);
    println!("  sparsity              {:.3}", s.sparsity);
    println!("  orientation diversity {:.3}", s.orientation_diversity);
    println!("  diagonality           {:.3}", s.diagonality);
    println!("  edge pixels           {}", s.edge_pixels);
}
