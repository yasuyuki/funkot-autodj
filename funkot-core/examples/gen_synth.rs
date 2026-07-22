//! Generate synthetic Funkot-like test tracks into a directory.
//!
//! Usage (inside the dev container):
//!   cargo run -p funkot-core --example gen_synth --features testutil --release -- testdata/synth

use funkot_core::testutil::{synth_track, write_wav};

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "testdata/synth".to_string());
    let dir = std::path::PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {e}", dir.display());
        std::process::exit(1);
    }

    // (name, bpm, intro_bars, main_bars, outro_bars)
    let specs = [
        ("track_a_180_i16_o16.wav", 180.0, 16u32, 48u32, 16u32),
        ("track_b_178_i32_o8.wav", 178.0, 32, 48, 8),
        ("track_c_181_i64_o64.wav", 181.0, 64, 32, 64),
    ];

    for (name, bpm, intro, main, outro) in specs {
        let path = dir.join(name);
        let buf = synth_track(bpm, intro, main, outro, 44_100);
        match write_wav(&path, &buf) {
            Ok(()) => println!(
                "wrote {} ({bpm} BPM, intro {intro} / main {main} / outro {outro} bars)",
                path.display()
            ),
            Err(e) => {
                eprintln!("failed to write {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
}
