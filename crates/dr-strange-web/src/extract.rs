//! Server-side document text extraction for the digest page (arch/08 + arch/07).
//! markdown/txt are decoded as UTF-8; PDF via `pdf-extract`; docx by unzipping
//! `word/document.xml` and stripping its tags. The extracted text is what the
//! digest pipeline sees — imperfect docx/pdf extraction is fine, the LLM
//! tolerates it.

use std::io::{Cursor, Read};

use anyhow::{Result, bail};
use pdf_extract::{
    Document, MediaBox, OutputDev, OutputError, PlainTextOutput, Transform, output_doc,
};

/// Extract plain text from a document, dispatching on the filename extension.
/// Reports progress as `(page, total)` for the PDF path (the only slow,
/// page-structured format); other formats extract in one shot and never call
/// `progress`. The callback drives the digest page's progress bar over the
/// streaming `/digest/extract` response.
pub fn extract_text_with_progress(
    name: &str,
    bytes: &[u8],
    progress: &mut dyn FnMut(u32, u32),
) -> Result<String> {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "txt" | "text" | "" => Ok(String::from_utf8_lossy(bytes).into_owned()),
        "pdf" => extract_pdf(bytes, progress),
        "docx" => extract_docx(bytes),
        other => bail!("unsupported file type '.{other}' — use md, txt, pdf, or docx"),
    }
}

/// Extracts a PDF page by page (via [`output_doc`]) so we can report progress —
/// `pdf-extract`'s one-shot `extract_text_from_mem` is opaque. `output_doc`
/// walks the pages in order, invoking [`ProgressOutput::begin_page`] for each.
fn extract_pdf(bytes: &[u8], progress: &mut dyn FnMut(u32, u32)) -> Result<String> {
    let doc = Document::load_mem(bytes).map_err(|e| anyhow::anyhow!("pdf: {e}"))?;
    let total = doc.get_pages().len() as u32;
    let mut text = String::new();
    {
        let mut out = ProgressOutput {
            inner: PlainTextOutput::new(&mut text),
            total,
            progress,
        };
        output_doc(&doc, &mut out).map_err(|e| anyhow::anyhow!("pdf: {e}"))?;
    }
    Ok(text)
}

/// An [`OutputDev`] that delegates all text accumulation to an inner
/// [`PlainTextOutput`] but fires `progress(page, total)` at each page boundary.
struct ProgressOutput<'a, 'p> {
    inner: PlainTextOutput<&'a mut String>,
    total: u32,
    progress: &'p mut dyn FnMut(u32, u32),
}

impl OutputDev for ProgressOutput<'_, '_> {
    fn begin_page(
        &mut self,
        page_num: u32,
        media_box: &MediaBox,
        art_box: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        (self.progress)(page_num, self.total);
        self.inner.begin_page(page_num, media_box, art_box)
    }
    fn end_page(&mut self) -> Result<(), OutputError> {
        self.inner.end_page()
    }
    fn output_character(
        &mut self,
        trm: &Transform,
        width: f64,
        spacing: f64,
        font_size: f64,
        ch: &str,
    ) -> Result<(), OutputError> {
        self.inner
            .output_character(trm, width, spacing, font_size, ch)
    }
    fn begin_word(&mut self) -> Result<(), OutputError> {
        self.inner.begin_word()
    }
    fn end_word(&mut self) -> Result<(), OutputError> {
        self.inner.end_word()
    }
    fn end_line(&mut self) -> Result<(), OutputError> {
        self.inner.end_line()
    }
}

fn extract_docx(bytes: &[u8]) -> Result<String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")?.read_to_string(&mut xml)?;
    // Paragraph breaks first, then strip tags, then decode basic entities.
    let xml = xml.replace("</w:p>", "\n");
    Ok(decode_entities(&strip_tags(&xml)))
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode the five predefined XML entities; `&amp;` last so it doesn't
/// double-decode (`&amp;lt;` → `&lt;`, not `<`).
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_text(name: &str, bytes: &[u8]) -> Result<String> {
        extract_text_with_progress(name, bytes, &mut |_, _| {})
    }

    #[test]
    fn plain_text_and_docx_xml_stripping() {
        assert_eq!(extract_text("a.md", b"# Hi\ntext").unwrap(), "# Hi\ntext");
        // The docx path's XML handling, exercised directly.
        let xml = "<w:p><w:r><w:t>Alice &amp; Bob</w:t></w:r></w:p><w:p><w:t>next</w:t></w:p>";
        let text = decode_entities(&strip_tags(&xml.replace("</w:p>", "\n")));
        assert_eq!(text, "Alice & Bob\nnext\n");
    }

    #[test]
    fn unknown_extension_errors() {
        assert!(extract_text("a.xyz", b"x").is_err());
    }
}
