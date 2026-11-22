//! Unsaved work, kept on disk so a crash or a forced quit cannot lose it.
//!
//! Winc keeps its own version of this in `winc_editor::backup`, but that module
//! is bound to its `Editor` — an owner of many documents, panes and tabs, which
//! Waku does not have. Waku's editor is one document per open file, owned by the
//! right panel, so the round trip is written here against what actually exists.
//! Keeping the promise is what matters: the user should never have to answer
//! "save?" for a document they are not done with, and a process that dies must
//! not take their typing with it.
//!
//! ## What is written
//!
//! One file per document, in a directory keyed by the project path, under the
//! application's data directory. The first line is a magic word and a JSON
//! header — the file it belongs to, whether it is untitled, the caret — and
//! everything after it is the document's text. That layout means a header can
//! be read without loading a large document, which is what lets a broken or
//! half-written file be rejected rather than restored over good work.
//!
//! ## Why the write replaces by rename
//!
//! `write` builds the backup beside its destination and then renames it into
//! place. A process that dies mid-write leaves the previous backup intact
//! instead of a truncated file that looks like the newest one.
//!
//! ## What this deliberately does not do
//!
//! It does not decide *when* to write. A backup is a snapshot of text the panel
//! already has, handed here by whoever owns the timing — the panel, on a debounce
//! — so this module can be tested without a window or a clock.

use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

/// Prefix identifying a file as one of ours, plus its format version.
///
/// A version in the magic word means a future layout can be told apart from
/// this one instead of being misread as a corrupt backup.
const MAGIC: &str = "waku-backup 1 ";

/// How much of a file the header may occupy.
///
/// A header is a path and a few numbers. A first line longer than this is not a
/// backup, so the read stops rather than pulling a whole file into memory to
/// find a newline that is not there.
const HEADER_LIMIT: u64 = 64 << 10;

/// One document's unsaved text, ready to restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backup {
    /// The file the backup was taken from, project-relative as the panel names
    /// it. `None` when the document had no file yet.
    pub relative_path: Option<String>,
    /// The text as it stood when the backup was taken.
    pub text: String,
    /// The caret, as a byte offset into `text`.
    pub cursor: usize,
}

/// Where a project's backups live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backups {
    directory: PathBuf,
}

impl Backups {
    /// The backups of the documents of one project.
    ///
    /// Keyed by the project path itself rather than by a hash of it: the
    /// directory is on the user's own machine and a readable name makes a stale
    /// backup something they can find and delete, which a hex digest is not.
    pub fn for_project(root: &Path, project: &Path) -> Self {
        Self {
            directory: root.join("backups").join(directory_key(project)),
        }
    }

    /// The backups of one project, under the application's data directory.
    ///
    /// `None` when the platform names no data directory: a backup written
    /// somewhere the program cannot find again is worse than none, because it
    /// looks like the work is safe and it is not.
    pub fn for_project_in_data_directory(project: &Path) -> Option<Self> {
        let root = dirs::data_local_dir()?.join(waku_protocol::identity::DATA_DIRECTORY_NAME);
        Some(Self::for_project(&root, project))
    }

