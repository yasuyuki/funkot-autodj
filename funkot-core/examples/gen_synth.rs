//! Generate synthetic Funkot-like test tracks into a directory.
//!
//! Usage (inside the dev container):
//!   cargo run -p funkot-core --example gen_synth --features testutil --release -- testdata/synth

use std::time::SystemTime;

use funkot_core::owned_wav::Checkout;
use funkot_core::testutil::{synth_track, write_wav_file};

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "testdata/synth".to_string());
    let dir = std::path::PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {e}", dir.display());
        std::process::exit(1);
    }

    let checkout = Checkout::this();
    let owned = checkout.open(SystemTime::now());

    // (name, bpm, intro_bars, main_bars, outro_bars)
    let specs = [
        ("track_a_180_i16_o16.wav", 180.0, 16u32, 48u32, 16u32),
        ("track_b_178_i32_o8.wav", 178.0, 32, 48, 8),
        ("track_c_181_i64_o64.wav", 181.0, 64, 32, 64),
    ];

    for (name, bpm, intro, main, outro) in specs {
        let buf = synth_track(bpm, intro, main, outro, 44_100);
        match owned.write_owned(&dir, name, SystemTime::now(), |path| write_wav_file(path, &buf)) {
            Ok(((), claimed)) => println!(
                "wrote {} ({bpm} BPM, intro {intro} / main {main} / outro {outro} bars) [{claimed}]",
                dir.join(name).display()
            ),
            Err(e) => {
                eprintln!("failed to write {}: {e}", dir.join(name).display());
                std::process::exit(1);
            }
        }
    }
}
