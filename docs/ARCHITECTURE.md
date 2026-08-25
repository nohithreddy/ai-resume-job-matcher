# Architecture

## Status and scope

Resume Job Matcher is a Rust 2024 Axum service for registration, login, resume
and job ingestion, and cosine-similarity recommendations. The code follows a
ports-and-adapters structure so domain and application logic do not depend on a
specific database or embedding vendor.

The service ships as a durable SQLite-backed service by default.
`build_application` resolves the repository backend via `RepositorySet::open()`
(`sqlite` WAL at `APP_DATABASE_PATH` by default; `memory` opt-in for tests
via `APP_PERSISTENCE=memory`) and surfaces the selected label in
`/health/ready`. NenDB is not linked, configured, or contacted by the compiled
service. NenDB's driver, API, durability, indexing, transaction, security,
backup, and operational properties have not been verified for this project.

See the [runtime architecture diagram](diagrams/architecture.md).

## Module boundaries

| Layer | Location | Responsibility |
| --- | --- | --- |
| Binary | `src/main.rs` | Load configuration, initialize JSON tracing, bind the listener, and handle graceful shutdown. |
| HTTP interface | `src/interfaces/http/` | Route requests, extract authentication, validate transport shapes, map errors, publish OpenAPI and metrics, and add middleware. |
| Application | `src/application/` | Orchestrate authentication, ingestion, and recommendation use cases. |
| Domain | `src/domain/` | Define entities, repository and service ports, domain errors, and similarity behavior. |
| Infrastructure | `src/infrastructure/` | Supply SQLite (WAL) and in-memory repositories, Argon2id/JWT security, deterministic parsers, and local or HTTP embeddings. |

Dependencies point inward: HTTP and infrastructure depend on application/domain
contracts, while the domain contains no Axum, NenDB, or HTTP embedding types.

## Runtime assembly

`build_application` resolves `RepositorySet` via `infrastructure::open_repositories`
(SQLite by default, in-memory when `APP_PERSISTENCE=memory`) and shares the
repository bundle with the application services:

- `AuthService` uses `UserRepository`, `PasswordService`, and `TokenService`.
- `ResumeJobService` uses resume/job repositories, deterministic parsers, and one
  `EmbeddingProvider`.
- `MatchingService` uses resume/job repositories and `CosineSimilarity`.
- The embedding provider is HTTP-backed only when `APP_EMBEDDING_ENDPOINT` is
  non-empty. Otherwise a deterministic 64-dimensional local vectorizer is used.

The deterministic parser recognizes a fixed skill vocabulary. The deterministic
embedding implementation is stable for tests and offline development, but it is
not a semantic model and should not be treated as a production matching system.

## Request lifecycle

Every request passes through these outer concerns before reaching a handler:

1. Request ID middleware accepts a safe `X-Request-Id` or generates a UUID v7,
   then returns it on the response.
2. Request metrics record count, status, route, method, and duration.
3. HTTP tracing emits structured events through the JSON tracing subscriber.
4. Panic handling converts handler panics into a generic RFC 7807-style response.
5. Axum enforces a 512 KiB body limit and dispatches to a route or fallback.

Protected routes extract a Bearer JWT, decode its HS256 subject, and verify that
the referenced user still exists through `UserRepository`. There is no refresh
token, revocation list, role model, or rate limiter.

### Ingestion

Resume and job ingestion validates input, extracts known skills, requests an
embedding, builds a UUID v7 entity with a UTC timestamp, and writes the complete
entity through one repository call. Raw resume text, job descriptions, password
hashes, and vectors are deliberately omitted from HTTP response serialization.

### Recommendations

The service loads the requested job, verifies that the requester owns it, loads
all resumes owned by that user, computes cosine similarity in process, sorts by
descending score with resume UUID as a deterministic tie-breaker, and truncates
to the requested limit. Recommendations are derived responses and are not stored.

See the [recommendation sequence diagram](diagrams/recommendation-sequence.md).

## Persistence boundary

The domain exposes three asynchronous, `Send + Sync` repository ports:

