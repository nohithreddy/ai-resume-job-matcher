use crate::domain::CoverLetterGenerator;

pub struct TemplateCoverLetterGenerator;

impl CoverLetterGenerator for TemplateCoverLetterGenerator {
    fn generate(&self, resume_text: &str, job_title: &str, job_description: &str) -> String {
        let snippet = resume_text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("my background")
            .chars()
            .take(120)
            .collect::<String>();
        let job_snippet = job_description
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(job_description)
            .chars()
            .take(200)
            .collect::<String>();

        format!(
            "Dear Hiring Manager,\n\n\
            I am excited to apply for the {job_title} position. {snippet} aligns with the requirements described: {job_snippet}\n\n\
            My experience and skills make me a strong fit for this role. I am eager to bring my expertise to your team and contribute to your success.\n\n\
            Thank you for considering my application. I look forward to the opportunity to discuss how I can add value.\n\n\
            Sincerely,\n\
            Candidate"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::CoverLetterGenerator;

    #[test]
    fn template_fills_fields() {
        let cover_gen = TemplateCoverLetterGenerator;
        let letter = cover_gen.generate(
            "Backend Engineer with Rust and SQL",
            "Rust Platform Engineer",
            "Build Rust services",
        );
        assert!(letter.contains("Rust Platform Engineer"));
        assert!(letter.contains("Backend Engineer"));
    }
}
