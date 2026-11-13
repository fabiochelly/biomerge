//! Measures the engine on generated files and on two real folders.
//! `cargo run --release -p biomerge-core --example bench [LEFT_DIR RIGHT_DIR [THREADS...]]`

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::time::Instant;

use biomerge_core::diff::{DiffOptions, diff_lines};
use biomerge_core::dirscan::{self, ScanEvent, ScanOptions};
use biomerge_core::document::Line;
use biomerge_core::highlight::{self, SyntaxTheme};

fn generated(lines: usize, seed: usize) -> Vec<String> {
    (0..lines)
        .map(|i| match (i * 7 + seed * (i % 97 / 96)) % 5 {
            0 => format!("    let value_{i} = compute(input_{}, {});", i % 100, i % 13),
            1 => format!("fn function_{}(argument: u32) -> Result<u32, Error> {{", i % 1000),
            2 => format!("    // comment number {} about the code below", i % 500),
            3 => "    }".to_owned(),
            _ => format!("    self.items.push(Item::new({i}));"),
        })
        .collect()
}

fn main() {
    for lines in [10_000, 100_000, 1_000_000] {
        let left = generated(lines, 0);
        let right = generated(lines, 1);
        let started = Instant::now();
        let result = diff_lines(&left, &right, DiffOptions::default());
        println!(
            "diff {lines:>9} lignes : {:>8.1} ms, {} blocs",
            started.elapsed().as_secs_f64() * 1e3,
            result.blocks.len()
        );
    }

    let lines: Vec<Line> = generated(20_000, 0)
        .into_iter()
        .map(|text| Line {
            text,
            eol: biomerge_core::document::Eol::Lf,
        })
        .collect();
    highlight::preload();
    let started = Instant::now();
    highlight::highlight_lines(
        "Rust",
        SyntaxTheme::Light,
        &lines,
        0,
        &highlight::Checkpoints::default(),
        &AtomicBool::new(false),
        |_, _| {},
    );
    let seconds = started.elapsed().as_secs_f64();
    println!("coloration Rust : {:.0} lignes/s", lines.len() as f64 / seconds);

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() >= 2 {
        let threads: Vec<usize> = args[2..].iter().map(|t| t.parse().unwrap()).collect();
        for count in threads {
            let started = Instant::now();
            let (tx, rx) = channel();
            dirscan::scan(
                PathBuf::from(&args[0]),
                PathBuf::from(&args[1]),
                ScanOptions {
                    compare_threads: Some(count),
                    ..ScanOptions::default()
                },
                Arc::new(AtomicBool::new(false)),
                tx,
                || {},
            );
            for event in rx {
                match event {
                    ScanEvent::Tree { entries, walk_time, .. } => {
                        print!("{} éléments, parcours {walk_time:?} ; ", entries.len());
                    }
                    ScanEvent::Done { bytes_compared, threads, .. } => println!(
                        "{threads} threads : total {:?}, {} Mo lus",
                        started.elapsed(),
                        bytes_compared / 1_000_000
                    ),
                    ScanEvent::Failed(message) => println!("échec : {message}"),
                    ScanEvent::Progress(_) => {}
                }
            }
        }
    }
}
