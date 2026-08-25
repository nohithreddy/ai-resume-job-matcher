# HANDOFF — AI-Powered Resume & Job Matcher

**Date:** 2026-05-11  
**Location:** `C:\Users\91767\Desktop\agents`  
**Service:** `resume-job-matcher` v0.1.0 — Rust 2024 / Axum / Tokio  
**Status:** Production-grade vertical slice **DONE** — durable, test-covered, and deployable. Remaining gaps are explicitly documented below; none are load-bearing for the shipped slice.

---

## 1. What This Is

A secure, modular REST service that:

- parses resumes and job descriptions,
- extracts skills / experience / education / certifications / keywords / location / availability,
- embeds text (deterministic local or OpenAI-compatible HTTP),
- computes a **weighted ATS score 0..100** with full explainability,
- manages roles (candidate / recruiter / admin), applications, persisted match reports,
- exposes a clean versioned API with OpenAPI, Prometheus metrics, structured tracing, and rate limiting,
- persists durably to **SQLite (WAL)** by default, with an in-memory opt-in for tests.

Designed to the spec's non-negotiables: clean architecture, single-responsibility modules, no `unwrap`/`panic` in production paths, `Result`-everywhere, dependency injection via traits.

---

## 2. Tech Stack (as shipped)

| Layer | Choice |
|---|---|
| Language | Rust stable 1.88 (see `rust-toolchain.toml`) |
| Web | Axum 0.8 + Tokio rt-multi-thread + tower-http (trace, catch-panic, request-id, rate-limit) |
| DB | **SQLite via rusqlite 0.32 bundled** (WAL, `busy_timeout 5s`, versioned `PRAGMA user_version` migration). `InMemoryRepositories` retained for tests. |
| Auth | Argon2id (argon2 0.5) + HS256 JWT (jsonwebtoken 9) + opaque refresh tokens digested with keyed Argon2id |
| Embeddings | `DeterministicEmbeddingProvider` (64-dim feature-hash, offset-stable) + `HttpEmbeddingProvider` (reqwest rustls, timeouts) — both behind `EmbeddingProvider` trait |
| Validation | `validator` 0.21 derive |
| Observability | `tracing` + `tracing-subscriber` (json, env-filter), `metrics` + `metrics-exporter-prometheus` |
| API docs | `utoipa` 5 (OpenAPI JSON at `/api/v1/openapi.json`) |
| Misc | `uuid` v7, `chrono`, `serde`, `thiserror`, `async-trait` |

> **Requested but not wired:** `NenDB` — see §6. No `utoipa` Swagger UI, no ONNX/GGUF/LLM, no PDF/DOCX or OCR yet.

---

## 3. Project Structure

```
src/
  lib.rs                 # composition root: build_application() — resolves RepositorySet, hasher, JWT, embeddings, services, AppState
  main.rs                # binds TcpListener, installs tracing, graceful shutdown (ctrl_c + SIGTERM)
  config.rs              # AppConfig::from_env() — all knobs, validated; for_tests() helper
  domain/
    entities.rs          # User, Role, Session, RefreshToken, Resume, Job, Application, MatchResult, Recommendation + scoring structs
    errors.rs            # DomainError (NotFound/Conflict/Unauthorized/Forbidden/InvalidInput/EmbeddingMismatch/InvalidEmbedding/DependencyUnavailable/Internal)
    ports.rs             # ResumeParser, JobParser, EmbeddingProvider, SimilarityScorer, PasswordService, TokenService
    repositories.rs      # UserRepository, ResumeRepository, JobRepository, ApplicationRepository, MatchResultRepository (no NenDB types)
    similarity.rs        # CosineSimilarity + weighted ATS scorer + profile parsers + vocab
  application/
    auth.rs              # AuthService (register/login/authenticate/refresh/logout/revoke, dummy-hash timing equalization)
    resume_jobs.rs       # ResumeJobService (CRUD + applications + pagination)
    matching.rs          # MatchingService (recommendations_for_job, create_match, list/get reports) — deterministic ranking
  infrastructure/
    mod.rs               # RepositorySet, open_repositories(), label() — backend switch
    memory.rs            # InMemoryRepositories (RwLock<HashMap<...>>) — test/dev adapter
    sqlite.rs            # SqliteRepositories — all 5 ports, blocking-pool + Mutex<Connection>, atomic transactions
    security.rs          # PasswordHasher (Argon2id, blocking pool) + SecurityService (JWT with iss/aud/nbf/jti, refresh verifier)
    parsing.rs           # DeterministicResumeParser / DeterministicJobParser — thin wrappers over similarity profile parsers
    embedding.rs         # Deterministic (FNV-hash) + HTTP (OpenAI-compatible) providers
  interfaces/
    http/
      mod.rs             # AppState, BootstrapError, install_metrics_recorder() (OnceLock), build_router() — nested /auth sub-router with rate-limit layer
      routes.rs          # All handlers, DTOs, From impls, ProblemJson/Query/Path extractors, AuthenticatedUser extractor
      errors.rs          # ApiError → RFC 7807 ProblemDetails (application/problem+json), WWW-Authenticate on 401, Retry-After on 429
      middleware.rs      # request_id (propagate-or-Uuid::now_v7), request_metrics, panic_response
      rate_limit.rs      # AuthRateLimiter (Mutex<HashMap<String, VecDeque<Instant>>>), auth_rate_limit middleware, FALLBACK_KEY="unknown"
      openapi.rs         # ApiDoc (utoipa derive) — every path + schema, bearerAuth scheme
tests/
  api.rs                 # 5 integration tests (oneshot, no network): health, full recruiter/candidate flow, sqlite restart durability, refresh/role boundaries, rate-limit burst
docs/
  API_GUIDE.md, ARCHITECTURE.md, DATA_MODEL.md, DEPLOYMENT.md, diagrams/
Dockerfile, docker-compose.yml, .github/workflows/ci.yml + security.yml, .env.example, rust-toolchain.toml, .gitignore
```

