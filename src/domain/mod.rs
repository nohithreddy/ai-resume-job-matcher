pub mod entities;
pub mod errors;
pub mod ports;
pub mod repositories;
pub mod similarity;

pub use entities::{
    Application, ApplicationStatus, Job, MatchResult, Recommendation, Resume, Role, User,
};
pub use errors::DomainError;
pub use ports::{
    CoverLetterGenerator, DocumentTextExtractor, EmbeddingProvider, InterviewQuestionGenerator,
    JobParser, ParsedJob, ParsedResume, PasswordService, ResumeParser, SimilarityScorer,
    TokenService, VirusScanner,
};
pub use repositories::{
    ApplicationRepository, JobFilter, JobRepository, MatchResultRepository, ResumeRepository,
    UserRepository,
};
pub use similarity::CosineSimilarity;
