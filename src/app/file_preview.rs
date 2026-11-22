//! What a file looks like when it is not source code to be edited.
//!
//! Waku's right panel opens a file into an editor, which is right for text and
//! wrong for everything else: a PNG is not a string, a workbook is a grid, and a
//! `.docx` is a zip of XML. Those formats need to be decoded and then drawn, and
//! the point of this module is to decide *which* of them a path is and to turn
//! the daemon's bytes into something a view can paint.
//!
//! ## Why the work is here and not in the view
//!
//! Decoding is CPU-bound and can take hundreds of milliseconds on a large image
//! or a long document. Keeping it out of the render path means the panel never
//! learns whether a file is slow to decode, and this module can be tested on
//! bytes without a window.
//!
//! ## What it leans on
//!
//! Winc's `winc-preview` crate does the parsing — PNG/JPEG/GIF/WebP decoding,
//! SVG rasterising, Markdown, xlsx/csv tables, and the docx/epub/LaTeX
//! converters. It is already linked through `winc-editor`, and its output types
//! (`Picture`, `Table`, `Markdown`) are formats rather than drawings, so they
//! fit a GPUI view without dragging in Winc's renderer.
//!
//! ## What it deliberately does not do
//!
//! It does not read the filesystem. Bytes arrive from the daemon like every
//! other file read in this program, so a preview cannot see a path the workspace
//! cannot.

use std::path::Path;

/// What the panel should draw for a file.
///
/// A closed set rather than "try each decoder": the panel has to know before it
/// reads anything whether a file is previewable, because reading a 200 MB video
/// only to discover nothing can display it is worse than not reading it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    /// A bitmap or vector image, drawn as one picture.
    Image,
    /// A spreadsheet or delimited text file, drawn as a grid.
    Table,
    /// A document converted to Markdown and rendered as prose.
    Document,
}

/// The kinds of file this build can preview, in the order the checks run.
///
/// An extension list rather than sniffing the bytes: the panel has the path
/// before it has the contents, and a `.png` that turned out to be something else
/// is a corrupt file the decoder will reject anyway.
const IMAGE: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "cur", "avif", "tif", "tiff",
];
const VECTOR: &[&str] = &["svg"];
const TABLE: &[&str] = &["xlsx", "xlsm", "csv", "tsv"];
const DOCUMENT: &[&str] = &["docx", "docm", "dotx", "dotm", "epub", "tex", "latex"];

/// How `path` should be previewed, or `None` when it should be edited as text.
///
/// Markdown is deliberately absent: it is already editable source with a
/// preview toggle of its own, and routing it here would take away the ability to
/// edit it.
pub fn kind_of(path: &str) -> Option<PreviewKind> {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();
    let extension = extension.as_str();
    if VECTOR.contains(&extension) {
        // An SVG is a picture, but it is also text people edit. The editor wins:
        // a preview of a file the user is about to change is not worth taking
        // the change away for.
        return None;
    }
    if IMAGE.contains(&extension) {
        return Some(PreviewKind::Image);
    }
    if TABLE.contains(&extension) {
        return Some(PreviewKind::Table);
    }
    if DOCUMENT.contains(&extension) {
        return Some(PreviewKind::Document);
    }
    None
}

/// Where a file's bytes become something drawable.
///
/// Each arm owns its decoding so a failure is reported against the format that
/// failed rather than as one vague "could not preview".
pub fn decode(kind: PreviewKind, path: &str, bytes: &[u8]) -> Result<Decoded, String> {
    match kind {
        PreviewKind::Image => decode_image(path, bytes),
        PreviewKind::Table => decode_table(path, bytes),
        PreviewKind::Document => decode_document(path, bytes),
    }
}

/// One decoded file. The variants mirror [`PreviewKind`], so a caller cannot
/// receive a table where it expected a picture.
#[derive(Clone, Debug)]
pub enum Decoded {
    /// The file's own bytes, with the format GPUI needs to decode them.
    ///
    /// Kept encoded rather than as raw RGBA on purpose: GPUI decodes images on
    /// the GPU, so handing it the original bytes costs nothing extra, while
    /// converting to RGBA here and back would mean encoding a PNG only for GPUI
    /// to decode that PNG again.
    Picture {
        format: gpui::ImageFormat,
        bytes: Vec<u8>,
    },
    /// A grid of cell text. Rows are already strings because a preview shows
    /// what the cell looks like, not what it evaluates to.
    ///
    /// No sheet name: the panel shows one sheet per file and names them by
    /// file, so a field here would be carried, cloned and never read. A
    /// multi-sheet workbook grows one when the view can switch between them.
    Grid {
        rows: Vec<Vec<String>>,
        truncated: bool,
    },
    /// Markdown text, for the renderer the transcript already has.
    Markdown { markdown: String },
}