Every module has a single responsibility; domain has zero framework imports.

---

## 4. What's Built (feature-complete for the slice)

### 4.1 Auth & Sessions
- Roles: `candidate` (default) / `recruiter` / `admin`. Public `POST /auth/register` rejects `admin`; `role` field defaults to `candidate`.
- Argon2id hashing on the blocking pool (`PasswordHasher::new(memory_cost)`; config default 19_456 KiB, test 8192).
- JWT: HS256, `iss=resume-job-matcher`, `aud=resume-job-matcher-api`, claims `sub/role/sid/jti/iat/nbf/exp`, `exp - iat ≤ 900s` enforced at decode, `WWW-Authenticate: Bearer` on 401. Secret ≥ 32 bytes, TTL validated >0 and clamped to 900s by `SecurityService`.
- Refresh tokens: 32 random bytes → 64-char hex, stored only as **keyed Argon2id verifier** (`REFRESH_SALT` + signing secret). `generate_refresh_token` + `refresh_token_verifier`.
- Rotation: `rotate_refresh_token(current_verifier, replacement, rotated_at)` — detects replay (`used_at`/`replaced_by`/stale `current_refresh_token_id`), revokes entire session family, validates replacement window and expiry. Millisecond-domain comparisons fix a real truncation bug between in-memory nanos and DB millis.
- Endpoints: `register` (201), `login` (200), `refresh` (200, rotates), `logout` (204 or 401 idempotent).
- Timing equalization: `AuthService` holds a startup dummy hash; `login` always verifies against either the real hash or the dummy, so missing-email and wrong-password paths take the same Argon2 path.
- Rate limiting: per-IP sliding window on `/api/v1/auth/*` only. Extracts `ConnectInfo<SocketAddr>` (main.rs uses `into_make_service_with_connect_info`), fallback key `"unknown"` for tests. Env `APP_AUTH_RATE_LIMIT_WINDOW_SECONDS` / `MAX_REQUESTS`. Emits `429` ProblemDetails + `Retry-After` + `auth_rate_limited_total` counter.

### 4.2 Resumes & Jobs
- `ResumeJobService` validates via `validator` (title ≤200, raw_text/description 512 KiB body limit + 20..100_000 field), parses, embeds, persists.
- `POST /resumes` (candidate/admin), `GET /resumes?offset&limit` (own, paginated, `Reverse(created_at)`), `GET/PUT/DELETE /resumes/{id}` (owner check).
- `POST /jobs` (recruiter/admin), `GET /jobs?offset&limit` (public board, `ORDER BY created_at,id`).
- Applications: `POST /applications {resume_id, job_id}` (candidate) — rejects self-application, duplicate (409), wrong owner (403); `GET /jobs/{id}/applications` (job owner, paginated).

### 4.3 Matching & Reports
- **Scoring weights** (`similarity.rs` constants, verified `TOTAL_WEIGHT == 100`):
  `SKILLS 40` + `EXPERIENCE 20` + `SEMANTIC_SIMILARITY 20` + `EDUCATION 10` + `CERTIFICATIONS 5` + `KEYWORDS 5`.
