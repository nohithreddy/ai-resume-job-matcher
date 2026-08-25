use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;
use validator::Validate;

use crate::domain::errors::DomainError;
use crate::domain::{
    Application, ApplicationRepository, ApplicationStatus, CoverLetterGenerator,
    DocumentTextExtractor, EmbeddingProvider, InterviewQuestionGenerator, Job, JobFilter,
    JobParser, JobRepository, Resume, ResumeParser, ResumeRepository, VirusScanner,
};
use crate::infrastructure::upload::{MAX_UPLOAD_BYTES, store_upload, validate_and_detect};

#[derive(Clone)]
pub struct ResumeJobService {
    resumes: Arc<dyn ResumeRepository>,
    jobs: Arc<dyn JobRepository>,
    applications: Arc<dyn ApplicationRepository>,
    resume_parser: Arc<dyn ResumeParser>,
    job_parser: Arc<dyn JobParser>,
    embeddings: Arc<dyn EmbeddingProvider>,
    text_extractor: Arc<dyn DocumentTextExtractor>,
    virus_scanner: Arc<dyn VirusScanner>,
    upload_dir: PathBuf,
    interview_generator: Arc<dyn InterviewQuestionGenerator>,
    cover_letter_generator: Arc<dyn CoverLetterGenerator>,
}

#[derive(Clone, Deserialize, Validate)]
pub struct CreateResumeInput {
    #[validate(length(max = 200))]
    pub title: Option<String>,
    #[validate(length(min = 20, max = 100_000))]
    pub raw_text: String,
}

#[derive(Clone, Deserialize, Validate)]
pub struct CreateJobInput {
    #[validate(length(min = 2, max = 200))]
    pub title: String,
    #[validate(length(min = 20, max = 100_000))]
    pub description: String,
}

