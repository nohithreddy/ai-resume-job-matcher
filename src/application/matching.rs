use std::sync::Arc;

use uuid::Uuid;

use crate::domain::SimilarityScorer;
use crate::domain::similarity::{
    build_weighted_recommendation, profile_for_job, profile_for_resume,
};
use crate::domain::{
    ApplicationRepository, ApplicationStatus, DomainError, Job, JobRepository, MatchResult,
    MatchResultRepository, Recommendation, Resume, ResumeRepository,
};

#[derive(Clone)]
pub struct MatchingService {
    resumes: Arc<dyn ResumeRepository>,
    jobs: Arc<dyn JobRepository>,
    applications: Arc<dyn ApplicationRepository>,
    matches: Arc<dyn MatchResultRepository>,
    scorer: Arc<dyn SimilarityScorer>,
}

impl MatchingService {
    pub fn new(
        resumes: Arc<dyn ResumeRepository>,
        jobs: Arc<dyn JobRepository>,
        applications: Arc<dyn ApplicationRepository>,
        matches: Arc<dyn MatchResultRepository>,
        scorer: Arc<dyn SimilarityScorer>,
    ) -> Self {
        Self {
            resumes,
            jobs,
            applications,
            matches,
            scorer,
        }
    }

    pub async fn recommendations_for_job(
        &self,
        job_id: Uuid,
        requester_id: Uuid,
        limit: usize,
    ) -> Result<Vec<Recommendation>, DomainError> {
        let job = self
            .jobs
            .find_by_id(job_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if job.owner_id != requester_id {
            return Err(DomainError::Forbidden);
        }
        let applications = self.applications.list_by_job(job_id, 0, limit).await?;
        let resume_ids = applications
            .into_iter()
            .filter(|application| application.status == ApplicationStatus::Submitted)
            .map(|application| application.resume_id)
            .collect::<Vec<_>>();
        let resumes = self.resumes.find_by_ids(&resume_ids).await?;
        self.rank(&job, &resumes, limit)
    }

    pub async fn create_match(
        &self,
        requester_id: Uuid,
        resume_id: Uuid,
        job_id: Uuid,
    ) -> Result<MatchResult, DomainError> {
        let resume = self
            .resumes
            .find_by_id(resume_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        let job = self
            .jobs
            .find_by_id(job_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if requester_id != resume.user_id && requester_id != job.owner_id {
            return Err(DomainError::Forbidden);
        }
        if requester_id == job.owner_id && requester_id != resume.user_id {
            let application = self
                .applications
                .find_by_job_and_resume(job_id, resume_id)
                .await?
                .ok_or(DomainError::Forbidden)?;
            if application.candidate_id != resume.user_id
                || application.status != ApplicationStatus::Submitted
            {
                return Err(DomainError::Forbidden);
            }
        }
        let report = self
            .rank(&job, std::slice::from_ref(&resume), 1)?
            .into_iter()
            .next()
            .ok_or_else(|| DomainError::Internal("matching produced no report".to_owned()))?;
        let created = self
            .matches
            .create(MatchResult {
                id: Uuid::now_v7(),
                resume_id,
                job_id,
                candidate_id: resume.user_id,
                recruiter_id: job.owner_id,
                requested_by: requester_id,
                report,
                created_at: chrono::Utc::now(),
            })
            .await?;
        tracing::info!(user_id=%requester_id, action="match.create", target_id=%created.id, resume_id=%resume_id, job_id=%job_id, "audit");
        Ok(created)
    }

    pub async fn list_matches(
        &self,
        principal_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MatchResult>, DomainError> {
        self.matches
            .list_for_principal(principal_id, offset, limit)
            .await
    }

    pub async fn get_report(
        &self,
        principal_id: Uuid,
        match_id: Uuid,
    ) -> Result<MatchResult, DomainError> {
        let result = self
            .matches
            .find_by_id(match_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if principal_id != result.candidate_id && principal_id != result.recruiter_id {
            return Err(DomainError::Forbidden);
        }
        Ok(result)
    }

    pub fn rank(
        &self,
        job: &Job,
        resumes: &[Resume],
        limit: usize,
    ) -> Result<Vec<Recommendation>, DomainError> {
        let job_profile = profile_for_job(job);
        let mut recommendations = resumes
            .iter()
            .map(|resume| {
                let semantic_similarity =
                    self.scorer.similarity(&job.embedding, &resume.embedding)?;
                let resume_profile = profile_for_resume(resume);
                build_weighted_recommendation(
                    job.id,
                    resume.id,
                    semantic_similarity,
                    &job_profile,
                    &resume_profile,
                )
            })
            .collect::<Result<Vec<_>, DomainError>>()?;
        recommendations.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.resume_id.cmp(&right.resume_id))
        });
        recommendations.truncate(limit);
        Ok(recommendations)
    }
}