- For each category: `score 0..100` → `weighted_score = score * weight / 100` → sum clamped to `0..TOTAL_WEIGHT` as final `Recommendation.score`.
- `semantic_fit_score = clamp(cosine,-1..1 → 0..1) * 100`. `ratio_score` for skills/certs/keywords (matched/required *100; empty required → 100). Experience: `candidate/required*100` (None→0, no requirement→100). Education: rank-based ratio.
- Reasons per category + `comparisons` (role/location/availability, outcomes `Match/Partial/Mismatch/Unknown/NotSpecified`, explanatory only — do not affect score) + `recommendations` list (missing skills, experience gap, certs, keywords, semantic <50, context mismatches).
- Parsers: deterministic, case-insensitive, token-boundary vocab (`KNOWN_SKILLS` 23 terms, `KNOWN_CERTIFICATIONS`, `KNOWN_KEYWORDS`), `extract_labeled_value` for `role/location/availability`, `extract_experience_years` (`N year(s)/yr(s)`), `extract_education` (doctorate→high-school priority). `profile_for_*` merges stored skills into re-parsed profile so old records stay matchable.
- `MatchingService::recommendations_for_job(job_id, requester, limit)` — loads job, checks owner, loads `ApplicationRepository::list_by_job` (Submitted only) → `find_by_ids` → `rank` (cosine via `CosineSimilarity` in f64, total_cmp then resume_id tie-break, truncate). `create_match` persists a `MatchResult` with embedded `Recommendation` report; `list_matches` / `get_report` enforce `candidate_id == principal || recruiter_id == principal`.

### 4.4 Persistence
- **Default: SQLite** at `APP_DATABASE_PATH` (default `./data/matcher.db`, Docker `/data/matcher.db`), WAL + `synchronous=NORMAL` + `busy_timeout 5s`, named volume `matcher-data:/data` (compose), `/data` created owned by `app:app` (Dockerfile).
- `RepositorySet::open()` / `label()` in `infrastructure/mod.rs`; `lib.rs` builds it once and threads `persistence_label` into `AppState` for `/health/ready`.
- `SqliteRepositories` (`sqlite.rs`, ~1400 lines) implements all 5 traits over a single `Arc<Mutex<Connection>>` via `spawn_blocking`. Atomic `create_with_session` and `rotate_refresh_token` use `unchecked_transaction`. Helpers: `millis`/`from_millis`, role/status ↔ text, `skills_to_text`/`embedding_to_blob` (LE f32), `storage_error` maps `ConstraintViolation→409`, `DatabaseBusy/DiskFull/OperationInterrupted→503`.
- `InMemoryRepositories` kept for fast unit/integration tests (no file I/O). `AppConfig::for_tests()` → `Memory`.

### 4.5 HTTP & Observability
- `AppState` holds `config`, `auth`, `resume_jobs`, `matching`, `metrics` (OnceLock PrometheusHandle), `persistence_label`, `rate_limiter`.
- `build_router`: `/health/*`, `/metrics`, `/api/v1/openapi.json`, nested `/api/v1/auth` with `route_layer(from_fn_with_state(rate_limit))`, rest of API, `method_not_allowed_fallback` + `not_found` fallback, `DefaultBodyLimit 512 KiB`, `CatchPanicLayer`, `TraceLayer`, `request_metrics`, `request_id`.
- `X-Request-Id` propagate-or-generate (Uuid v7, validated `≤128` alnum/`-_`/`/.`); `request_metrics` records `http_requests_total` + `http_request_duration_seconds` with `method/route/status`.
- Errors: `ApiError` → `ProblemDetails {type,title,status,detail}` with `Content-Type: application/problem+json`. Never leaks stack traces; internal/embedding errors are logged and returned as 500/503 with generic detail.

---

## 5. Key Decisions & Why

