use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{
    entities::{Application, Job, MatchResult, RefreshToken, Resume, Session, User},
    errors::DomainError,
};

/// Storage ports intentionally contain no NenDB-specific types. Implement these
/// ports with an official NenDB driver when one is verified, or with an HTTP
/// gateway that owns serialization and retries.
#[derive(Clone, Debug, Default)]
pub struct JobFilter {
    pub query: Option<String>,
    pub skills: Option<Vec<String>>,
    pub location: Option<String>,
}

#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn create(&self, user: User) -> Result<User, DomainError>;
    async fn create_with_session(
        &self,
        user: User,
        session: Session,
        refresh_token: RefreshToken,
    ) -> Result<User, DomainError>;
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, DomainError>;
    async fn find_by_id(&self, id: Uuid) -> Result<Option<User>, DomainError>;
    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<User>, DomainError>;
    async fn create_session(
        &self,
        session: Session,
        refresh_token: RefreshToken,
    ) -> Result<Session, DomainError>;
    async fn find_session(&self, id: Uuid) -> Result<Option<Session>, DomainError>;
    async fn find_refresh_token_by_verifier(
        &self,
        verifier: &str,
    ) -> Result<Option<RefreshToken>, DomainError>;
    async fn find_refresh_token(
        &self,
        verifier: &str,
    ) -> Result<Option<RefreshToken>, DomainError> {
        self.find_refresh_token_by_verifier(verifier).await
    }
    async fn rotate_refresh_token(
        &self,
        current_verifier: &str,
        replacement: RefreshToken,
        rotated_at: DateTime<Utc>,
    ) -> Result<Session, DomainError>;
    async fn revoke_session(
        &self,
        session_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
    async fn revoke_session_by_refresh_token(
        &self,
        verifier: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;
    async fn revoke_refresh_token(
        &self,
        verifier: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        self.revoke_session_by_refresh_token(verifier, revoked_at)
            .await
    }
    async fn revoke_all_sessions(
        &self,
        user_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<usize, DomainError>;
}

#[async_trait]
pub trait ResumeRepository: Send + Sync {
    async fn create(&self, resume: Resume) -> Result<Resume, DomainError>;
    async fn update(&self, resume: Resume) -> Result<Resume, DomainError>;
    async fn find_by_id(&self, id: Uuid) -> Result<Option<Resume>, DomainError>;
    async fn find_by_ids(&self, ids: &[Uuid]) -> Result<Vec<Resume>, DomainError>;
    async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<Resume>, DomainError>;
    async fn list_by_user_paginated(
        &self,
        user_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Resume>, DomainError> {
        let mut items = self.list_by_user(user_id).await?;
        items.sort_by_key(|resume| (std::cmp::Reverse(resume.created_at), resume.id));
        Ok(items.into_iter().skip(offset).take(limit).collect())
    }
    async fn delete(&self, id: Uuid) -> Result<(), DomainError>;
}

#[async_trait]
pub trait JobRepository: Send + Sync {
    async fn create(&self, job: Job) -> Result<Job, DomainError>;
    async fn find_by_id(&self, id: Uuid) -> Result<Option<Job>, DomainError>;
    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<Job>, DomainError>;
    async fn list_filtered(
        &self,
        offset: usize,
        limit: usize,
        filter: JobFilter,
    ) -> Result<Vec<Job>, DomainError> {
        let _ = filter;
        self.list(offset, limit).await
    }
}

#[async_trait]
pub trait ApplicationRepository: Send + Sync {
    async fn create(&self, application: Application) -> Result<Application, DomainError>;
    async fn find_by_job_and_resume(
        &self,
        job_id: Uuid,
        resume_id: Uuid,
    ) -> Result<Option<Application>, DomainError>;
    async fn list_by_job(
        &self,
        job_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Application>, DomainError>;
}

#[async_trait]
pub trait MatchResultRepository: Send + Sync {
    async fn create(&self, result: MatchResult) -> Result<MatchResult, DomainError>;
    async fn find_by_id(&self, id: Uuid) -> Result<Option<MatchResult>, DomainError>;
    async fn list_for_principal(
        &self,
        principal_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MatchResult>, DomainError>;
}
