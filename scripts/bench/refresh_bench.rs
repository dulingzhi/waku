// Incremental-refresh cost probe. Answers the question the full-scan benchmark
// cannot: after the index exists, what does one save cost?
//
// Usage: incrbench <root>

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

const TEXT_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "md", "mdx", "toml", "json", "yml", "yaml",
    "css", "html", "sql", "sh", "ps1", "py", "go", "java", "rb", "swift", "c", "h", "cc", "cpp",
    "cxx", "hpp", "hxx", "inl", "ui", "qrc", "def", "rc", "pc", "manifest", "lua",
];

fn is_text(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| TEXT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == "target" || name == "node_modules" || name == "dist" {
            continue;
        }
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => walk(&path, out),
            Ok(file_type) if file_type.is_file() && is_text(&path) => out.push(path),
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    Some(Stamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));

    // ── Build the initial stat table ───────────────────────────────────────
    let start = Instant::now();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let walk_time = start.elapsed();

    let start = Instant::now();
    let mut table: HashMap<PathBuf, Stamp> = HashMap::with_capacity(files.len());
    for path in &files {
        if let Some(stamp) = stamp(path) {
            table.insert(path.clone(), stamp);
        }
    }
    let stat_time = start.elapsed();
    println!(
        "build: {} files, walk {:.1} ms, stat-all {:.1} ms",
        table.len(),
        ms(walk_time),
        ms(stat_time)
    );

    // ── A poll: the check that runs on a timer, with no file changed ────────
    println!();
    println!("── poll with no changes ──");
    for pass in 1..=5 {
        let start = Instant::now();
        let mut changed = 0usize;
        for (path, previous) in &table {
            if stamp(path) != Some(*previous) {
                changed += 1;
            }
        }
        println!(
            "poll {}: {:.1} ms ({} changed of {})",
            pass,
            ms(start.elapsed()),
            changed,
            table.len()
        );
    }

    // ── The same poll without the walk, which is what a real tick looks like ─
    // A real tick has no file list yet: it must walk to discover new files too.
    println!();
    println!("── poll including a walk (what a real tick must do) ──");
    for pass in 1..=3 {
        let start = Instant::now();
        let mut current = Vec::new();
        walk(&root, &mut current);
        let walk_elapsed = start.elapsed();
        let mut changed = 0usize;
        for path in &current {
            match table.get(path) {
                Some(previous) if stamp(path) == Some(*previous) => {}
                _ => changed += 1,
            }
        }
        // Set membership, not a linear scan: the first version of this probe
        // used `current.contains`, which is O(n^2) and reported 18.5 seconds of
        // "tick" that was entirely its own fault.
        let current_set: std::collections::HashSet<&PathBuf> = current.iter().collect();
        let removed = table
            .keys()
            .filter(|path| !current_set.contains(path))
            .count();
        println!(
            "tick {}: {:.1} ms total (walk {:.1} ms, {} changed, {} removed)",
            pass,
            ms(start.elapsed()),
            ms(walk_elapsed),
            changed,
            removed
        );
    }

    // ── Reindex one changed file ───────────────────────────────────────────
    println!();
    println!("── reindex one changed file ──");
    let biggest = files
        .iter()
        .filter_map(|path| fs::metadata(path).ok().map(|meta| (path, meta.len())))
        .max_by_key(|(_, len)| *len);
    if let Some((path, len)) = biggest {
        let start = Instant::now();
        let content = fs::read_to_string(path).unwrap_or_default();
        let read_elapsed = start.elapsed();
        let start = Instant::now();
        let lines = content.lines().count();
        let elapsed = start.elapsed();
        println!(
            "largest file: {} ({:.2} MiB, {} lines)",
            path.display(),
            len as f64 / (1024.0 * 1024.0),
            lines
        );
        println!(
            "read: {:.1} ms, count lines: {:.1} ms",
            ms(read_elapsed),
            ms(elapsed)
        );
    }

    // ── Memory: what the stored index would hold ───────────────────────────
    // Every identifier occurrence, stored as (name offset in a pool, file id,
    // line). 16 bytes a hit is the honest lower bound without interning.
    println!();
    println!("── index memory estimate ──");
    let start = Instant::now();
    let mut occurrences = 0usize;
    let mut declaration_lines = 0usize;
    for path in &files {
        let Ok(content) = fs::read(path) else {
            continue;
        };
        for byte in &content {
            if *byte == b'\n' {
                occurrences += 1;
            }
        }
        declaration_lines += 0;
    }
    println!(
        "total newlines (proxy for lines): {} scanned in {:.1} ms",
        occurrences,
        ms(start.elapsed())
    );
    println!(
        "at 16 bytes/line if every line were stored: {:.1} MiB",
        occurrences as f64 * 16.0 / (1024.0 * 1024.0)
    );
    let _ = declaration_lines;
}

fn ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
