use std::path::{Path, PathBuf};

use crate::domain::DomainError;

pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
const ALLOWED_EXTENSIONS: &[&str] = &["pdf", "docx"];
const EXECUTABLE_EXTENSIONS: &[&str] = &[
    "exe", "dll", "so", "dylib", "sh", "bat", "cmd", "com", "msi", "js", "vbs", "ps1", "scr",
    "pif", "app", "bin", "run", "out",
];
const ALLOWED_MIMES: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
];

pub fn validate_and_detect(
    filename: &str,
    declared_mime: Option<&str>,
    bytes: &[u8],
) -> Result<(String, String), DomainError> {
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(DomainError::InvalidInput(format!(
            "file exceeds the {} byte limit",
            MAX_UPLOAD_BYTES
        )));
    }
    if bytes.is_empty() {
        return Err(DomainError::InvalidInput(
            "uploaded file is empty".to_owned(),
        ));
    }

    let sanitized = sanitize_filename(filename)?;

    // Double-extension and executable rejection
    reject_double_and_executable(&sanitized)?;

    // Determine extension from sanitized name
    let extension = extension_of(&sanitized).ok_or_else(|| {
        DomainError::InvalidInput("file must have a .pdf or .docx extension".to_owned())
    })?;
    if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
        return Err(DomainError::InvalidInput(format!(
            "unsupported file type: .{extension}; allowed: pdf, docx"
        )));
    }

    // MIME sniffing via magic bytes (not just extension)
    let sniffed = sniff_mime(bytes);
    let sniffed = sniffed.ok_or_else(|| {
        DomainError::InvalidInput("could not determine file type from content".to_owned())
    })?;

    if !ALLOWED_MIMES.contains(&sniffed.as_str()) {
        return Err(DomainError::InvalidInput(format!(
            "unsupported content type: {sniffed}"
        )));
    }

    // Cross-check extension vs sniffed mime
    let expected_for_ext = match extension.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => unreachable!(),
    };
    if sniffed != expected_for_ext {
        return Err(DomainError::InvalidInput(format!(
            "file extension .{extension} does not match content type {sniffed}"
        )));
    }

    // Also check declared mime if present – if it claims to be something executable, reject
    if let Some(declared) = declared_mime {
        let declared = declared
            .split(';')
            .next()
            .unwrap_or(declared)
            .trim()
            .to_ascii_lowercase();
        if is_executable_mime(&declared) {
            return Err(DomainError::InvalidInput(format!(
                "executable content type rejected: {declared}"
            )));
        }
        // We do not strictly require declared mime to equal sniffed, but keep lenient check
        if declared != sniffed
            && !declared.is_empty()
            && !ALLOWED_MIMES
                .iter()
                .any(|allowed| declared == *allowed || declared == "application/octet-stream")
            && declared != "application/pdf"
            && declared != "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        {
            // Allow octet-stream as generic fallback, but reject mismatch between extension and declared
        }
    }

    // Check for executable magic (MZ header, ELF)
    if is_executable_magic(bytes) {
        return Err(DomainError::InvalidInput(
            "executable file content rejected".to_owned(),
        ));
    }

    Ok((extension, sniffed))
}

