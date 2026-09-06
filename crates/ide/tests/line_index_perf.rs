//! Run locally with `cargo test -p solsp-ide --release --test line_index_perf -- --ignored --nocapture`.

use std::{hint::black_box, time::Instant};

use rowan::TextSize;
use solsp_ide::{LineCol, LineIndex};

#[test]
#[ignore = "local benchmark for coordinate conversion on long lines"]
fn long_line_coordinate_conversion() {
    for (name, fragment) in [
        ("ASCII", "uint value = 1; "),
        ("Unicode", "uint value = 1; /* é🌍 */ "),
    ] {
        for repeats in [2_000, 4_000, 8_000] {
            let text = fragment.repeat(repeats);
            let started = Instant::now();
            let index = LineIndex::new(black_box(&text));
            let build = started.elapsed();
            let started = Instant::now();
            for i in 0..repeats {
                black_box(index.line_col(black_box(TextSize::from((i * fragment.len()) as u32))));
            }
            let forward = started.elapsed();
            let utf16_width = fragment.encode_utf16().count();
            let started = Instant::now();
            for i in 0..repeats {
                black_box(index.offset(black_box(LineCol {
                    line: 0,
                    col: (i * utf16_width) as u32,
                })));
            }
            let reverse = started.elapsed();
            println!("{name}: bytes={} queries={repeats} build={build:?} byte_to_utf16={forward:?} utf16_to_byte={reverse:?}", text.len());
        }
    }
}
