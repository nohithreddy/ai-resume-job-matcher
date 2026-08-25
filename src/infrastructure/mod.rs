pub mod cover_letter;
mod embedding;
pub mod idempotency;
pub mod interview;
mod memory;
mod parsing;
mod security;
pub mod sqlite;
pub mod text_extraction;
pub mod upload;
pub mod virus_scan;

use std::path::Path;
use std::sync::Arc;

use crate::config::{AppConfig, PersistenceBackend};
use crate::domain::repositories::{
    ApplicationRepository, JobRepository, MatchResultRepository, ResumeRepository, UserRepository,
};

pub use cover_letter::TemplateCoverLetterGenerator;
pub use embedding::{DeterministicEmbeddingProvider, HttpEmbeddingProvider};
pub use idempotency::IdempotencyStore;
pub use interview::DeterministicInterviewGenerator;
pub use memory::InMemoryRepositories;
pub use parsing::{DeterministicJobParser, DeterministicResumeParser};
pub use security::{PasswordHasher, SecurityService};
pub use sqlite::SqliteRepositories;
pub use text_extraction::StubTextExtractor;
pub use virus_scan::{ClamAvScanner, NoopScanner};

/// The set of repository ports the application services depend on, resolved at
/// composition time so domain and HTTP code never learn which backend is live.
#[derive(Clone)]
pub struct RepositorySet {
    pub users: Arc<dyn UserRepository>,
    pub resumes: Arc<dyn ResumeRepository>,
    pub jobs: Arc<dyn JobRepository>,
    pub applications: Arc<dyn ApplicationRepository>,
    pub matches: Arc<dyn MatchResultRepository>,
}

impl RepositorySet {
    pub fn in_memory() -> Self {
        let repositories = InMemoryRepositories::new();
        Self {
            users: repositories.users,
            resumes: repositories.resumes,
            jobs: repositories.jobs,
            applications: repositories.applications,
            matches: repositories.matches,
        }
    }

    pub fn sqlite(path: &Path) -> Result<Self, crate::domain::DomainError> {
        let repositories = SqliteRepositories::open(path)?;
        Ok(Self {
            users: Arc::new(repositories.clone()),
            resumes: Arc::new(repositories.clone()),
            jobs: Arc::new(repositories.clone()),
            applications: Arc::new(repositories.clone()),
            matches: Arc::new(repositories),
        })
    }
}

pub fn open_repositories(config: &AppConfig) -> Result<RepositorySet, crate::domain::DomainError> {
    match config.persistence {
        PersistenceBackend::Memory => Ok(RepositorySet::in_memory()),
        PersistenceBackend::Sqlite => RepositorySet::sqlite(&config.database_path),
    }
}

impl RepositorySet {
    /// Human-readable backend name for readiness reporting.
    pub fn label(config: &AppConfig) -> &'static str {
        match config.persistence {
            PersistenceBackend::Memory => "in-memory",
            PersistenceBackend::Sqlite => "sqlite",
        }
    }
}