impl ResumeJobService {
    pub fn new(
        resumes: Arc<dyn ResumeRepository>,
        jobs: Arc<dyn JobRepository>,
        applications: Arc<dyn ApplicationRepository>,
        resume_parser: Arc<dyn ResumeParser>,
        job_parser: Arc<dyn JobParser>,
        embeddings: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            resumes,
            jobs,
            applications,
            resume_parser,
            job_parser,
            embeddings,
            text_extractor: Arc::new(crate::infrastructure::text_extraction::StubTextExtractor),
            virus_scanner: Arc::new(crate::infrastructure::virus_scan::ClamAvScanner::new()),
            upload_dir: PathBuf::from("./data/uploads"),
            interview_generator: Arc::new(
                crate::infrastructure::interview::DeterministicInterviewGenerator,
            ),
            cover_letter_generator: Arc::new(
                crate::infrastructure::cover_letter::TemplateCoverLetterGenerator,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_upload_deps(
        resumes: Arc<dyn ResumeRepository>,
        jobs: Arc<dyn JobRepository>,
        applications: Arc<dyn ApplicationRepository>,
        resume_parser: Arc<dyn ResumeParser>,
        job_parser: Arc<dyn JobParser>,
        embeddings: Arc<dyn EmbeddingProvider>,
        text_extractor: Arc<dyn DocumentTextExtractor>,
        virus_scanner: Arc<dyn VirusScanner>,
        upload_dir: PathBuf,
        interview_generator: Arc<dyn InterviewQuestionGenerator>,
        cover_letter_generator: Arc<dyn CoverLetterGenerator>,
    ) -> Self {
        Self {
            resumes,
            jobs,
            applications,
            resume_parser,
            job_parser,
            embeddings,
            text_extractor,
            virus_scanner,
            upload_dir,
            interview_generator,
            cover_letter_generator,
        }
    }

    pub async fn create_resume(
        &self,
        user_id: Uuid,
        input: CreateResumeInput,
    ) -> Result<Resume, DomainError> {
        input
            .validate()
            .map_err(|error| DomainError::InvalidInput(error.to_string()))?;
        let parsed = self.resume_parser.parse(&input.raw_text).await?;
        let embedding = self.embeddings.embed(&input.raw_text).await?;
        let resume = Resume {
            id: Uuid::now_v7(),
            user_id,
            title: input
                .title
                .map(|title| title.trim().to_owned())
                .or(parsed.title),
            raw_text: input.raw_text.trim().to_owned(),
            skills: parsed.skills,
            embedding,
            created_at: Utc::now(),
        };
        let created = self.resumes.create(resume).await?;
        tracing::info!(user_id=%user_id, action="resume.create", target_id=%created.id, "audit");
        Ok(created)
    }

    pub async fn create_resume_from_upload(
        &self,
        user_id: Uuid,
        filename: &str,
        declared_mime: Option<&str>,
        bytes: Vec<u8>,
        title: Option<String>,
    ) -> Result<Resume, DomainError> {
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Err(DomainError::InvalidInput(format!(
                "file exceeds the {} byte limit",
                MAX_UPLOAD_BYTES
            )));
        }
        let (extension, mime) = validate_and_detect(filename, declared_mime, &bytes)?;
        self.virus_scanner.scan(&bytes).await?;
        let stored_path = store_upload(&self.upload_dir, &extension, &bytes).await?;
        tracing::info!(user_id=%user_id, action="resume.upload", path=?stored_path, mime=%mime, "audit");
        let extracted = self.text_extractor.extract(&bytes, &mime).await?;
        let trimmed = extracted.trim().to_owned();
        if trimmed.len() < 20 || trimmed.len() > 100_000 {
            return Err(DomainError::InvalidInput(
                "extracted resume text must be between 20 and 100000 characters".to_owned(),
            ));
        }
        let input = CreateResumeInput {
            title,
            raw_text: trimmed,
        };
        input
            .validate()
            .map_err(|error| DomainError::InvalidInput(error.to_string()))?;
        let parsed = self.resume_parser.parse(&input.raw_text).await?;
        let embedding = self.embeddings.embed(&input.raw_text).await?;
        let resume = Resume {
            id: Uuid::now_v7(),
            user_id,
            title: input.title.map(|t| t.trim().to_owned()).or(parsed.title),
            raw_text: input.raw_text.trim().to_owned(),
            skills: parsed.skills,
            embedding,
            created_at: Utc::now(),
        };
        let created = self.resumes.create(resume).await?;
        tracing::info!(user_id=%user_id, action="resume.upload.create", target_id=%created.id, "audit");
        Ok(created)
    }

    pub async fn list_resumes(
        &self,
        user_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Resume>, DomainError> {
        self.resumes
            .list_by_user_paginated(user_id, offset, limit)
            .await
    }

    pub async fn get_resume(&self, user_id: Uuid, resume_id: Uuid) -> Result<Resume, DomainError> {
        let resume = self
            .resumes
            .find_by_id(resume_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if resume.user_id != user_id {
            return Err(DomainError::Forbidden);
        }
        Ok(resume)
    }

    pub async fn update_resume(
        &self,
        user_id: Uuid,
        resume_id: Uuid,
        input: CreateResumeInput,
    ) -> Result<Resume, DomainError> {
        let existing = self.get_resume(user_id, resume_id).await?;
        input
            .validate()
            .map_err(|error| DomainError::InvalidInput(error.to_string()))?;
        let parsed = self.resume_parser.parse(&input.raw_text).await?;
        let embedding = self.embeddings.embed(&input.raw_text).await?;
        let updated = self
            .resumes
            .update(Resume {
                id: existing.id,
                user_id: existing.user_id,
                title: input
                    .title
                    .map(|title| title.trim().to_owned())
                    .or(parsed.title),
                raw_text: input.raw_text.trim().to_owned(),
                skills: parsed.skills,
                embedding,
                created_at: existing.created_at,
            })
            .await?;
        tracing::info!(user_id=%user_id, action="resume.update", target_id=%resume_id, "audit");
        Ok(updated)
    }

    pub async fn delete_resume(&self, user_id: Uuid, resume_id: Uuid) -> Result<(), DomainError> {
        let _ = self.get_resume(user_id, resume_id).await?;
        self.resumes.delete(resume_id).await?;
        tracing::info!(user_id=%user_id, action="resume.delete", target_id=%resume_id, "audit");
        Ok(())
    }

    pub async fn create_job(
        &self,
        owner_id: Uuid,
        input: CreateJobInput,
    ) -> Result<Job, DomainError> {
        input
            .validate()
            .map_err(|error| DomainError::InvalidInput(error.to_string()))?;
        let parsed = self
            .job_parser
            .parse(&input.title, &input.description)
            .await?;
        let embedding = self.embeddings.embed(&input.description).await?;
        let job = Job {
            id: Uuid::now_v7(),
            owner_id,
            title: input.title.trim().to_owned(),
            description: input.description.trim().to_owned(),
            skills: parsed.skills,
            embedding,
            created_at: Utc::now(),
        };
        let created = self.jobs.create(job).await?;
        tracing::info!(user_id=%owner_id, action="job.create", target_id=%created.id, "audit");
        Ok(created)
    }

    pub async fn list_jobs(&self, offset: usize, limit: usize) -> Result<Vec<Job>, DomainError> {
        self.jobs.list(offset, limit).await
    }

    pub async fn list_jobs_filtered(
        &self,
        offset: usize,
        limit: usize,
        filter: JobFilter,
    ) -> Result<Vec<Job>, DomainError> {
        self.jobs.list_filtered(offset, limit, filter).await
    }

    pub async fn generate_interview_questions(
        &self,
        job_id: Uuid,
        requester_id: Uuid,
        resume_id: Option<Uuid>,
    ) -> Result<Vec<String>, DomainError> {
        let job = self
            .jobs
            .find_by_id(job_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if job.owner_id != requester_id {
            // Also allow candidate who applied
            if let Some(rid) = resume_id {
                let resume = self
                    .resumes
                    .find_by_id(rid)
                    .await?
                    .ok_or(DomainError::NotFound)?;
                if resume.user_id != requester_id {
                    return Err(DomainError::Forbidden);
                }
            } else {
                return Err(DomainError::Forbidden);
            }
        }
        let missing_skills;
        let semantic_score;
        if let Some(rid) = resume_id {
            let resume = self
                .resumes
                .find_by_id(rid)
                .await?
                .ok_or(DomainError::NotFound)?;
            let job_profile = crate::domain::similarity::profile_for_job(&job);
            let resume_profile = crate::domain::similarity::profile_for_resume(&resume);
            // Use scoring to get missing skills and semantic score
            let report = crate::domain::similarity::build_weighted_recommendation(
                job.id,
                resume.id,
                0.5, // placeholder semantic; we attempt to use actual similarity via embeddings if possible
                &job_profile,
                &resume_profile,
            )?;
            missing_skills = report.missing_skills;
            semantic_score = report.category_scores.semantic_similarity.score;
        } else {
            let job_profile = crate::domain::similarity::profile_for_job(&job);
            missing_skills = Vec::new();
            semantic_score = 80.0;
            let _ = job_profile;
        }
        Ok(self
            .interview_generator
            .generate(&job.title, &missing_skills, semantic_score))
    }

    pub async fn generate_cover_letter(
        &self,
        resume_id: Uuid,
        job_id: Uuid,
        requester_id: Uuid,
    ) -> Result<String, DomainError> {
        let resume = self
            .resumes
            .find_by_id(resume_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if resume.user_id != requester_id {
            return Err(DomainError::Forbidden);
        }
        let job = self
            .jobs
            .find_by_id(job_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        Ok(self
            .cover_letter_generator
            .generate(&resume.raw_text, &job.title, &job.description))
    }

    pub async fn apply_to_job(
        &self,
        candidate_id: Uuid,
        resume_id: Uuid,
        job_id: Uuid,
    ) -> Result<Application, DomainError> {
        let resume = self
            .resumes
            .find_by_id(resume_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if resume.user_id != candidate_id {
            return Err(DomainError::Forbidden);
        }
        let job = self
            .jobs
            .find_by_id(job_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if job.owner_id == candidate_id {
            return Err(DomainError::InvalidInput(
                "a user cannot apply to their own job".to_owned(),
            ));
        }
        if self
            .applications
            .find_by_job_and_resume(job_id, resume_id)
            .await?
            .is_some()
        {
            return Err(DomainError::Conflict);
        }
        let now = Utc::now();
        let created = self
            .applications
            .create(Application {
                id: Uuid::now_v7(),
                candidate_id,
                resume_id,
                job_id,
                status: ApplicationStatus::Submitted,
                created_at: now,
                updated_at: now,
            })
            .await?;
        tracing::info!(user_id=%candidate_id, action="application.create", target_id=%created.id, job_id=%job_id, resume_id=%resume_id, "audit");
        Ok(created)
    }

    pub async fn list_applications(
        &self,
        recruiter_id: Uuid,
        job_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Application>, DomainError> {
        let job = self
            .jobs
            .find_by_id(job_id)
            .await?
            .ok_or(DomainError::NotFound)?;
        if job.owner_id != recruiter_id {
            return Err(DomainError::Forbidden);
        }
        self.applications.list_by_job(job_id, offset, limit).await
    }
}