    /// The directory these backups live in.
    ///
    /// Public so the tests can plant an unreadable file beside a real backup and
    /// so a user can be shown where their unsaved work is kept.
    #[allow(
        dead_code,
        reason = "the tests plant files in it; nothing in the running panel names it yet"
    )]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The file a document's backup is written to.
    ///
    /// Named after the document rather than the moment: backing up the same file
    /// twice must replace its backup, not accumulate copies of it.
    pub fn file_for(&self, relative_path: Option<&str>) -> PathBuf {
        match relative_path {
            Some(path) => self
                .directory
                .join(format!("{:016x}", fnv(path.as_bytes()))),
            None => self.directory.join("untitled"),
        }
    }

    /// Write one backup, replacing any earlier one for the same document.
    ///
    /// The text goes to a temporary file first and is renamed into place, so a
    /// process that dies here leaves the previous backup readable rather than a
    /// truncated file.
    pub fn write(&self, backup: &Backup) -> io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let destination = self.file_for(backup.relative_path.as_deref());
        let temporary = destination.with_extension("tmp");
        let mut header = String::from(MAGIC);
        header.push_str(&encode_header(backup));
        header.push('\n');

        {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(header.as_bytes())?;
            file.write_all(backup.text.as_bytes())?;
            // Without this the rename can beat the bytes to the disk, which on a
            // power cut leaves a correctly named empty file.
            file.sync_all()?;
        }
        fs::rename(&temporary, &destination)
    }

    /// Delete a document's backup, as saving it does.
    pub fn remove(&self, relative_path: Option<&str>) -> io::Result<()> {
        match fs::remove_file(self.file_for(relative_path)) {
            // Already gone is the state the caller wanted.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// Every backup in this project, newest first.
    ///
    /// A file that cannot be read or does not carry the magic word is skipped
    /// rather than failing the whole restore: one corrupt backup must not make
    /// the others unrecoverable. Leftover `.tmp` files are deleted, since a
    /// writer that died mid-rename left them and they are never the newest
    /// truth.
    pub fn read_all(&self) -> Vec<Backup> {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        let mut backups: Vec<(PathBuf, Backup)> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter(|entry| {
                let temporary = entry.path().extension().is_some_and(|ext| ext == "tmp");
                if temporary {
                    let _ = fs::remove_file(entry.path());
                }
                !temporary
            })
            .filter_map(|entry| {
                let path = entry.path();
                let backup = read_backup(&path)?;
                Some((path, backup))
            })
            .collect();
        // Newest first: when a project has more backups than the panel will
        // reopen, the most recently touched work is the work to keep.
        backups.sort_by_key(|(path, _)| std::cmp::Reverse(modified(path)));
        backups.into_iter().map(|(_, backup)| backup).collect()
    }
}

/// A directory name standing for one project path.
///
/// The path's last component where there is one, so a user browsing the backup
/// directory recognises their project, made unique by a hash of the whole path.
/// Two projects named `app` in different places therefore get different
/// directories, and neither one is named something unreadable.
fn directory_key(project: &Path) -> String {
    let name = project
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_owned());
    // Anything a filesystem could object to becomes a dash; the hash alone is
    // what keeps the name unique, the readable part is for the user.
    let readable: String = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "{:016x}-{}",
        fnv(project.to_string_lossy().as_bytes()),
        readable
    )
}

/// The last time a backup file changed, for ordering.
///
/// A file whose metadata cannot be read sorts as the oldest, which only moves it
/// down the list: it is still restored, just later than the others.
fn modified(path: &Path) -> std::time::SystemTime {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// The header line, without the magic word or its newline.
///
/// Written as JSON for the same reason the rest of this program's state is: a
/// path can hold any character, and a delimiter that only some paths contain
/// would eventually meet one of them.
fn encode_header(backup: &Backup) -> String {
    let mut fields = vec![("cursor".to_owned(), backup.cursor.to_string())];
    if let Some(path) = &backup.relative_path {
        fields.push(("path".to_owned(), json_string(path)));
    }
    let object = fields
        .into_iter()
        .map(|(key, value)| format!("{}:{}", json_string(&key), value))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{object}}}")
}

/// Read one backup file, or `None` when it is not one of ours.
fn read_backup(path: &Path) -> Option<Backup> {
    let file = fs::File::open(path).ok()?;
    let mut reader = io::BufReader::new(file.take(HEADER_LIMIT));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).ok()?;
    let line = std::str::from_utf8(line.strip_suffix(b"\n")?).ok()?;
    let header = line.strip_prefix(MAGIC)?;
    let (relative_path, cursor) = decode_header(header)?;

    // The body starts where the header ended, so it is read from the file
    // rather than from the limited reader above: a backup can be any size.
    let mut text = String::new();
    let mut file = fs::File::open(path).ok()?;
    let body = header.len() as u64 + MAGIC.len() as u64 + 1;
    io::Seek::seek(&mut file, io::SeekFrom::Start(body)).ok()?;
    file.read_to_string(&mut text).ok()?;
    Some(Backup {
        relative_path,
        text,
        cursor,
    })
}

