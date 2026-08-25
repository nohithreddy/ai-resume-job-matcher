use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::domain::{DomainError, EmbeddingProvider};

/// Stable local embedding used for offline development and deterministic tests.
/// It is not intended to replace a semantic model in production.
#[derive(Clone)]
pub struct DeterministicEmbeddingProvider {
    dimensions: usize,
}

impl Default for DeterministicEmbeddingProvider {
    fn default() -> Self {
        // This is a local feature-hash vector, not a model-backed 384-dimensional
        // embedding. Keep the size explicit so dimension mismatches fail clearly.
        Self { dimensions: 64 }
    }
}

#[async_trait]
impl EmbeddingProvider for DeterministicEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, DomainError> {
        if self.dimensions == 0 {
            return Err(DomainError::InvalidEmbedding);
        }
        let mut vector = vec![0.0_f32; self.dimensions];
        for token in text
            .to_ascii_lowercase()
            .split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
        {
            let index = (stable_hash(token) as usize) % self.dimensions;
            vector[index] += 1.0;
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(DomainError::InvalidEmbedding);
        }
        Ok(vector)
    }
}

fn stable_hash(value: &str) -> u64 {
    value
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

pub(crate) fn expected_dimensions(model: &str) -> Option<usize> {
    match model {
        "all-MiniLM-L6-v2" => Some(384),
        "text-embedding-3-small" => Some(1536),
        _ => None,
    }
}

pub(crate) fn validate_embedding_dimensions(
    embedding: &[f32],
    model: &str,
) -> Result<(), DomainError> {
    if embedding.is_empty() || embedding.iter().any(|value| !value.is_finite()) {
        return Err(DomainError::InvalidEmbedding);
    }
    if expected_dimensions(model).is_some_and(|expected| embedding.len() != expected) {
        return Err(DomainError::InvalidEmbedding);
    }
    Ok(())
}

#[derive(Clone)]
pub struct HttpEmbeddingProvider {
    client: Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
}

impl HttpEmbeddingProvider {
    pub fn new(
        client: Client,
        endpoint: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client,
            endpoint: endpoint.into(),
            api_key,
            model: model.into(),
        }
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[async_trait]
impl EmbeddingProvider for HttpEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, DomainError> {
        let mut request = self.client.post(&self.endpoint).json(&EmbeddingRequest {
            model: &self.model,
            input: text,
        });
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }
        let response = request.send().await.map_err(|error| {
            DomainError::DependencyUnavailable(format!("embedding request failed: {error}"))
        })?;
        if !response.status().is_success() {
            return Err(DomainError::DependencyUnavailable(format!(
                "embedding provider returned {}",
                response.status()
            )));
        }
        let payload: EmbeddingResponse = response.json().await.map_err(|error| {
            DomainError::DependencyUnavailable(format!(
                "embedding provider returned invalid JSON: {error}"
            ))
        })?;
        let embedding = payload
            .data
            .into_iter()
            .next()
            .map(|data| data.embedding)
            .ok_or(DomainError::InvalidEmbedding)?;
        validate_embedding_dimensions(&embedding, &self.model)?;
        Ok(embedding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EmbeddingProvider;

    #[tokio::test]
    async fn deterministic_embeddings_are_stable_and_explicitly_sixty_four_dimensional() {
        let provider = DeterministicEmbeddingProvider::default();
        let first = provider.embed("Rust SQL").await;
        let second = provider.embed("Rust SQL").await;
        assert!(first.is_ok());
        assert!(second.is_ok());
        let first = first.unwrap_or_default();
        let second = second.unwrap_or_default();
        assert_eq!(first.len(), 64);
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn token_order_and_case_do_not_change_local_embedding() {
        let provider = DeterministicEmbeddingProvider::default();
        let first = provider.embed("Rust SQL").await;
        let second = provider.embed("rust sql").await;
        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(first.unwrap_or_default(), second.unwrap_or_default());
    }

    #[test]
    fn http_embedding_dimension_guard_rejects_mismatched_lengths() {
        let mismatch = vec![0.0_f32; 10];
        assert!(matches!(
            validate_embedding_dimensions(&mismatch, "all-MiniLM-L6-v2"),
            Err(crate::domain::DomainError::InvalidEmbedding)
        ));
        assert!(matches!(
            validate_embedding_dimensions(&mismatch, "text-embedding-3-small"),
            Err(crate::domain::DomainError::InvalidEmbedding)
        ));
        let correct_mini = vec![0.0_f32; 384];
        assert!(validate_embedding_dimensions(&correct_mini, "all-MiniLM-L6-v2").is_ok());
        let correct_small = vec![0.0_f32; 1536];
        assert!(validate_embedding_dimensions(&correct_small, "text-embedding-3-small").is_ok());
        // Unknown models accept any non-empty finite vector.
        assert!(validate_embedding_dimensions(&mismatch, "unknown-model").is_ok());
        assert!(validate_embedding_dimensions(&[], "unknown-model").is_err());
        assert!(matches!(
            validate_embedding_dimensions(&[f32::INFINITY], "text-embedding-3-small"),
            Err(crate::domain::DomainError::InvalidEmbedding)
        ));
    }

    #[tokio::test]
    async fn http_embedding_provider_rejects_wrong_dimension_via_mock_server() {
        use axum::{Json, Router, routing::post};
        use serde_json::json;
        use std::net::SocketAddr;

        async fn handler(Json(_body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            // Return a deliberately wrong-sized embedding for text-embedding-3-small (expect 1536)
            Json(json!({ "data": [{ "embedding": vec![0.0; 10] }] }))
        }

        let app = Router::new().route("/", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let provider = HttpEmbeddingProvider::new(
            client,
            format!("http://{addr}/"),
            None,
            "text-embedding-3-small",
        );
        let result = provider.embed("hello world").await;
        assert!(matches!(
            result,
            Err(crate::domain::DomainError::InvalidEmbedding)
        ));
    }
}