| Decision | Why | Trade-off |
|---|---|---|
| **SQLite (rusqlite bundled) as default durable store** | NenDB has no Rust crate / no transactions / self-described as not production-ready (Zig, single-process, no WAL) — verified via live crate/GitHub searches in earlier turns. SQLite gives widest test/deploy coverage, single-file restart durability, real transactions for `create_with_session`, and no external service. | Graph queries not needed; vector search would need a different store later. Migration path kept via repository traits — NenDB/ Postgres can be slotted without touching domain/HTTP. |
| **Repository traits hide storage completely** | `src/domain/repositories.rs` exposes only `async fn` with domain entities; no `rusqlite`/`sqlx` types leak. `DATA_MODEL.md` defines logical collections + verification gate. | In-memory adapter still benefits from the same contract; unbounded `list` calls noted as pagination debt. |
| **Weighted ATS, not raw cosine** | Spec demands category breakdown + 100-point score; raw cosine is `-1..1` and not explainable. Folding semantic similarity as one 20-point category preserves determinism while exposing skill/experience/education gaps. | Deterministic local embeddings limit semantic quality — HTTP provider exists for a real model. |
| **Role-gated registration (candidate/recruiter), admin not self-registrable** | Prevents privilege escalation via mass assignment; `admin` must be seeded. Enforced both at application layer and with `Role::permits` checks on every protected handler. | Recruiter ↔ candidate is the only public boundary; finer RBAC (e.g. orgs) deferred. |
| **Short JWT (≤15 min) + opaque refresh rotation** | Limits blast radius of leaked access tokens; refresh reuse revokes family (OWASP-recommended). Keyed Argon2id verifier avoids storing raw tokens. | Extra round-trip on expiry; refresh endpoint is rate-limited. |
| **In-memory sliding-window limiter, no new deps** | `HashMap<String, VecDeque<Instant>>` behind `Mutex` is sufficient at this scale; per-IP, window 60s / 10 hits by default. | Not distributed — each replica has its own window. Documented; a Redis limiter would be the next step for horizontal scale. |
| **Deterministic local parser + 64-dim hash embedding** | Offline, reproducible, fast; no model download, no API key. HTTP provider behind same `EmbeddingProvider` trait keeps the seam open for `all-MiniLM-L6-v2` 384-dim later. | Skill vocab is fixed (23 terms); education/experience extraction is heuristic — LLM/ONNX would improve recall. |
| **Single `Mutex<Connection>` + `spawn_blocking`** | rusqlite is synchronous; this avoids holding an async lock across `.await` and keeps WAL concurrency safe. | Write contention serializes; acceptable for the current throughput target. A pool (e.g. `deadpool-sqlite`) would be next. |

---

## 6. NenDB — What We Found

- No published Rust crate (`crates.io` 404, no Rust repo in the org search).
- Go/Python drivers exist but wrap a Zig HTTP server exposing `/nodes`, `/edges`, `/query`, `/health` — **no documented unique indexes, no conditional creates, no multi-document transactions**.
- The project's own `CURRENT_STATUS.md` disclaims production readiness (single-process, basic concurrency, WAL not production-ready).
- **Verdict used:** keep NenDB out of the build; document it as an unverified future option in `docs/DATA_MODEL.md` with a verification checklist (driver, TLS, indexes, atomicity, backup) that must pass before any adapter is written. This avoids coupling the Rust service to an unproven API.

---

## 7. Configuration

All via env (see `.env.example`); `AppConfig::from_env()` validates.

| Var | Default | Notes |
|---|---|---|
| `APP_JWT_SECRET` | *(required)* | ≥32 bytes or `WeakJwtSecret` |
| `APP_JWT_TTL_SECONDS` | `3600` | clamped to ≤900 by `SecurityService` |
| `APP_ARGON2_MEMORY_COST` | `19456` | ≥8192 |
| `APP_BIND_ADDRESS` | `127.0.0.1:3000` | Docker overrides to `0.0.0.0:3000` |
| `APP_LOG_FILTER` | `resume_job_matcher=info,tower_http=info` |  |
| `APP_PERSISTENCE` | `sqlite` | `sqlite` or `memory` |
| `APP_DATABASE_PATH` | `./data/matcher.db` | `/data/matcher.db` in Docker |
| `APP_EMBEDDING_ENDPOINT` | *(none)* | if set, uses `HttpEmbeddingProvider` |
| `APP_EMBEDDING_API_KEY` | *(none)* | optional bearer |
| `APP_EMBEDDING_MODEL` | `text-embedding-3-small` |  |
| `APP_AUTH_RATE_LIMIT_WINDOW_SECONDS` | `60` | >0 |
| `APP_AUTH_RATE_LIMIT_MAX_REQUESTS` | `10` | >0 |

---

## 8. API Surface (current)

