# Resume Job Matcher

A Rust 2024, Axum-based service for authenticated resume ingestion, job
ingestion, weighted ATS scoring with explainable reports, job applications,
and recruiter-side candidate ranking.

## Run

```text
cargo run
```

Configuration is read from environment variables. See [`.env.example`](.env.example).
The JWT secret must be at least 32 bytes.

## Roles

Public registration accepts `candidate` (default) or `recruiter`. Candidates
manage resumes and apply to jobs. Recruiters publish jobs and rank submitted
applicants. `admin` cannot be created through the public API.

## API

- `GET /health/live` process liveness
- `GET /health/ready` dependency/readiness status
- `GET /metrics` Prometheus metrics
- `POST /api/v1/auth/register`
- `POST /api/v1/auth/login`
- `POST /api/v1/auth/refresh` rotate a refresh token; reuse revokes the session family
- `POST /api/v1/auth/logout`
- `GET|POST /api/v1/resumes` list own resumes / upload one (candidate)
- `GET|PUT|DELETE /api/v1/resumes/{resume_id}` manage one resume (owner)
- `GET /api/v1/jobs` public job board
- `POST /api/v1/jobs` publish a job (recruiter)
- `POST /api/v1/applications` apply a resume to a job (candidate)
- `GET /api/v1/jobs/{job_id}/applications` list applicants (job owner)
- `GET /api/v1/jobs/{job_id}/recommendations` weighted ATS ranking of submitted applications (owner)
- `GET|POST /api/v1/matches` persisted match results visible to candidate or recruiter
- `GET /api/v1/reports/{match_id}` explainable report: category scores, reasons,
  recommendations, and role/location/availability comparisons
- `GET /api/v1/openapi.json`

Protected endpoints use `Authorization: Bearer <jwt>` with short-lived access
tokens (max 15 minutes). Refresh tokens are opaque, stored only as keyed
Argon2id verifiers, and rotated on every use. Every response includes an
`X-Request-Id`; errors use RFC 7807 `application/problem+json`.

## Scoring

The ATS score is a weighted 0..100 composite: skills 40%, experience 20%,
semantic similarity 20%, education 10%, certifications 5%, keywords 5%.
Role, location, and availability comparisons are reported for explainability
without changing the score. Missing skills, reasons, and improvement
recommendations accompany every report.

## Persistence

The default backend is **SQLite** (`APP_PERSISTENCE=sqlite`, file at
`APP_DATABASE_PATH`, default `./data/matcher.db`) in WAL mode with a
versioned embedded migration. All repository ports from
`src/domain/repositories.rs` are implemented, including the atomic
user + session + refresh-token creation and rotation-with-reuse-detection.
Set `APP_PERSISTENCE=memory` for a volatile in-process adapter (used by tests).

NenDB remains documented as an unverified future option in
[docs/DATA_MODEL.md](docs/DATA_MODEL.md); it has no Rust driver and no
transactional guarantees today, so it is not wired.

## Verification

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
