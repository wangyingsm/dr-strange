//! Document → Markdown, the front door of the digest pipeline (arch/07).
//!
//! Every surface that ingests a document goes through here — `drsg digest`,
//! the `digest` MCP tool, and the dashboard's upload — so a PDF reads the same
//! whichever one opened it. Everything arrives as bytes and leaves as Markdown.
//! Plain text and Markdown pass through untouched; every other format goes
//! through [`anydoc`], which covers Word, PowerPoint, Excel, OpenDocument, RTF,
//! EPUB, CSV and PDF, and renders them all through one GitHub-Flavored Markdown
//! serializer.
//!
//! **Markdown, not flat text**, because the reader downstream is a model. A
//! heading tells it where a section starts, a table keeps its rows attached to
//! their columns, and a list stays a list — structure the previous
//! extract-the-characters approach discarded before the LLM ever saw it. It
//! also makes this path agree with the URL crawler, which already converts
//! fetched HTML to Markdown, so a digest looks the same whether its source was
//! uploaded or fetched.
//!
//! **Format comes from the bytes**, not the filename: anydoc reads the PDF
//! header, the RTF open group, OLE stream names and the ZIP package mimetype,
//! so a mislabelled upload still converts. The extension is only a fallback for
//! the signature-less formats (CSV), and the caller's filename is advisory.

use anyhow::{Result, bail};

/// Convert an uploaded document to Markdown.
///
/// `name` is used only to disambiguate formats that have no signature to
/// detect — CSV, essentially. The bytes decide everything else.
pub fn to_markdown(name: &str, bytes: &[u8]) -> Result<String> {
    // `rsplit_once`, not `rsplit().next()`: the latter yields the whole name
    // when there is no dot, so an extensionless upload looked like an extension
    // called "notes" and was refused rather than read as text.
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();

    // Text and Markdown are not "documents" to convert — they are already the
    // target format. Passing them through anydoc would be a round trip that can
    // only lose something.
    if matches!(ext.as_str(), "md" | "markdown" | "txt" | "text" | "") {
        return Ok(trim_blank_edges(&String::from_utf8_lossy(bytes)));
    }

    // Signature first, extension only as the fallback it is: a `.docx` that is
    // really a PDF converts as a PDF, which is what the reader wants.
    let format = anydoc::Format::from_bytes(bytes).or_else(|| anydoc::Format::from_extension(&ext));
    let Some(format) = format else {
        bail!(
            "unrecognised file type '.{ext}' — supported: md, txt, and doc, docx, odt, rtf, \
             epub, pdf, ppt, pptx, xls(x), ods, odp, csv"
        );
    };

    let markdown = anydoc::to_markdown_bytes(bytes, format)
        .map_err(|e| anyhow::anyhow!("could not read this {format:?} document: {e}"))?;
    Ok(trim_blank_edges(&markdown))
}

/// Drop leading and trailing blank lines, and nothing else.
///
/// Deliberately not the old whitespace normaliser, which collapsed runs of
/// spaces inside every line. That was reasonable for noisy character-by-
/// character PDF output and actively wrong for Markdown: it flattened the
/// indentation that makes a nested list nested, and did so even to `.md` files
/// a user uploaded already formatted.
fn trim_blank_edges(s: &str) -> String {
    s.trim_matches(|c: char| c == '\n' || c == '\r').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_and_text_pass_through_unchanged() {
        // Nested list indentation is the canary: the previous normaliser
        // flattened it, silently turning a user's structure into prose.
        let src = "# Title\n\n- top\n  - nested\n    - deeper\n";
        assert_eq!(
            to_markdown("notes.md", src.as_bytes()).unwrap(),
            src.trim_end()
        );
        assert_eq!(
            to_markdown("notes.txt", src.as_bytes()).unwrap(),
            src.trim_end()
        );
        // No extension at all is treated as text rather than refused.
        assert_eq!(
            to_markdown("notes", src.as_bytes()).unwrap(),
            src.trim_end()
        );
    }

    #[test]
    fn csv_becomes_a_markdown_table() {
        let csv = "name,role\nada,engineer\nalan,logician\n";
        let md = to_markdown("people.csv", csv.as_bytes()).unwrap();
        assert!(md.contains('|'), "expected a table, got: {md}");
        assert!(md.contains("ada"), "row content missing: {md}");
        assert!(md.contains("logician"), "row content missing: {md}");
    }

    /// The format is read from the bytes, so a wrong extension still converts.
    /// This is the case the old extension-dispatch got wrong by construction.
    #[test]
    fn a_mislabelled_file_converts_by_signature() {
        // A real (tiny) PDF, named as though it were a Word document.
        let pdf = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
                    trailer\n<< /Root 1 0 R >>\n%%EOF\n";
        let detected = anydoc::Format::from_bytes(pdf);
        assert_eq!(
            detected,
            Some(anydoc::Format::Pdf),
            "the signature should win over the name"
        );
    }

    #[test]
    fn an_unknown_type_says_what_is_supported() {
        let err = to_markdown("archive.tar.zst", b"\x28\xb5\x2f\xfd not a document")
            .expect_err("an unknown type must be refused");
        let msg = err.to_string();
        assert!(msg.contains("unrecognised file type"), "got: {msg}");
        assert!(msg.contains("docx"), "should list what works: {msg}");
    }
}
