use async_trait::async_trait;
use uuid::Uuid;

use super::{
    entities::{AccessTokenClaims, EducationLevel, Role},
    errors::DomainError,
};

#[async_trait]
pub trait ResumeParser: Send + Sync {
    async fn parse(&self, raw_text: &str) -> Result<ParsedResume, DomainError>;
}

#[async_trait]
pub trait JobParser: Send + Sync {
    async fn parse(&self, title: &str, description: &str) -> Result<ParsedJob, DomainError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedJob {
    pub role: Option<String>,
    pub skills: Vec<String>,
    pub minimum_experience_years: Option<u16>,
    pub minimum_education: Option<EducationLevel>,
    pub required_certifications: Vec<String>,
    pub keywords: Vec<String>,
    pub location: Option<String>,
    pub availability: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedResume {
    pub title: Option<String>,
    pub role: Option<String>,
    pub skills: Vec<String>,
    pub experience_years: Option<u16>,
    pub education: Option<EducationLevel>,
    pub certifications: Vec<String>,
    pub keywords: Vec<String>,
    pub location: Option<String>,
    pub availability: Option<String>,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, DomainError>;
}

pub trait SimilarityScorer: Send + Sync {
    fn similarity(&self, left: &[f32], right: &[f32]) -> Result<f32, DomainError>;
}

#[async_trait]
pub trait VirusScanner: Send + Sync {
    async fn scan(&self, bytes: &[u8]) -> Result<(), DomainError>;
}

#[async_trait]
pub trait DocumentTextExtractor: Send + Sync {
    async fn extract(&self, bytes: &[u8], mime_type: &str) -> Result<String, DomainError>;
}

pub trait InterviewQuestionGenerator: Send + Sync {
    fn generate(
        &self,
        job_title: &str,
        missing_skills: &[String],
        semantic_score: f32,
    ) -> Vec<String>;
}

pub trait CoverLetterGenerator: Send + Sync {
    fn generate(&self, resume_text: &str, job_title: &str, job_description: &str) -> String;
}

#[async_trait]
pub trait PasswordService: Send + Sync {
    async fn hash_password(&self, password: &str) -> Result<String, DomainError>;
    async fn verify_password(
        &self,
        password: &str,
        encoded_hash: &str,
    ) -> Result<bool, DomainError>;
}

pub trait TokenService: Send + Sync {
    fn issue_access_token(
        &self,
        user_id: Uuid,
        role: Role,
        session_id: Uuid,
    ) -> Result<(String, i64), DomainError>;

    fn decode_access_token(&self, token: &str) -> Result<AccessTokenClaims, DomainError>;

    fn generate_refresh_token(&self) -> Result<String, DomainError>;

    fn refresh_token_verifier(&self, token: &str) -> Result<String, DomainError>;

    fn refresh_token_ttl_seconds(&self) -> i64;

    fn issue(&self, user_id: Uuid) -> Result<(String, i64), DomainError> {
        self.issue_access_token(user_id, Role::default(), Uuid::now_v7())
    }

    fn decode_user_id(&self, token: &str) -> Result<Uuid, DomainError> {
        self.decode_access_token(token).map(|claims| claims.user_id)
    }

    fn refresh_token_digest(&self, token: &str) -> Result<String, DomainError> {
        self.refresh_token_verifier(token)
    }
}