```
GET  /health/live
GET  /health/ready              → {status, persistence, embeddings}
GET  /metrics                   → Prometheus text
GET  /api/v1/openapi.json       → utoipa OpenAPI

POST /api/v1/auth/register      {email, password, role?} → 201 AuthResponse
POST /api/v1/auth/login         {email, password}        → 200 AuthResponse
POST /api/v1/auth/refresh       {refresh_token}          → 200 AuthResponse (rotates)
POST /api/v1/auth/logout        {refresh_token}          → 204

GET  /api/v1/resumes?offset&limit   (candidate) → ResumeList
POST /api/v1/resumes                (candidate) → 201 ResumeResponse
GET  /api/v1/resumes/{id}           (owner)     → ResumeResponse
PUT  /api/v1/resumes/{id}           (owner)     → ResumeResponse
DELETE /api/v1/resumes/{id}         (owner)     → 204

GET  /api/v1/jobs?offset&limit               → JobList (public)
POST /api/v1/jobs                  (recruiter) → 201 JobResponse
POST /api/v1/applications          (candidate) → 201 ApplicationResponse
GET  /api/v1/jobs/{id}/applications (owner)   → ApplicationList
GET  /api/v1/jobs/{id}/recommendations?limit  (owner) → RecommendationList (weighted reports)
POST /api/v1/matches               (candidate|owner) → 201 MatchResultResponse (persisted report)
GET  /api/v1/matches?offset&limit  (principal) → MatchResultList
GET  /api/v1/reports/{match_id}    (principal) → MatchResultResponse
```

`AuthResponse` = `{user_id, role, access_token, refresh_token, token_type, expires_in, refresh_expires_in, session_id}`.  
`ResumeResponse`/`JobResponse` never expose `raw_text`/`description`/`embedding`.  
All errors are RFC 7807 `application/problem+json`; 429 includes `Retry-After`.

---

## 9. Persistence Detail (SQLite)

- File: `APP_DATABASE_PATH`, auto-created parent dirs, `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout 5s`.
- Tables: `users(id PK, email UNIQUE, password_hash, role, created_at)`, `sessions`, `refresh_tokens(verifier PK, id UNIQUE)`, `resumes`, `jobs`, `applications`, `match_results` + indexes on every lookup path.
- Encodings: skills JSON, embedding LE f32 BLOB, report JSON, UUID TEXT, millis `INTEGER`.
- Migrations: `PRAGMA user_version` checked against `SCHEMA_VERSION=1`; `SCHEMA_V1` executed in a transaction.
- Restart proof: `tests/api.rs::sqlite_backend_persists_across_application_restarts` registers + creates a resume, drops the first `Router`, rebuilds a second `Router` over the same file, and verifies login, resume listing, readiness label, and refresh rotation all still work.

---

## 10. Security Posture

- OWASP-aligned: Argon2id (not SHA), short JWT + rotated opaque refresh, `WWW-Authenticate` on 401, no stack traces, input `validator` + body limit, `CatchPanicLayer` → generic 500, `X-Request-Id` everywhere, `Secure` assumptions documented, file paths never executed (no file upload yet).
- Timing: dummy Argon2 verify on unknown email; refresh verifier is a keyed digest so timing doesn't leak the raw token.
- Refresh reuse → revoke entire session family; `authenticate_claims` re-checks user, role, session liveness on every request.
- Rate limiting on auth only; other routes are not throttled (documented).

---

## 11. Scoring — How to Read a Report

A `Recommendation` (or `MatchResult.report`) contains:
- `score 0..100` (sum of weighted categories, clamped),
- `matched_skills` / `missing_skills`,
- `category_scores: {skills,experience,education,semantic_similarity,certifications,keywords}` each `{score, weight, weighted_score, reasons}`,
- `reasons` (flat), `recommendations` (actionable), `comparisons {role,location,availability}` (each `{resume_value, job_value, outcome, reason}`).

Tune weights in `src/domain/similarity.rs` constants; every test asserts `score == 100` on a perfect-match fixture.

---

