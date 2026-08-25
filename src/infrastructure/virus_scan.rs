use async_trait::async_trait;

use crate::domain::{DomainError, VirusScanner};

pub struct NoopScanner;

#[async_trait]
impl VirusScanner for NoopScanner {
    async fn scan(&self, _bytes: &[u8]) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Placeholder for ClamAV integration. In production this would connect to
/// clamd via TCP/Unix socket and stream the bytes for scanning.
/// For now it checks for the EICAR test string and delegates to Noop.
pub struct ClamAvScanner {
    // clamd_address: Option<String>,
}

impl ClamAvScanner {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for ClamAvScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirusScanner for ClamAvScanner {
    async fn scan(&self, bytes: &[u8]) -> Result<(), DomainError> {
        // EICAR test string detection – used to verify scanner wiring without a real daemon
        const EICAR: &[u8] =
            b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";
        if bytes.windows(EICAR.len()).any(|window| window == EICAR) {
            return Err(DomainError::InvalidInput(
                "file rejected: virus detected (EICAR test)".to_owned(),
            ));
        }
        // In real implementation: connect to clamd, send INSTREAM, check response
        // For placeholder, treat as clean
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::VirusScanner;

    #[tokio::test]
    async fn noop_always_passes() {
        let scanner = NoopScanner;
        assert!(scanner.scan(b"clean content").await.is_ok());
    }

    #[tokio::test]
    async fn clamav_placeholder_detects_eicar() {
        let scanner = ClamAvScanner::new();
        let eicar = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";
        assert!(scanner.scan(eicar).await.is_err());
        assert!(scanner.scan(b"clean").await.is_ok());
    }
}