/// The path and caret from a header, or `None` if it cannot be understood.
///
/// Hand-written rather than pulled in from a JSON crate: the header is two
/// numbers and a string, and a backup reader that panics on a malformed file
/// would stop the program from starting for good.
fn decode_header(header: &str) -> Option<(Option<String>, usize)> {
    let header = header.trim();
    let header = header.strip_prefix('{')?.strip_suffix('}')?;
    let mut relative_path = None;
    let mut cursor = None;
    for field in split_fields(header) {
        let (key, value) = field.split_once(':')?;
        match unquote(key)?.as_str() {
            "path" => relative_path = Some(unquote(value)?),
            "cursor" => cursor = value.trim().parse().ok(),
            _ => {}
        }
    }
    Some((relative_path, cursor?))
}

/// Split a flat JSON object's body on the commas that are not inside a string.
fn split_fields(header: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in header.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            ',' if !in_string => {
                fields.push(&header[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    fields.push(&header[start..]);
    fields.retain(|field| !field.trim().is_empty());
    fields
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn unquote(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let digits: String = characters.by_ref().take(4).collect();
                let code = u32::from_str_radix(&digits, 16).ok()?;
                out.push(char::from_u32(code)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// FNV-1a, 64 bit.
///
/// The standard library's hasher is randomly seeded per process, so a backup
/// written by one run could not be found by the next. This one gives the same
/// answer everywhere, which is the whole requirement.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans itself up, so the tests write real files
    /// the way the program does instead of standing in a mock for the disk.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("waku-backup-test-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("a scratch directory");
            Self { root }
        }

        fn backups(&self, project: &str) -> Backups {
            Backups::for_project(&self.root, Path::new(project))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_backup_round_trips_its_text_path_and_caret() {
        let scratch = Scratch::new("round-trip");
        let backups = scratch.backups("/work/app");
        let backup = Backup {
            relative_path: Some("src/main.rs".to_owned()),
            text: "fn main() {\n    todo!()\n}\n".to_owned(),
            cursor: 12,
        };
        backups.write(&backup).expect("the write");

        let read = backups.read_all();
        assert_eq!(read.len(), 1, "one document, one backup");
        assert_eq!(read[0], backup, "everything it recorded comes back");
    }

    #[test]
    fn a_second_write_replaces_the_first_rather_than_piling_up() {
        let scratch = Scratch::new("replace");
        let backups = scratch.backups("/work/app");
        let path = Some("a.rs".to_owned());
        backups
            .write(&Backup {
                relative_path: path.clone(),
                text: "first".to_owned(),
                cursor: 0,
            })
            .expect("the first write");
        backups
            .write(&Backup {
                relative_path: path.clone(),
                text: "second".to_owned(),
                cursor: 3,
            })
            .expect("the second write");

        let read = backups.read_all();
        assert_eq!(read.len(), 1, "the newer text replaces the older");
        assert_eq!(read[0].text, "second");
    }

    /// Two projects can share a last path component. Keeping them apart is what
    /// stops one project's restore from writing into another's file.
    #[test]
    fn projects_named_alike_do_not_share_a_directory() {
        let scratch = Scratch::new("keys");
        let left = scratch.backups("/work/one/app");
        let right = scratch.backups("/work/two/app");
        assert_ne!(left.directory(), right.directory());
    }

    /// A backup whose header cannot be read must be skipped, not restored over
    /// good work and not allowed to fail the whole restore.
    #[test]
    fn an_unreadable_file_is_skipped_and_the_others_still_load() {
        let scratch = Scratch::new("corrupt");
        let backups = scratch.backups("/work/app");
        backups
            .write(&Backup {
                relative_path: Some("good.rs".to_owned()),
                text: "kept".to_owned(),
                cursor: 0,
            })
            .expect("the good write");
        fs::write(backups.directory().join("junk"), b"not a backup at all").expect("the junk");

        let read = backups.read_all();
        assert_eq!(read.len(), 1, "the corrupt file is skipped");
        assert_eq!(read[0].text, "kept");
    }

    /// The text is everything after the first newline, so a document whose
    /// first line looks like a header must still come back whole.
    #[test]
    fn a_document_that_starts_with_a_brace_is_not_mistaken_for_a_header() {
        let scratch = Scratch::new("brace");
        let backups = scratch.backups("/work/app");
        let text = "{\"path\":\"fake\"}\nreal body\n".to_owned();
        backups
            .write(&Backup {
                relative_path: Some("a.json".to_owned()),
                text: text.clone(),
                cursor: 0,
            })
            .expect("the write");

        let read = backups.read_all();
        assert_eq!(read[0].text, text, "the body is the body, not a header");
    }

    /// A path can hold any character a filesystem accepts, and the header is
    /// line-oriented: a newline in a path would otherwise end the header early.
    #[test]
    fn a_path_with_awkward_characters_round_trips() {
        let scratch = Scratch::new("chars");
        let backups = scratch.backups("/work/app");
        let relative_path = Some("odd \"quoted\" \\ path\\tab\there.rs".to_owned());
        backups
            .write(&Backup {
                relative_path: relative_path.clone(),
                text: "body".to_owned(),
                cursor: 2,
            })
            .expect("the write");

        let read = backups.read_all();
        assert_eq!(read[0].relative_path, relative_path);
    }

    #[test]
    fn removing_a_backup_leaves_nothing_to_restore() {
        let scratch = Scratch::new("remove");
        let backups = scratch.backups("/work/app");
        backups
            .write(&Backup {
                relative_path: Some("a.rs".to_owned()),
                text: "text".to_owned(),
                cursor: 0,
            })
            .expect("the write");
        backups
            .remove(Some("a.rs"))
            .expect("the removal is not an error");
        assert!(backups.read_all().is_empty());
        // Removing what is not there is the state the caller wanted, not a
        // failure: a save calls this whether or not a backup was ever written.
        backups.remove(Some("a.rs")).expect("still not an error");
    }

    /// A writer that died between creating the temporary file and renaming it
    /// leaves a `.tmp` behind. It is never the newest truth, so it must not be
    /// restored — and it is cleaned up rather than accumulating.
    #[test]
    fn a_leftover_temporary_file_is_deleted_and_never_restored() {
        let scratch = Scratch::new("tmp");
        let backups = scratch.backups("/work/app");
        backups
            .write(&Backup {
                relative_path: Some("a.rs".to_owned()),
                text: "complete".to_owned(),
                cursor: 0,
            })
            .expect("the write");
        let leftover = backups.file_for(Some("a.rs")).with_extension("tmp");
        fs::write(
            &leftover,
            format!("{MAGIC}{{\"path\":\"a.rs\",\"cursor\":0}}\nhalf"),
        )
        .expect("the leftover");
        let read = backups.read_all();
        assert_eq!(read.len(), 1, "the half-written file is not a backup");
        assert_eq!(read[0].text, "complete");
        assert!(!leftover.exists(), "the leftover is cleaned up");
    }

    #[test]
    fn the_header_is_read_without_loading_the_body() {
        let scratch = Scratch::new("header");
        let backups = scratch.backups("/work/app");
        let text = "x".repeat(200_000);
        backups
            .write(&Backup {
                relative_path: Some("big.rs".to_owned()),
                text: text.clone(),
                cursor: 7,
            })
            .expect("the write");

        let read = backups.read_all();
        assert_eq!(read[0].cursor, 7, "the header is where the caret was");
        assert_eq!(read[0].text.len(), text.len(), "and the body is whole");
    }
}
