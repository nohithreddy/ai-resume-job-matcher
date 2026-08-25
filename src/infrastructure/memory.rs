use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::{
    entities::{Application, Job, MatchResult, RefreshToken, Resume, Session, User},
    errors::DomainError,
    repositories::{
        ApplicationRepository, JobRepository, MatchResultRepository, ResumeRepository,
        UserRepository,
    },
};

#[derive(Clone)]
pub struct InMemoryRepositories {
    pub users: Arc<InMemoryUserRepository>,
    pub resumes: Arc<InMemoryResumeRepository>,
    pub jobs: Arc<InMemoryJobRepository>,
    pub applications: Arc<InMemoryApplicationRepository>,
    pub matches: Arc<InMemoryMatchResultRepository>,
}

impl InMemoryRepositories {
    pub fn new() -> Self {
        Self {
            users: Arc::new(InMemoryUserRepository::default()),
            resumes: Arc::new(InMemoryResumeRepository::default()),
            jobs: Arc::new(InMemoryJobRepository::default()),
            applications: Arc::new(InMemoryApplicationRepository::default()),
            matches: Arc::new(InMemoryMatchResultRepository::default()),
        }
    }
}

impl Default for InMemoryRepositories {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct AuthState {
    users: HashMap<Uuid, User>,
    sessions: HashMap<Uuid, Session>,
    refresh_tokens: HashMap<String, RefreshToken>,
}

#[derive(Default)]
pub struct InMemoryUserRepository {
    state: RwLock<AuthState>,
}

#[async_trait]
impl UserRepository for InMemoryUserRepository {
    async fn create(&self, user: User) -> Result<User, DomainError> {
        let mut state = self.state.write().await;
        if state.users.contains_key(&user.id)
            || state
                .users
                .values()
                .any(|existing| existing.email == user.email)
        {
            return Err(DomainError::Conflict);
        }
        state.users.insert(user.id, user.clone());
        Ok(user)
    }