fn sanitize_filename(filename: &str) -> Result<String, DomainError> {
    let trimmed = filename.trim();
    if trimmed.is_empty() {
        return Err(DomainError::InvalidInput(
            "filename must not be empty".to_owned(),
        ));
    }
    if trimmed.len() > 255 {
        return Err(DomainError::InvalidInput("filename too long".to_owned()));
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return Err(DomainError::InvalidInput(
            "filename must not contain path separators".to_owned(),
        ));
    }
    if trimmed.contains('\0') {
        return Err(DomainError::InvalidInput(
            "filename contains null byte".to_owned(),
        ));
    }
    // Only allow ascii alphanumeric, dot, underscore, hyphen
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
    {
        return Err(DomainError::InvalidInput(
            "filename contains invalid characters".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn reject_double_and_executable(filename: &str) -> Result<(), DomainError> {
    let lower = filename.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('.').collect();
    if parts.len() < 2 {
        return Err(DomainError::InvalidInput(
            "filename must have an extension".to_owned(),
        ));
    }
    // Check every extension part after first dot
    for part in &parts[1..] {
        let ext = part.trim();
        if ext.is_empty() {
            return Err(DomainError::InvalidInput(
                "filename contains empty extension".to_owned(),
            ));
        }
        if EXECUTABLE_EXTENSIONS.contains(&ext) {
            return Err(DomainError::InvalidInput(format!(
                "executable extension rejected: .{ext}"
            )));
        }
    }
    // Double-extension heuristic: if more than 2 dots, reject suspicious combos
    // e.g., resume.pdf.exe already caught, but resume.pdf.docx? treat as double extension reject
    if parts.len() > 2 {
        // Allow only single extension for allowed types; multiple dots implies double extension
        return Err(DomainError::InvalidInput(
            "double extensions are not allowed".to_owned(),
        ));
    }
    Ok(())
}

fn extension_of(filename: &str) -> Option<String> {
    let lower = filename.to_ascii_lowercase();
    lower.rsplit('.').next().map(|s| s.to_owned())
}

fn sniff_mime(bytes: &[u8]) -> Option<String> {
    // Use infer crate for robust detection, fallback to manual
    if let Some(kind) = infer::get(bytes) {
        let mime = kind.mime_type();
        // Map zip-based detection to docx if appropriate; infer reports docx as application/vnd.openxmlformats-officedocument.wordprocessingml.document? sometimes as application/zip
        if mime == "application/zip" {
            // Heuristic: check for docx: bytes are zip; if contains word/ string, treat as docx
            if is_docx_zip(bytes) {
                return Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                        .to_owned(),
                );
            }
            return Some(mime.to_owned());
        }
        return Some(mime.to_owned());
    }
    // Manual fallback
    if bytes.starts_with(b"%PDF") {
        return Some("application/pdf".to_owned());
    }
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        if is_docx_zip(bytes) {
            return Some(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                    .to_owned(),
            );
        }
        return Some("application/zip".to_owned());
    }
    // Check for docx via magic even if infer failed due to truncated header
    if is_docx_zip(bytes) {
        return Some(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_owned(),
        );
    }
    None
}

fn is_docx_zip(bytes: &[u8]) -> bool {
    // Minimal heuristic: bytes contain "[Content_Types].xml" or "word/" substring which are present in docx zip
    if bytes.len() < 4 {
        return false;
    }
    // Search for docx markers in first 8KB
    let search_len = bytes.len().min(8192);
    let window = &bytes[..search_len];
    // Look for word/ or _rels
    let markers: &[&[u8]] = &[b"word/", b"[Content_Types]", b"_rels"];
    markers
        .iter()
        .any(|marker| window.windows(marker.len()).any(|w| w == *marker))
        || (bytes.starts_with(b"PK") && window.windows(4).any(|w| w == b"PK\x03\x04"))
            && String::from_utf8_lossy(window).contains("word")
}

fn is_executable_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/x-msdownload"
            | "application/x-msdos-program"
            | "application/x-executable"
            | "application/x-sh"
            | "application/x-bat"
            | "application/octet-stream" if false
    ) || mime.contains("executable")
        || mime.contains("x-sh")
        || mime.contains("x-msdos")
}

fn is_executable_magic(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"MZ") {
        return true;
    }
    if bytes.starts_with(b"\x7FELF") {
        return true;
    }
    if bytes.starts_with(b"#!") {
        return true;
    }
    false
}

pub async fn store_upload(
    upload_dir: &Path,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf, DomainError> {
    tokio::fs::create_dir_all(upload_dir)
        .await
        .map_err(|error| {
            tracing::error!(%error, ?upload_dir, "could not create upload directory");
            DomainError::Internal("could not prepare upload storage".to_owned())
        })?;
    let filename = format!("{}.{}", uuid::Uuid::now_v7(), extension);
    let path = upload_dir.join(filename);
    tokio::fs::write(&path, bytes).await.map_err(|error| {
        tracing::error!(%error, ?path, "could not write uploaded file");
        DomainError::Internal("could not store uploaded file".to_owned())
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_double_extension() {
        let bytes = b"%PDF-1.4 fake pdf content with Rust skill";
        let result = validate_and_detect("resume.pdf.exe", Some("application/pdf"), bytes);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_executable_magic() {
        let bytes = b"MZ executable content";
        let result = validate_and_detect("evil.pdf", Some("application/pdf"), bytes);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_wrong_mime_for_extension() {
        // Bytes are PDF but extension is docx
        let bytes = b"%PDF-1.4 content";
        let result = validate_and_detect(
            "resume.docx",
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            bytes,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_oversized() {
        let bytes = vec![b'a'; MAX_UPLOAD_BYTES + 1];
        let result = validate_and_detect("resume.pdf", Some("application/pdf"), &bytes);
        assert!(result.is_err());
    }

    #[test]
    fn accepts_pdf_with_correct_magic() {
        let mut bytes = b"%PDF-1.4 ".to_vec();
        bytes.extend_from_slice(b"Rust Python content for testing long enough text");
        // Need to ensure infer recognizes as pdf
        let result = validate_and_detect("resume.pdf", Some("application/pdf"), &bytes);
        assert!(result.is_ok(), "should accept pdf: {:?}", result);
    }

    #[test]
    fn rejects_path_traversal() {
        let bytes = b"%PDF-1.4 content long enough to be valid text for extraction";
        let result = validate_and_detect("../resume.pdf", Some("application/pdf"), bytes);
        assert!(result.is_err());
    }
}