- `UserRepository`: create and lookup by email or ID.
- `ResumeRepository`: create, lookup by ID, and list by user.
- `JobRepository`: create, lookup by ID, and list all jobs.

Two adapters exist: `SqliteRepositories` (default, WAL, `busy_timeout 5s`,
versioned `PRAGMA user_version = 1` migration) and `InMemoryRepositories`
(volatile `HashMap` + `RwLock`, retained for tests). When running with
`APP_PERSISTENCE=memory`, the in-memory adapter has several operational
consequences:

- Every restart, replacement, or crash removes all users, jobs, and resumes.
- JWTs surviving a restart fail authentication because their users no longer exist.
- Multiple replicas have isolated datasets and issue tokens for users unknown to
  the other replicas. Load balancing is therefore unsafe even with sticky sessions.
- There are no migrations, backups, restore procedures, cross-process constraints,
  or durable transaction guarantees (SQLite mode provides WAL durability and migrations).

The intended NenDB adapter must remain behind the repository ports. It must not
leak NenDB handles, query types, or error types into application/domain modules.
The logical collections, indexes, atomicity rules, and error mapping are specified
in [Data model and intended schema](DATA_MODEL.md). They are a design contract,
not evidence that NenDB supports or currently implements those features.

## External embedding boundary

When configured, `HttpEmbeddingProvider` sends an OpenAI-compatible JSON request
to the exact URL in `APP_EMBEDDING_ENDPOINT`:

```json
{
  "model": "configured-model",
  "input": "text to embed"
}
```

An optional API key is sent as a Bearer token. The HTTP client uses a 3-second
connect timeout and a 10-second total timeout. A successful response must contain
a non-empty finite vector at `data[0].embedding`. The service does not currently
retry, rate-limit, cache, batch, or circuit-break embedding calls.

The embedding model and dimensions are not stored separately from each entity.
Changing providers or models while old data exists can make vectors incompatible
and turn recommendation requests into internal errors. A persistent adapter needs
an explicit model/version and dimensions strategy before migration.

## Observability

- Logs are JSON on standard output and controlled by `APP_LOG_FILTER`.
- `/metrics` exports Prometheus text. HTTP counters and duration histograms use
  bounded method, matched-route, and status labels.
- Domain counters include registrations, resumes, jobs, and recommendation
  requests. They are process-local and reset on restart.
- `/health/live` only proves the process can answer HTTP (liveness).
- `/health/ready` reports `persistence`/`embeddings` labels and, when
  `APP_PERSISTENCE=sqlite`, probes SQLite with `SELECT 1` under a 500 ms timeout
  (via `spawn_blocking`); on failure it returns `503` `ProblemDetails`
  (`dependency-unavailable`). In-memory mode reports readiness without a storage
  probe. The endpoint does not probe the embedding provider.

## Security boundaries

- Passwords are hashed with Argon2id in blocking worker tasks. Default memory cost
  is 19,456 KiB per operation, with two iterations and one lane.
- JWTs are HS256 and use one shared secret of at least 32 bytes. Secret rotation,
  key IDs, refresh tokens, and revocation are not implemented.
- Authorization currently consists of user existence and job ownership checks.
- Resume text and job descriptions are sensitive data even though responses hide
  them. A persistent adapter must encrypt transport, restrict database access,
  define retention/deletion, and protect backups.
- Browser CORS policy and application-level rate limiting are not configured.
  Deployments must supply appropriate edge controls or add reviewed application
  behavior before exposing the service publicly.

## Scaling and production gates

Do not run more than one instance or promise durability with the current adapter.
Before production traffic, all of the following need evidence:

- A verified NenDB distribution and supported integration mechanism.
- A repository adapter passing contract and failure-injection tests.
- Applied and versioned collections/indexes with uniqueness and atomicity checks.
- Dependency-aware readiness, migrations, backup, and tested restore procedures.
- Embedding model/version compatibility and a re-embedding plan.
- TLS, secret rotation, rate limits, abuse controls, and data lifecycle controls.
- Load tests covering concurrent Argon2 work, ingestion, and recommendation scans.

Container hardening and CI reduce packaging risk, but they do not satisfy these
application and data durability gates.
