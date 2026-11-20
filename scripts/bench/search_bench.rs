// Standalone indexer cost probe. No external crates: rustc compiles this alone,
// so the numbers describe reading and scanning, not dependency build time.
//
// Usage: indexbench <root> [threads]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TEXT_EXTENSIONS: &[&str] = &[
    // Rust / web / scripts
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "md", "mdx", "toml", "json", "yml", "yaml", "css",
    "html", "sql", "sh", "ps1", "py", "go", "java", "rb", "swift",
    // C / C++ — the bulk of a native client
    "c", "h", "cc", "cpp", "cxx", "hpp", "hxx", "inl",
    // Qt / project metadata that a C++ client index must include
    "ui", "qrc", "def", "rc", "pc", "manifest", // Other scripting the client ships
    "lua",
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

/// Count whole-word-ish identifier occurrences without allocating per hit.
/// This is the shape a "find references" scan takes: one pass over the bytes,
/// a window check on both sides of the match.
fn count_identifier(bytes: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || bytes.len() < needle.len() {
        return 0;
    }
    let mut hits = 0;
    let mut index = 0;
    while index + needle.len() <= bytes.len() {
        match find_from(&bytes[index..], needle) {
            Some(found) => {
                let at = index + found;
                let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
                let after = at + needle.len();
                let after_ok = after >= bytes.len() || !is_word_byte(bytes[after]);
                if before_ok && after_ok {
                    hits += 1;
                }
                index = at + 1;
            }
            None => break,
        }
    }
    hits
}

fn find_from(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let threads: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    // ── 1. Walk ────────────────────────────────────────────────────────────
    let walk_start = Instant::now();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let walk_time = walk_start.elapsed();

    let total_bytes: u64 = files
        .iter()
        .filter_map(|path| fs::metadata(path).ok())
        .map(|meta| meta.len())
        .sum();

    println!("files: {}", files.len());
    println!(
        "bytes: {} ({:.2} MiB)",
        total_bytes,
        total_bytes as f64 / (1024.0 * 1024.0)
    );
    println!("walk: {:.1} ms", ms(walk_time));

    // ── 2. Read everything (page-cache warm after the first run) ───────────
    for pass in 1..=3 {
        let start = Instant::now();
        let mut read_bytes = 0usize;
        for path in &files {
            if let Ok(content) = fs::read(path) {
                read_bytes += content.len();
            }
        }
        let elapsed = start.elapsed();
        println!(
            "read pass {}: {:.1} ms ({} bytes)",
            pass,
            ms(elapsed),
            read_bytes
        );
    }

    // ── 3. Read + scan, single thread ──────────────────────────────────────
    let needles: [&[u8]; 5] = [b"fn", b"struct", b"let", b"sidebar_rows", b"theme"];
    for pass in 1..=3 {
        let start = Instant::now();
        let mut hits = 0usize;
        let mut scanned = 0usize;
        for path in &files {
            let Ok(content) = fs::read(path) else {
                continue;
            };
            scanned += content.len();
            for needle in needles {
                hits += count_identifier(&content, needle);
            }
        }
        println!(
            "read+scan x{} pass {}: {:.1} ms ({} hits, {} bytes)",
            needles.len(),
            pass,
            ms(start.elapsed()),
            hits,
            scanned
        );
    }

    // ── 4. Read + scan, N threads ──────────────────────────────────────────
    if threads > 1 {
        let files = Arc::new(files.clone());
        let next = Arc::new(AtomicUsize::new(0));
        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..threads {
            let files = Arc::clone(&files);
            let next = Arc::clone(&next);
            handles.push(std::thread::spawn(move || {
                let mut hits = 0usize;
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(index) else { break };
                    let Ok(content) = fs::read(path) else {
                        continue;
                    };
                    for needle in needles {
                        hits += count_identifier(&content, needle);
                    }
                }
                hits
            }));
        }
        let hits: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        println!(
            "read+scan x{} with {} threads: {:.1} ms ({} hits)",
            needles.len(),
            threads,
            ms(start.elapsed()),
            hits
        );
    }

    // ── 5. Declaration-line extraction, single thread ──────────────────────
    // The index a go-to-definition build actually stores: for every line, is
    // it a declaration, and what name does it declare. Covers the declaration
    // shapes a C/C++ tree uses most, plus the Rust ones Waku itself needs.
    let start = Instant::now();
    let mut declarations = 0usize;
    for path in &files {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines() {
            let trimmed = line.trim_start();
            for prefix in [
                "fn ",
                "pub fn ",
                "struct ",
                "pub struct ",
                "enum ",
                "impl ",
                "class ",
                "void ",
                "int ",
                "bool ",
                "double ",
                "float ",
                "static ",
                "inline ",
            ] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let name = rest
                        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .next()
                        .unwrap_or("");
                    if !name.is_empty() {
                        declarations += 1;
                    }
                    break;
                }
            }
        }
    }
    println!(
        "declaration scan: {:.1} ms ({} declarations)",
        ms(start.elapsed()),
        declarations
    );
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