fn decode_image(path: &str, bytes: &[u8]) -> Result<Decoded, String> {
    // The bytes are validated here so a corrupt file is reported as such when it
    // is opened, rather than as an empty pane when GPUI later fails to decode
    // it. What is handed on is still the original file.
    let picture =
        winc_preview::image::load_bytes(bytes).map_err(|error| format!("{path}: {error}"))?;
    let format = match picture.source {
        winc_preview::image::Source::Raster(_) => image_format(path)
            .ok_or_else(|| format!("{path}: this build cannot draw that image format"))?,
        winc_preview::image::Source::Vector(_) => {
            return Err(format!("{path}: an SVG is edited as source, not previewed"));
        }
    };
    Ok(Decoded::Picture {
        format,
        bytes: bytes.to_vec(),
    })
}

/// The GPUI decoder for an image path, from its extension.
fn image_format(path: &str) -> Option<gpui::ImageFormat> {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some(gpui::ImageFormat::Png),
        "jpg" | "jpeg" => Some(gpui::ImageFormat::Jpeg),
        "gif" => Some(gpui::ImageFormat::Gif),
        "webp" => Some(gpui::ImageFormat::Webp),
        "bmp" => Some(gpui::ImageFormat::Bmp),
        "tif" | "tiff" => Some(gpui::ImageFormat::Tiff),
        "ico" => Some(gpui::ImageFormat::Ico),
        _ => None,
    }
}

fn decode_table(path: &str, bytes: &[u8]) -> Result<Decoded, String> {
    // A workbook is a zip; a delimited file is text. The two are decided by the
    // extension the panel already resolved, so a `.csv` never goes to the zip
    // reader and a `.xlsx` never goes to the text one.
    let sheet = if winc_preview::table::is_workbook(Path::new(path)) {
        let table = winc_preview::table::read_workbook(bytes)
            .map_err(|error| format!("{path}: {error}"))?;
        table.sheets.into_iter().next()
    } else {
        let text = String::from_utf8_lossy(bytes);
        let tabs = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("tsv"));
        Some(winc_preview::table::read_delimited(&text, tabs))
    };
    let Some(sheet) = sheet else {
        return Ok(Decoded::Grid {
            rows: Vec::new(),
            truncated: false,
        });
    };
    let rows = (0..sheet.row_count())
        .map(|row| sheet.row(row).map(str::to_owned).collect::<Vec<_>>())
        .collect();
    Ok(Decoded::Grid {
        rows,
        truncated: sheet.truncated,
    })
}

fn decode_document(path: &str, bytes: &[u8]) -> Result<Decoded, String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let markdown = match extension.as_str() {
        "docx" | "docm" | "dotx" | "dotm" => {
            winc_preview::docx::convert(bytes)
                .map_err(|error| format!("{path}: {error}"))?
                .markdown
        }
        "epub" => {
            winc_preview::epub::convert(bytes)
                .map_err(|error| format!("{path}: {error}"))?
                .markdown
        }
        "tex" | "latex" => {
            // LaTeX arrives as text, not as a compiled document: the converter
            // reads the source, so a lossy decode is the right one here.
            let source = String::from_utf8_lossy(bytes);
            winc_preview::latex::to_markdown(&source).markdown
        }
        other => return Err(format!("{path}: no preview for .{other}")),
    };
    Ok(Decoded::Markdown { markdown })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_files_are_edited_rather_than_previewed() {
        for path in ["src/main.rs", "Cargo.toml", "README.md", "script.sh"] {
            assert_eq!(kind_of(path), None, "{path} should open in the editor");
        }
    }

    #[test]
    fn an_svg_stays_editable_because_it_is_also_source() {
        assert_eq!(kind_of("logo.svg"), None);
    }

    #[test]
    fn each_format_maps_to_the_view_that_can_show_it() {
        assert_eq!(kind_of("shot.PNG"), Some(PreviewKind::Image));
        assert_eq!(kind_of("book.xlsx"), Some(PreviewKind::Table));
        assert_eq!(kind_of("data.csv"), Some(PreviewKind::Table));
        assert_eq!(kind_of("paper.docx"), Some(PreviewKind::Document));
        assert_eq!(kind_of("novel.epub"), Some(PreviewKind::Document));
    }

    #[test]
    fn a_path_without_an_extension_is_not_previewable() {
        assert_eq!(kind_of("Makefile"), None);
        assert_eq!(kind_of(""), None);
    }

    /// A delimited file decodes into a grid whose cells are what the file says,
    /// which is the whole contract of the table view.
    #[test]
    fn a_csv_decodes_into_its_cells() {
        let bytes = b"name,size\nalpha,1\nbeta,2\n";
        let Decoded::Grid { rows, .. } = decode(PreviewKind::Table, "a.csv", bytes).unwrap() else {
            panic!("a csv is a grid");
        };
        assert_eq!(rows[0], ["name", "size"]);
        assert_eq!(rows[2], ["beta", "2"]);
    }

    /// A corrupt image has to come back as an error naming the file, not as a
    /// panic inside the decoder.
    #[test]
    fn a_broken_image_reports_the_file_it_could_not_read() {
        let error = decode(PreviewKind::Image, "broken.png", b"not a png").unwrap_err();
        assert!(
            error.contains("broken.png"),
            "the error names the file: {error}"
        );
    }
}
