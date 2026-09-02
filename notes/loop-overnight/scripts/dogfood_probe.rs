//! Parse every stored path in the dogfood library and print one row each.
//!
//! **Two entry points, because they answer different questions.** The corpus
//! harness calls `nightjar_core::parse_filename` on a basename, and three
//! instruments on this project have shared that blindness: none of them can see
//! a change above the parser. `nightjar_scanner::stored_parse` is what the
//! product actually calls (`scanner/src/lib.rs`), and it reads the path. Both
//! are printed so a diff says which layer moved.
//!
//! Input on stdin: `root<TAB>stored_path`, one per line.
//! Output on stdout: TSV, one row per input, no header.
//!
//! No database, no network, no writes. The caller extracts the paths.

use std::io::{BufWriter, Read, Write};

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let out = std::io::stdout();
    let mut out = BufWriter::new(out.lock());
    for line in input.lines() {
        let Some((root, path)) = line.split_once('\t') else {
            continue;
        };
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let b = nightjar_core::parse_filename(base);
        let s = nightjar_scanner::stored_parse(path, root);
        writeln!(
            out,
            "{path}\tbase\t{}\t{:?}\t{:?}\t{:?}\t{:?}\t{:?}\t{}\n{path}\tstored\t{}\t{:?}\t{:?}\t{:?}\t{:?}\t{:?}\t{}",
            b.title, b.kind, b.year, b.season, b.episode, b.episode_end, b.episode_absolute,
            s.title, s.kind, s.year, s.season, s.episode, s.episode_end, s.episode_absolute,
        )
        .unwrap();
    }
}