    async fn create_with_session(
        &self,
        user: User,
        session: Session,
        refresh_token: RefreshToken,
    ) -> Result<User, DomainError> {
        validate_session_token(&session, &refresh_token)?;
        if session.user_id != user.id {
            return Err(DomainError::InvalidInput(
                "session user does not match the new user".to_owned(),
            ));
        }

        let mut state = self.state.write().await;
        if state.users.contains_key(&user.id)
            || state
                .users
                .values()
                .any(|existing| existing.email == user.email)
            || state.sessions.contains_key(&session.id)
            || state.refresh_tokens.contains_key(&refresh_token.verifier)
            || state
                .refresh_tokens
                .values()
                .any(|token| token.id == refresh_token.id)
        {
            return Err(DomainError::Conflict);
        }
        state.users.insert(user.id, user.clone());
        state.sessions.insert(session.id, session);
        state
            .refresh_tokens
            .insert(refresh_token.verifier.clone(), refresh_token);
        Ok(user)
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<User>, DomainError> {
        Ok(self
            .state
            .read()
            .await
            .users
            .values()
            .find(|user| user.email == email)
            .cloned())
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<User>, DomainError> {
        Ok(self.state.read().await.users.get(&id).cloned())
    }

    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<User>, DomainError> {
        let mut users = self
            .state
            .read()
            .await
            .users
            .values()
            .cloned()
            .collect::<Vec<_>>();
        users.sort_by_key(|user| (user.created_at, user.id));
        Ok(users.into_iter().skip(offset).take(limit).collect())
    }

    async fn create_session(
        &self,
        session: Session,
        refresh_token: RefreshToken,
    ) -> Result<Session, DomainError> {
        validate_session_token(&session, &refresh_token)?;

        let mut state = self.state.write().await;
        if !state.users.contains_key(&session.user_id) {
            return Err(DomainError::NotFound);
        }
        if state.sessions.contains_key(&session.id)
            || state.refresh_tokens.contains_key(&refresh_token.verifier)
            || state
                .refresh_tokens
                .values()
                .any(|token| token.id == refresh_token.id)
        {
            return Err(DomainError::Conflict);
        }
        state.sessions.insert(session.id, session.clone());
        state
            .refresh_tokens
            .insert(refresh_token.verifier.clone(), refresh_token);
        Ok(session)
    }

    async fn find_session(&self, id: Uuid) -> Result<Option<Session>, DomainError> {
        Ok(self.state.read().await.sessions.get(&id).cloned())
    }

    async fn find_refresh_token_by_verifier(
        &self,
        verifier: &str,
    ) -> Result<Option<RefreshToken>, DomainError> {
        Ok(self
            .state
            .read()
            .await
            .refresh_tokens
            .get(verifier)
            .cloned())
    }

    async fn rotate_refresh_token(
        &self,
        current_verifier: &str,
        replacement: RefreshToken,
        rotated_at: DateTime<Utc>,
    ) -> Result<Session, DomainError> {
        let mut state = self.state.write().await;
        let current = state
            .refresh_tokens
            .get(current_verifier)
            .cloned()
            .ok_or(DomainError::Unauthorized)?;
        let session = state
            .sessions
            .get(&current.session_id)
            .cloned()
            .ok_or(DomainError::Unauthorized)?;

        let reused = current.used_at.is_some()
            || current.replaced_by.is_some()
            || session.current_refresh_token_id != current.id;
        if reused {
            revoke_session_state(&mut state, current.session_id, rotated_at);
            return Err(DomainError::Unauthorized);
        }
        if current.revoked_at.is_some()
            || current.issued_at > rotated_at
            || current.expires_at <= rotated_at
            || !session.is_active_at(rotated_at)
        {
            return Err(DomainError::Unauthorized);
        }
        validate_refresh_token(&replacement)?;
        if replacement.session_id != session.id
            || replacement.issued_at < current.issued_at
            || replacement.issued_at > rotated_at
            || replacement.expires_at <= rotated_at
            || replacement.expires_at > session.expires_at
            || state.refresh_tokens.contains_key(&replacement.verifier)
            || state
                .refresh_tokens
                .values()
                .any(|token| token.id == replacement.id)
        {
            return Err(DomainError::InvalidInput(
                "invalid refresh token replacement".to_owned(),
            ));
        }

        let session_id = current.session_id;
        {
            let current = state
                .refresh_tokens
                .get_mut(current_verifier)
                .ok_or_else(|| DomainError::Internal("refresh token disappeared".to_owned()))?;
            current.used_at = Some(rotated_at);
            current.replaced_by = Some(replacement.id);
        }

        let session = state
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| DomainError::Internal("session disappeared".to_owned()))?;
        session.current_refresh_token_id = replacement.id;
        session.last_rotated_at = rotated_at;
        let session = session.clone();
        state
            .refresh_tokens
            .insert(replacement.verifier.clone(), replacement);
        Ok(session)
    }

    async fn revoke_session(
        &self,
        session_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut state = self.state.write().await;
        if !state.sessions.contains_key(&session_id) {
            return Err(DomainError::Unauthorized);
        }
        revoke_session_state(&mut state, session_id, revoked_at);
        Ok(())
    }

    async fn revoke_session_by_refresh_token(
        &self,
        verifier: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut state = self.state.write().await;
        let session_id = state
            .refresh_tokens
            .get(verifier)
            .map(|token| token.session_id)
            .ok_or(DomainError::Unauthorized)?;
        revoke_session_state(&mut state, session_id, revoked_at);
        Ok(())
    }

    async fn revoke_all_sessions(
        &self,
        user_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<usize, DomainError> {
        let mut state = self.state.write().await;
        if !state.users.contains_key(&user_id) {
            return Err(DomainError::Unauthorized);
        }
        let session_ids = state
            .sessions
            .values()
            .filter(|session| session.user_id == user_id && session.revoked_at.is_none())
            .map(|session| session.id)
            .collect::<Vec<_>>();
        for session_id in &session_ids {
            revoke_session_state(&mut state, *session_id, revoked_at);
        }
        Ok(session_ids.len())
    }
}

fn validate_session_token(
    session: &Session,
    refresh_token: &RefreshToken,
) -> Result<(), DomainError> {
    if session.id.is_nil()
        || session.current_refresh_token_id.is_nil()
        || refresh_token.id.is_nil()
        || session.id != refresh_token.session_id
        || session.current_refresh_token_id != refresh_token.id
        || session.user_id.is_nil()
        || refresh_token.issued_at < session.created_at
        || session.expires_at <= session.created_at
        || session.last_rotated_at < session.created_at
        || session.last_rotated_at > session.expires_at
        || refresh_token.expires_at > session.expires_at
    {
        return Err(DomainError::InvalidInput(
            "invalid session or refresh token".to_owned(),
        ));
    }
    validate_refresh_token(refresh_token)
}

fn validate_refresh_token(refresh_token: &RefreshToken) -> Result<(), DomainError> {
    if refresh_token.id.is_nil()
        || refresh_token.session_id.is_nil()
        || refresh_token.verifier.is_empty()
        || refresh_token.expires_at <= refresh_token.issued_at
        || refresh_token.used_at.is_some()
        || refresh_token.revoked_at.is_some()
        || refresh_token.replaced_by.is_some()
    {
        return Err(DomainError::InvalidInput(
            "invalid refresh token".to_owned(),
        ));
    }
    Ok(())
}

fn revoke_session_state(state: &mut AuthState, session_id: Uuid, revoked_at: DateTime<Utc>) {
    if let Some(session) = state.sessions.get_mut(&session_id)
        && session.revoked_at.is_none()
    {
        session.revoked_at = Some(revoked_at);
    }
    for token in state
        .refresh_tokens
        .values_mut()
        .filter(|token| token.session_id == session_id && token.revoked_at.is_none())
    {
        token.revoked_at = Some(revoked_at);
    }
}

#[derive(Default)]
pub struct InMemoryResumeRepository {
    resumes: RwLock<HashMap<Uuid, Resume>>,
}

#[async_trait]
impl ResumeRepository for InMemoryResumeRepository {
    async fn create(&self, resume: Resume) -> Result<Resume, DomainError> {
        let mut resumes = self.resumes.write().await;
        if resumes.contains_key(&resume.id) {
            return Err(DomainError::Conflict);
        }
        resumes.insert(resume.id, resume.clone());
        Ok(resume)
    }

    async fn update(&self, resume: Resume) -> Result<Resume, DomainError> {
        let mut resumes = self.resumes.write().await;
        if !resumes.contains_key(&resume.id) {
            return Err(DomainError::NotFound);
        }
        resumes.insert(resume.id, resume.clone());
        Ok(resume)
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<Resume>, DomainError> {
        Ok(self.resumes.read().await.get(&id).cloned())
    }

    async fn find_by_ids(&self, ids: &[Uuid]) -> Result<Vec<Resume>, DomainError> {
        let resumes = self.resumes.read().await;
        Ok(ids
            .iter()
            .filter_map(|id| resumes.get(id).cloned())
            .collect())
    }

    async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<Resume>, DomainError> {
        Ok(self
            .resumes
            .read()
            .await
            .values()
            .filter(|resume| resume.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_by_user_paginated(
        &self,
        user_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Resume>, DomainError> {
        let mut items = self
            .resumes
            .read()
            .await
            .values()
            .filter(|resume| resume.user_id == user_id)
            .cloned()
            .collect::<Vec<_>>();
        items.sort_by_key(|resume| (std::cmp::Reverse(resume.created_at), resume.id));
        Ok(items.into_iter().skip(offset).take(limit).collect())
    }

    async fn delete(&self, id: Uuid) -> Result<(), DomainError> {
        self.resumes
            .write()
            .await
            .remove(&id)
            .map(|_| ())
            .ok_or(DomainError::NotFound)
    }
}

#[derive(Default)]
pub struct InMemoryJobRepository {
    jobs: RwLock<HashMap<Uuid, Job>>,
}

#[async_trait]
impl JobRepository for InMemoryJobRepository {
    async fn create(&self, job: Job) -> Result<Job, DomainError> {
        let mut jobs = self.jobs.write().await;
        if jobs.contains_key(&job.id) {
            return Err(DomainError::Conflict);
        }
        jobs.insert(job.id, job.clone());
        Ok(job)
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<Job>, DomainError> {
        Ok(self.jobs.read().await.get(&id).cloned())
    }

    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<Job>, DomainError> {
        let mut jobs = self.jobs.read().await.values().cloned().collect::<Vec<_>>();
        jobs.sort_by_key(|job| (job.created_at, job.id));
        Ok(jobs.into_iter().skip(offset).take(limit).collect())
    }

    async fn list_filtered(
        &self,
        offset: usize,
        limit: usize,
        filter: crate::domain::JobFilter,
    ) -> Result<Vec<Job>, DomainError> {
        let mut jobs = self.jobs.read().await.values().cloned().collect::<Vec<_>>();
        jobs.retain(|job| job_matches_filter(job, &filter));
        jobs.sort_by_key(|job| (job.created_at, job.id));
        Ok(jobs.into_iter().skip(offset).take(limit).collect())
    }
}

fn job_matches_filter(job: &Job, filter: &crate::domain::JobFilter) -> bool {
    if let Some(query) = &filter.query {
        let q = query.to_ascii_lowercase();
        let title = job.title.to_ascii_lowercase();
        let desc = job.description.to_ascii_lowercase();
        if !title.contains(&q) && !desc.contains(&q) {
            return false;
        }
    }
    if let Some(skills) = &filter.skills {
        let job_skills = job
            .skills
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let required = skills
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect::<Vec<_>>();
        if !required.iter().any(|skill| job_skills.contains(skill)) {
            return false;
        }
    }
    if let Some(location) = &filter.location {
        let loc = location.to_ascii_lowercase();
        let title = job.title.to_ascii_lowercase();
        let desc = job.description.to_ascii_lowercase();
        if !title.contains(&loc) && !desc.contains(&loc) {
            // also check extracted location via skills/location? For deterministic, check desc contains location
            return false;
        }
    }
    true
}

#[derive(Default)]
pub struct InMemoryApplicationRepository {
    applications: RwLock<HashMap<Uuid, Application>>,
}

#[async_trait]
impl ApplicationRepository for InMemoryApplicationRepository {
    async fn create(&self, application: Application) -> Result<Application, DomainError> {
        let mut applications = self.applications.write().await;
        if applications.contains_key(&application.id)
            || applications.values().any(|existing| {
                existing.job_id == application.job_id
                    && existing.resume_id == application.resume_id
                    && existing.status == application.status
            })
        {
            return Err(DomainError::Conflict);
        }
        applications.insert(application.id, application.clone());
        Ok(application)
    }

    async fn find_by_job_and_resume(
        &self,
        job_id: Uuid,
        resume_id: Uuid,
    ) -> Result<Option<Application>, DomainError> {
        Ok(self
            .applications
            .read()
            .await
            .values()
            .find(|application| application.job_id == job_id && application.resume_id == resume_id)
            .cloned())
    }

    async fn list_by_job(
        &self,
        job_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Application>, DomainError> {
        let mut applications = self
            .applications
            .read()
            .await
            .values()
            .filter(|application| application.job_id == job_id)
            .cloned()
            .collect::<Vec<_>>();
        applications.sort_by_key(|application| (application.created_at, application.id));
        Ok(applications.into_iter().skip(offset).take(limit).collect())
    }
}

#[derive(Default)]
pub struct InMemoryMatchResultRepository {
    results: RwLock<HashMap<Uuid, MatchResult>>,
}

#[async_trait]
impl MatchResultRepository for InMemoryMatchResultRepository {
    async fn create(&self, result: MatchResult) -> Result<MatchResult, DomainError> {
        let mut results = self.results.write().await;
        if results.contains_key(&result.id) {
            return Err(DomainError::Conflict);
        }
        results.insert(result.id, result.clone());
        Ok(result)
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<MatchResult>, DomainError> {
        Ok(self.results.read().await.get(&id).cloned())
    }

    async fn list_for_principal(
        &self,
        principal_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MatchResult>, DomainError> {
        let mut results = self
            .results
            .read()
            .await
            .values()
            .filter(|result| {
                result.candidate_id == principal_id || result.recruiter_id == principal_id
            })
            .cloned()
            .collect::<Vec<_>>();
        results.sort_by_key(|result| (std::cmp::Reverse(result.created_at), result.id));
        Ok(results.into_iter().skip(offset).take(limit).collect())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::InMemoryUserRepository;
    use crate::domain::entities::{RefreshToken, Role, Session, User};
    use crate::domain::repositories::UserRepository;
    use chrono::Utc;
    use uuid::Uuid;

    #[tokio::test]
    async fn refresh_token_reuse_revokes_the_entire_session_family() {
        let repository = InMemoryUserRepository::default();
        let now = Utc::now();
        let user = User {
            id: Uuid::now_v7(),
            email: "user@example.com".to_owned(),
            password_hash: "argon2id-test-hash".to_owned(),
            role: Role::Candidate,
            created_at: now,
        };
        let session = Session {
            id: Uuid::now_v7(),
            user_id: user.id,
            current_refresh_token_id: Uuid::now_v7(),
            created_at: now,
            last_rotated_at: now,
            expires_at: now + Duration::hours(1),
            revoked_at: None,
        };
        let first = RefreshToken {
            id: session.current_refresh_token_id,
            session_id: session.id,
            verifier: "digest-1".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        repository
            .create_with_session(user, session.clone(), first)
            .await
            .expect("initial auth state should persist");

        let replacement = RefreshToken {
            id: Uuid::now_v7(),
            session_id: session.id,
            verifier: "digest-2".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        repository
            .rotate_refresh_token("digest-1", replacement, now)
            .await
            .expect("first rotation should succeed");

        let reuse = RefreshToken {
            id: Uuid::now_v7(),
            session_id: session.id,
            verifier: "digest-3".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        assert!(matches!(
            repository
                .rotate_refresh_token("digest-1", reuse, now)
                .await,
            Err(crate::domain::DomainError::Unauthorized)
        ));
        assert!(
            repository
                .find_session(session.id)
                .await
                .expect("session lookup should succeed")
                .expect("session should exist")
                .revoked_at
                .is_some()
        );
        assert!(
            repository
                .find_refresh_token_by_verifier("digest-2")
                .await
                .expect("token lookup should succeed")
                .expect("replacement should exist")
                .revoked_at
                .is_some()
        );
    }
}