## 12. Tests & Verification

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# → 28/28 green as of this handoff:
#    23 unit (cosine, ATS weighting, parsers, embeddings, security, memory+sqlite rotations)
#    5 integration (health, full candidate→recruiter flow, sqlite restart, refresh/role boundaries, rate-limit burst)
```

CI (`.github/workflows/ci.yml`) formats, checks, clips (`-D warnings`), tests, builds release binary, builds Docker image, and smoke-tests the container. `security.yml` runs `cargo audit` + Trivy repo/secret/config/image scans (pinned SHAs). `rust-toolchain.toml` pins `stable` + `rustfmt`.

---

## 13. Deployment

- **Dockerfile** (multi-stage, `rust:1.88-bookworm` → `debian:bookworm-slim`, pinned digests): cached `cargo build --release`, `app:app` (10001), read-only rootfs, `no-new-privileges`, `pids_limit 256`, `/data` owned by `app`, `HEALTHCHECK` on `/health/live` (readiness is informational — reports `persistence`/`embeddings`).
- **docker-compose.yml**: `matcher` service, `env_file` passthrough, named volume `matcher-data:/data`, `tmpfs /tmp`, `security_opt`, `cap_drop ALL`.
- **Env:** `APP_JWT_SECRET` required (≥32 bytes); everything else has safe defaults.
- **On-disk:** `./data/matcher.db*` + `-wal`/`-shm` are gitignored; CI never writes to host `./data`.

---

## 14. What's Left / Known Gaps

1. **File upload** — PDF/DOCX/MIME/double-extension/executable rejection, randomized filenames outside web root, virus-scan abstraction, max 10 MB enforcement. Current ingestion is JSON `raw_text` only.
2. **Real embeddings** — replace or feature-flag the 64-dim hash with `sentence-transformers/all-MiniLM-L6-v2` 384-dim via `ort`/`onnxruntime` or keep the HTTP seam and validate `APP_EMBEDDING_ENDPOINT` responses dimensionally.
3. **Vector DB / NenDB** — only after the verification gate in `docs/DATA_MODEL.md` passes; otherwise consider `pgvector` or `Qdrant` behind the same `EmbeddingProvider` + repository traits.
4. **Swagger UI** — only `openapi.json` is served; add `utoipa-swagger-ui` route if desired.
5. **Pagination hardening** — current ports return unbounded `Vec`s in some paths; evolve list methods to cursors before 100k scale, and add connection pooling (e.g. `deadpool-sqlite`) if write contention appears.
6. **Distributed rate limiting** — current limiter is per-process; a Redis token bucket would be needed for multi-replica deployments.
7. **Observability depth** — add `tracing` spans on service boundaries and an `/health/ready` dependency probe (DB ping + embedding ping) if uptime SLOs demand it.

None of these block the shipped slice; each has a clean seam already in place.

---

## 15. How to Continue

```bash
# from C:\Users\91767\Desktop\agents
cp .env.example .env   # set APP_JWT_SECRET to 32+ random bytes
cargo run              # → http://127.0.0.1:3000  (SQLite at ./data/matcher.db)

# run the full gate
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

# docker
docker build -t resume-job-matcher:local .
APP_JWT_SECRET=... docker compose up
curl http://127.0.0.1:3000/health/ready
```

Next recommended slice (smallest coherent, in order): (a) multipart upload with the file-security checklist above, behind a `DocumentParser` trait; (b) feature-flag the ONNX embedding provider and record model/dimension alongside each stored embedding; (c) introduce a connection pool once profiling shows `Mutex<Connection>` contention. Keep each behind the existing traits and add the corresponding integration fixtures.

---

## 16. File Map (entry points)

- Composition: `src/lib.rs:build_application` — start here.
- Config: `src/config.rs:AppConfig`
- Domain contracts: `src/domain/repositories.rs`, `src/domain/ports.rs`, `src/domain/entities.rs`
- Scoring: `src/domain/similarity.rs:build_weighted_recommendation`
- Auth: `src/application/auth.rs:AuthService`, `src/infrastructure/security.rs:SecurityService`
- HTTP: `src/interfaces/http/mod.rs:build_router`, `src/interfaces/http/routes.rs` (all handlers)
- Persistence: `src/infrastructure/sqlite.rs:SqliteRepositories` (durable), `src/infrastructure/memory.rs` (volatile)
- Tests: `tests/api.rs` (read this for the canonical flows)

---

## 17. Handoff Notes

- **Git:** the worktree's git root is `C:\Users\91767` (not `Desktop\agents`), so `git status` from `Desktop\agents` shows `?? Desktop/agents/`. Any commit should be done from the intended root with a narrow pathspec, or by initializing a repo inside `Desktop\agents` if the project is to be split out.
- **Secrets:** never commit `.env`; `APP_JWT_SECRET` and `APP_EMBEDDING_API_KEY` are the only secrets in play.
- **Windows:** the service, tests, and Docker build were all validated on Windows (`win32`, `pwsh`) at this handoff; SQLite `bundled` feature avoids needing a system `libsqlite3`.
- **Telemetry:** `~/.gstack/analytics` and `.pending-*` files are local-only; remote telemetry is opt-in.
