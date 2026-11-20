// Query-latency probe: one identifier, parallel, capped. This measures the
// design that needs no index at all — every query re-scans the tree.
//
// Usage: querybench <root> <identifier> [threads]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

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

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whole-word hits with line numbers, capped. This is what "find references"
/// returns, and the cap is what keeps a common name from flooding the answer.
fn scan(bytes: &[u8], needle: &[u8], cap: usize) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    if needle.is_empty() || bytes.len() < needle.len() {
        return hits;
    }
    let mut line = 1usize;
    let mut index = 0usize;
    let mut line_start = 0usize;
    while index + needle.len() <= bytes.len() {
        let Some(found) = bytes[index..]
            .windows(needle.len())
            .position(|window| window == needle)
        else {
            break;
        };
        let at = index + found;
        // Count newlines up to `at` to keep the line number honest.
        for byte in &bytes[line_start..at] {
            if *byte == b'\n' {
                line += 1;
            }
        }
        line_start = at;
        let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            hits.push((line, at));
            if hits.len() >= cap {
                return hits;
            }
        }
        index = at + 1;
    }
    hits
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let identifier = args.next().unwrap_or_else(|| "main".into());
    let threads: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);
    // A global result cap, which is what a real find-references UI applies:
    // once it has enough to show, more scanning is wasted work.
    let global_cap: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(usize::MAX);

    let start = Instant::now();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let walk_time = start.elapsed();
    println!("tree: {} files, walk {:.1} ms", files.len(), ms(walk_time));
    let files = Arc::new(files);
    let needle = Arc::new(identifier.clone().into_bytes());

    for pass in 1..=5 {
        let next = Arc::new(AtomicUsize::new(0));
        let found = Arc::new(AtomicUsize::new(0));
        let scanned = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..threads {
            let files = Arc::clone(&files);
            let next = Arc::clone(&next);
            let needle = Arc::clone(&needle);
            let found = Arc::clone(&found);
            let scanned = Arc::clone(&scanned);
            handles.push(std::thread::spawn(move || {
                loop {
                    // Stop the whole query once the cap is met.
                    if found.load(Ordering::Relaxed) >= global_cap {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(index) else { break };
                    let Ok(content) = fs::read(path) else {
                        continue;
                    };
                    scanned.fetch_add(1, Ordering::Relaxed);
                    let hits = scan(&content, &needle, 200);
                    if !hits.is_empty() {
                        found.fetch_add(hits.len(), Ordering::Relaxed);
                    }
                }
            }));
        }
        for handle in handles {
            let _ = handle.join();
        }
        println!(
            "query '{}' pass {}: {:.1} ms ({} hits, {} files read, cap {})",
            identifier,
            pass,
            ms(start.elapsed()),
            found.load(Ordering::Relaxed),
            scanned.load(Ordering::Relaxed),
            global_cap
        );
    }
}

fn ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
