//! Server-side document text extraction for the digest page (arch/08 + arch/07).
//! markdown/txt are decoded as UTF-8; PDF via `pdf-extract`; docx by unzipping
//! `word/document.xml` and stripping its tags. The extracted text is what the
//! digest pipeline sees — imperfect docx/pdf extraction is fine, the LLM
//! tolerates it.

use std::io::{Cursor, Read};

use anyhow::{Result, bail};

/// Extract plain text from a document, dispatching on the filename extension.
pub fn extract_text(name: &str, bytes: &[u8]) -> Result<String> {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "txt" | "text" | "" => Ok(String::from_utf8_lossy(bytes).into_owned()),
        "pdf" => pdf_extract::extract_text_from_mem(bytes).map_err(|e| anyhow::anyhow!("pdf: {e}")),
        "docx" => extract_docx(bytes),
        other => bail!("unsupported file type '.{other}' — use md, txt, pdf, or docx"),
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
