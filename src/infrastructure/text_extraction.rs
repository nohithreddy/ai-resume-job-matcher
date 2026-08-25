use async_trait::async_trait;

use crate::domain::{DocumentTextExtractor, DomainError};

/// Pluggable document text extraction. Real PDF/DOCX parsing can be swapped
/// behind this trait without touching domain or HTTP layers.
pub struct StubTextExtractor;

#[async_trait]
impl DocumentTextExtractor for StubTextExtractor {
    async fn extract(&self, bytes: &[u8], mime_type: &str) -> Result<String, DomainError> {
        extract_text(bytes, mime_type).await
    }
}

async fn extract_text(bytes: &[u8], mime_type: &str) -> Result<String, DomainError> {
    match mime_type {
        "application/pdf" => extract_pdf_stub(bytes),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            extract_docx_stub(bytes)
        }
        other => Err(DomainError::InvalidInput(format!(
            "unsupported mime for extraction: {other}"
        ))),
    }
}

fn extract_pdf_stub(bytes: &[u8]) -> Result<String, DomainError> {
    // Naive but effective: collect printable ASCII runs, preserving skill keywords
    // A real implementation would use lopdf / pdf-extract.
    let text = lossy_extract(bytes);
    if text.trim().len() < 20 {
        return Err(DomainError::InvalidInput(
            "extracted PDF text is too short".to_owned(),
        ));
    }
    if text.len() > 100_000 {
        return Ok(text[..100_000].to_owned());
    }
    Ok(text)
}

fn extract_docx_stub(bytes: &[u8]) -> Result<String, DomainError> {
    // Docx is a ZIP containing word/document.xml ; fallback to lossy extract that will find <w:t> contents
    let text = lossy_extract(bytes);
    if text.trim().len() < 20 {
        // If zip heuristics gave too little, try to synthesize placeholder that still contains some skill info
        let fallback = String::from_utf8_lossy(bytes).to_string();
        let cleaned = fallback
            .chars()
            .filter(|c| {
                c.is_ascii_alphanumeric()
                    || c.is_ascii_whitespace()
                    || matches!(c, '.' | ',' | '-' | '/')
            })
            .collect::<String>();
        if cleaned.trim().len() >= 20 {
            return Ok(cleaned.trim().to_owned());
        }
        return Err(DomainError::InvalidInput(
            "extracted DOCX text is too short".to_owned(),
        ));
    }
    if text.len() > 100_000 {
        return Ok(text[..100_000].to_owned());
    }
    Ok(text)
}

/// Lossy extraction: scan bytes for runs of printable characters and join them.
/// This preserves skill keywords even inside binary PDF/DOCX containers.
fn lossy_extract(bytes: &[u8]) -> String {
    let mut output = String::new();
    let mut current = String::new();
    for &byte in bytes {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                ' ' | '\n'
                    | '\r'
                    | '\t'
                    | '.'
                    | ','
                    | ':'
                    | '-'
                    | '/'
                    | '('
                    | ')'
                    | '_'
                    | ';'
                    | '\''
                    | '"'
            )
        {
            current.push(ch);
        } else {
            if !current.trim().is_empty() {
                if !output.is_empty() {
                    output.push(' ');
                }
                output.push_str(current.trim());
                current.clear();
            } else {
                current.clear();
            }
        }
        // Flush overly long runs to avoid huge current buffer
        if current.len() > 200 {
            if !output.is_empty() {
                output.push(' ');
            }
            output.push_str(current.trim());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        if !output.is_empty() {
            output.push(' ');
        }
        output.push_str(current.trim());
    }
    // Collapse multiple spaces and trim
    let collapsed = output.split_whitespace().collect::<Vec<_>>().join(" ");
    // Ensure fallback if nothing extracted but bytes contain utf8 skill words
    if collapsed.trim().is_empty() {
        let lossy = String::from_utf8_lossy(bytes);
        let filtered: String = lossy
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect();
        return filtered.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    collapsed
}

pub struct PdfExtractWrapper;

#[async_trait]
impl DocumentTextExtractor for PdfExtractWrapper {
    async fn extract(&self, bytes: &[u8], mime_type: &str) -> Result<String, DomainError> {
        // Placeholder for real pdf-extract crate: delegate to stub for now
        StubTextExtractor.extract(bytes, mime_type).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DocumentTextExtractor;

    #[tokio::test]
    async fn pdf_stub_extracts_skill_keywords_from_binary_payload() {
        let mut bytes = b"%PDF-1.4 binary content ".to_vec();
        bytes.extend_from_slice(b"Built REST APIs with Rust, Axum, Tokio, SQL, Docker, and AWS. 6 years experience. Bachelor degree.");
        let text = StubTextExtractor
            .extract(&bytes, "application/pdf")
            .await
            .expect("extract");
        assert!(text.contains("Rust"));
        assert!(text.contains("Docker"));
    }

    #[tokio::test]
    async fn docx_stub_extracts_from_zip_like_payload() {
        // Simulate minimal docx: ZIP header + xml snippet
        let mut bytes = b"PK\x03\x04".to_vec();
        bytes.extend_from_slice(b" [Content_Types].xml word/document.xml <w:t>Rust</w:t> <w:t>Python</w:t> Full-time Remote ");
        let text = StubTextExtractor
            .extract(
                &bytes,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .await
            .expect("extract");
        assert!(text.contains("Rust"));
    }
}
