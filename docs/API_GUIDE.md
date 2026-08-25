# API Guide

Reference for the shipped HTTP API of `resume-job-matcher`. This document tracks
the code in `src/interfaces/http/routes.rs` and `src/interfaces/http/mod.rs`; if
they disagree, the code wins.

## Service status

The API is a local vertical slice. Every user, session, resume, job,
application, and match lives in process memory, so all data disappears on
restart and even unexpired tokens stop working after a restart. The local
embedding provider is deterministic rather than semantic.

## Run locally

Prerequisites: Rust 1.88 or newer and a JWT secret of at least 32 bytes.

```bash
export APP_JWT_SECRET="$(openssl rand -hex 32)"
cargo run --locked
```

Default bind address is `127.0.0.1:3000`. Configuration is read from process
environment variables only; see [`.env.example`](../.env.example) and
[Deployment](DEPLOYMENT.md) for the full reference.

## HTTP conventions

- Base URL: `http://127.0.0.1:3000`.
- JSON request bodies require `Content-Type: application/json`.
- Protected routes require `Authorization: Bearer <jwt>`.
- Every response carries `X-Request-Id`. A caller-supplied value is preserved
  only when it is 1–128 characters of ASCII letters, digits, `-`, `_`, or `.`;
  otherwise a UUID v7 is generated.
- Request bodies are limited to 512 KiB (`413` beyond that).
- Errors use RFC 7807 `application/problem+json` (see [Problems](#problems)).
- The generated OpenAPI document is served at `GET /api/v1/openapi.json`.
  `/metrics` and `/health/*` handlers are not listed as OpenAPI paths except
  where noted by the generator.

## Rate limiting (authentication endpoints)

`POST /api/v1/auth/register`, `/login`, `/refresh`, and `/logout` sit behind an
in-memory sliding-window limiter keyed by client IP:

- Defaults: **10 requests per 60 seconds** per IP across those four routes.
- Configure with `APP_AUTH_RATE_LIMIT_WINDOW_SECONDS` (default `60`) and
  `APP_AUTH_RATE_LIMIT_MAX_REQUESTS` (default `10`). Both must be greater than
  zero or startup fails.
- The client address comes from the socket peer (`ConnectInfo<SocketAddr>`).
  Proxy headers such as `X-Forwarded-For` are not honored. When no connection
  information exists (for example in tests that drive the router directly),
  requests share one fallback bucket.
- Exceeding the window returns `429 Too Many Requests` with
  `Content-Type: application/problem+json` and a `Retry-After` header giving
  whole seconds until the oldest hit leaves the window. Each rejection
  increments the Prometheus counter `auth_rate_limited_total`.
- Counters are per application instance and reset on restart. All other
  endpoints (health, metrics, jobs listing, authenticated routes) are never
  throttled by this middleware.

## Roles

Registration accepts `role: "candidate"` (the default when omitted) or
`"recruiter"`. Requesting `"admin"` publicly fails with `400`.

| Capability | Candidate | Recruiter | Admin |
| --- | --- | --- | --- |
| Create/list/get/update/delete own resumes | yes | no | yes |
| Apply own resumes to jobs | yes | no | yes |
| Publish jobs | no | yes | yes |
| List applicants and rank them for owned jobs | no | yes | yes |
| Compute matches and read reports as candidate or recruiter party | yes | yes | yes |

Admins hold both capability sets (`can_manage_resumes` and `can_manage_jobs`)
but cannot be created through the public API.

## Authentication

### Register

`POST /api/v1/auth/register` — `201 Created`

```json
{
  "email": "engineer@example.com",
  "password": "correct horse battery staple",
  "role": "candidate"
}
```

- `email` must be a valid address; it is trimmed and lowercased before storage.
- `password` length must be 12–128.
- Duplicate normalized emails return `409`; invalid input returns `400`.

Success body (`AuthResponse`):

```json
{
  "user_id": "0198f3d5-68c0-7b8b-a3df-8c2316c41f1f",
  "role": "candidate",
  "access_token": "<jwt>",
  "refresh_token": "<64-char opaque string>",
  "token_type": "Bearer",
  "expires_in": 900,
  "refresh_expires_in": 2592000,
  "session_id": "0198f3d5-68c0-7b8b-a3df-8c2316c41f20"
}
```

Access tokens are HS256 JWTs whose subject is `user_id`; their lifetime is
capped at 15 minutes regardless of `APP_JWT_TTL_SECONDS`. Refresh tokens are
opaque, stored only as keyed Argon2id verifiers, valid for 30 days, and rotated
on every use.

### Login

`POST /api/v1/auth/login` — `200 OK`

```json
{ "email": "engineer@example.com", "password": "correct horse battery staple" }
```

Password length must be 1–128. Unknown users and wrong passwords return the
same `401` problem response; timing is equalized against a dummy hash. A fresh
session and refresh token are issued on each successful login.

### Refresh

`POST /api/v1/auth/refresh` — `200 OK`

```json
{ "refresh_token": "<current refresh token>" }
```

Returns a new `AuthResponse`. The used token is retired atomically; replaying a
retired token revokes the entire session family, so every subsequent refresh in
that family returns `401`. An empty or over-long (`>256` char after trimming)
token returns `400`.

### Logout

`POST /api/v1/auth/logout` — `204 No Content`

```json
{ "refresh_token": "<current refresh token>" }
```

Revokes the owning session. Unknown or already-revoked tokens return `401`;
validation failures return `400`.

## Health and operations

- `GET /health/live` → `{"status":"ok"}`
- `GET /health/ready` →

  ```json
  {
    "status": "ready",
    "persistence": "in-memory",
    "embeddings": "deterministic-local"
  }
  ```

  `embeddings` is `"http"` when `APP_EMBEDDING_ENDPOINT` is set. The handler
  does not probe dependencies, so `200` is not proof they work.
- `GET /metrics` — Prometheus text exposition
  (`text/plain; version=0.0.4`). Includes `http_requests_total`,
  `http_request_duration_seconds`, `auth_registrations_total`,
  `auth_rate_limited_total`, `resumes_created_total`, `jobs_created_total`,
  `applications_created_total`, `recommendation_requests_total`, and
  `matches_created_total`. Restrict at the deployment edge.
- `GET /api/v1/openapi.json` — runtime-generated OpenAPI document.

## Resumes (candidate or admin)

`raw_text` must be 20–100,000 characters; `title` is optional and up to 200.
Skills are extracted deterministically and lowercased. Responses never include
raw text or embeddings. When `title` is omitted, the parser uses the first
non-empty line of `raw_text`.

| Route | Success | Notes |
| --- | --- | --- |
| `POST /api/v1/resumes` | `201` | Body `{"title": "...", "raw_text": "..."}` |
| `GET /api/v1/resumes?offset=0&limit=25` | `200` | Own resumes, newest first |
| `GET /api/v1/resumes/{resume_id}` | `200` | Owner only (`403` otherwise, `404` if missing) |
| `PUT /api/v1/resumes/{resume_id}` | `200` | Owner only; same body as create |
| `DELETE /api/v1/resumes/{resume_id}` | `204` | Owner only |

`ResumeResponse`:

```json
{
  "id": "0198f3d7-9bd7-7c2e-9dfa-af2331f7cb3c",
  "user_id": "0198f3d5-68c0-7b8b-a3df-8c2316c41f1f",
  "title": "Backend Engineer",
  "skills": ["aws", "axum", "docker", "rest", "rust", "sql", "tokio"],
  "created_at": "2026-08-24T12:00:00Z"
}
```

List responses wrap items with pagination echo:

```json
{ "items": [], "offset": 0, "limit": 25 }
```

Pagination: `limit` must be 1–100 (default `25`); out-of-range values return
`400`.

## Jobs

- `GET /api/v1/jobs?offset=0&limit=25` — public job board, no auth.
- `POST /api/v1/jobs` — recruiter (or admin). `title` must be 2–200,
  `description` 20–100,000. Skills are parsed from both fields; the embedding
  is computed from the description.

`JobResponse` (description and embedding excluded):

```json
{
  "id": "0198f3d8-e3bd-711c-8815-019d9b4c1f23",
  "owner_id": "0198f3d5-68c0-7b8b-a3df-8c2316c41f99",
  "title": "Rust Platform Engineer",
  "skills": ["kubernetes", "rest", "rust", "sql", "tokio"],
  "created_at": "2026-08-24T12:05:00Z"
}
```

## Applications

- `POST /api/v1/applications` — candidate (or admin):

  ```json
  { "resume_id": "…", "job_id": "…" }
  ```

  The resume must belong to the caller (`403` otherwise). Applying to your own
  job returns `400`; a duplicate application returns `409`; missing resources
  return `404`.

- `GET /api/v1/jobs/{job_id}/applications?offset=0&limit=25` — job owner only
  (`403` otherwise, `404` if the job does not exist).

`ApplicationResponse`:

```json
{
  "id": "0198f3da-1c5e-7a44-8f21-b7c9e2d4a610",
  "candidate_id": "0198f3d5-68c0-7b8b-a3df-8c2316c41f1f",
  "resume_id": "0198f3d7-9bd7-7c2e-9dfa-af2331f7cb3c",
  "job_id": "0198f3d8-e3bd-711c-8815-019d9b4c1f23",
  "status": "submitted",
  "created_at": "2026-08-24T12:06:00Z"
}
```

`status` is one of `"submitted"` or `"withdrawn"`.

## Recommendations

`GET /api/v1/jobs/{job_id}/recommendations?limit=10` — job owner only. Ranks
**submitted applications** for the job (not arbitrary resumes) by weighted ATS
score, descending. `limit` must be 1–100 and defaults to `10`. Missing job →
`404`; non-owner → `403`.

Each item is a full recommendation report:

```json
{
  "items": [
    {
      "job_id": "0198f3d8-e3bd-711c-8815-019d9b4c1f23",
      "resume_id": "0198f3d7-9bd7-7c2e-9dfa-af2331f7cb3c",
      "score": 87.21,
      "matched_skills": ["axum", "docker", "rest", "rust", "sql", "tokio"],
      "missing_skills": ["kubernetes"],
      "category_scores": {
        "skills": { "score": 85.0, "weight": 40, "weighted_score": 34.0, "reasons": ["..."] },
        "experience": { "score": 80.0, "weight": 20, "weighted_score": 16.0, "reasons": [] },
        "education": { "score": 50.0, "weight": 10, "weighted_score": 5.0, "reasons": [] },
        "semantic_similarity": { "score": 78.0, "weight": 20, "weighted_score": 15.6, "reasons": [] },
        "certifications": { "score": 0.0, "weight": 5, "weighted_score": 0.0, "reasons": [] },
        "keywords": { "score": 70.0, "weight": 5, "weighted_score": 3.5, "reasons": [] }
      },
      "reasons": ["Strong skill overlap ..."],
      "recommendations": ["Add Kubernetes exposure ..."],
      "comparisons": {
        "role": {
          "resume_value": "backend engineer",
          "job_value": "platform engineer",
          "outcome": "partial",
          "reason": "..."
        },
        "location": { "resume_value": null, "job_value": null, "outcome": "unknown", "reason": "..." },
        "availability": { "resume_value": null, "job_value": null, "outcome": "unknown", "reason": "..." }
      }
    }
  ]
}
```

## Scoring weights

The ATS `score` is a weighted 0–100 composite. Each category reports its raw
fit (`score`, 0–100), its fixed `weight` in percentage points, and the points
it actually contributed (`weighted_score = score × weight ÷ 100`):

| Category | Weight |
| --- | --- |
| `skills` | 40 |
| `experience` | 20 |
| `semantic_similarity` | 20 |
| `education` | 10 |
| `certifications` | 5 |
| `keywords` | 5 |

Role, location, and availability `comparisons` are explanatory only and never
change the score. `outcome` is one of `"match"`, `"partial"`, `"mismatch"`,
`"unknown"`, or `"not_specified"`. Every report also includes human-readable
`reasons` and improvement `recommendations`.

## Matches and reports

- `POST /api/v1/matches` — computes and persists a report:

  ```json
  { "resume_id": "…", "job_id": "…" }
  ```

  Allowed when the caller owns the resume (candidate side) **or** owns the job
  (recruiter side; then the candidate must have a submitted application for it).
  Otherwise `403`; missing resources `404`. Returns `201`.

- `GET /api/v1/matches?offset=0&limit=25` — persisted matches where the caller
  is the candidate or the recruiter.

- `GET /api/v1/reports/{match_id}` — a single persisted report; only the
  candidate or recruiter of that match may read it (`403` otherwise, `404` if
  missing).

`MatchResultResponse`:

```json
{
  "id": "0198f3db-9a2f-7d31-b4c0-5f8e1a2c3d40",
  "resume_id": "0198f3d7-9bd7-7c2e-9dfa-af2331f7cb3c",
  "job_id": "0198f3d8-e3bd-711c-8815-019d9b4c1f23",
  "candidate_id": "0198f3d5-68c0-7b8b-a3df-8c2316c41f1f",
  "recruiter_id": "0198f3d5-68c0-7b8b-a3df-8c2316c41f99",
  "requested_by": "0198f3d5-68c0-7b8b-a3df-8c2316c41f1f",
  "report": { "…same shape as a recommendation item above…": "" },
  "created_at": "2026-08-24T12:07:00Z"
}
```

## Problems (RFC 7807)

Every error uses `Content-Type: application/problem+json` with `type`, `title`,
`status`, and `detail` fields:

```http
HTTP/1.1 429 Too Many Requests
Content-Type: application/problem+json
Retry-After: 42
X-Request-Id: 0198f3dc-4ce8-7ccf-bb70-3f2f1adf7f18

{
  "type": "https://resume-matcher.example/problems/too-many-requests",
  "title": "Too Many Requests",
  "status": 429,
  "detail": "Too many requests from this address. Retry after the indicated delay."
}
```

Observed status codes: `400` invalid input/path/query/JSON, `401` missing or
invalid credentials (with `WWW-Authenticate: Bearer`), `403` role or ownership
failure, `404` missing resource or route, `405` unsupported method, `409`
duplicate user/application, `413` body too large, `415` wrong media type,
`422` JSON shape failure, `429` auth rate limit exceeded (with `Retry-After`),
`500` internal consistency failure, `503` embedding dependency unavailable.
Details intentionally hide internal provider and storage messages.

## End-to-end example

Requires `curl` and `jq`, against a locally running server:

```bash
BASE_URL=http://127.0.0.1:3000

# 1. Register a candidate and a recruiter.
CANDIDATE=$(curl --fail --silent --show-error \
  --request POST "$BASE_URL/api/v1/auth/register" \
  --header 'Content-Type: application/json' \
  --data '{"email":"engineer@example.com","password":"correct horse battery staple","role":"candidate"}')
CTOKEN="$(printf '%s' "$CANDIDATE" | jq --raw-output '.access_token')"

RECRUITER=$(curl --fail --silent --show-error \
  --request POST "$BASE_URL/api/v1/auth/register" \
  --header 'Content-Type: application/json' \
  --data '{"email":"hiring@example.com","password":"correct horse battery staple","role":"recruiter"}')
RTOKEN="$(printf '%s' "$RECRUITER" | jq --raw-output '.access_token')"

# 2. Upload a resume as the candidate.
RESUME=$(curl --fail --silent --show-error \
  --request POST "$BASE_URL/api/v1/resumes" \
  --header "Authorization: Bearer $CTOKEN" \
  --header 'Content-Type: application/json' \
  --data '{"title":"Backend Engineer","raw_text":"Built production REST services with Rust, Axum, Tokio, SQL, Docker, and AWS."}')
RESUME_ID="$(printf '%s' "$RESUME" | jq --raw-output '.id')"

# 3. Publish a job as the recruiter.
JOB=$(curl --fail --silent --show-error \
  --request POST "$BASE_URL/api/v1/jobs" \
  --header "Authorization: Bearer $RTOKEN" \
  --header 'Content-Type: application/json' \
  --data '{"title":"Rust Platform Engineer","description":"Build REST services with Rust, Axum, Tokio, SQL, Docker, and Kubernetes. Location: Remote."}')
JOB_ID="$(printf '%s' "$JOB" | jq --raw-output '.id')"

# 4. Browse the public board, then apply as the candidate.
curl --fail --silent "$BASE_URL/api/v1/jobs?limit=10" | jq

curl --fail --silent --show-error \
  --request POST "$BASE_URL/api/v1/applications" \
  --header "Authorization: Bearer $CTOKEN" \
  --header 'Content-Type: application/json' \
  --data "{\"resume_id\":\"$RESUME_ID\",\"job_id\":\"$JOB_ID\"}" | jq

# 5. Rank submitted applicants as the recruiter.
curl --fail --silent --show-error \
  --header "Authorization: Bearer $RTOKEN" \
  "$BASE_URL/api/v1/jobs/$JOB_ID/recommendations?limit=5" | jq

# 6. Persist a match and fetch the explainable report.
MATCH=$(curl --fail --silent --show-error \
  --request POST "$BASE_URL/api/v1/matches" \
  --header "Authorization: Bearer $CTOKEN" \
  --header 'Content-Type: application/json' \
  --data "{\"resume_id\":\"$RESUME_ID\",\"job_id\":\"$JOB_ID\"}")
MATCH_ID="$(printf '%s' "$MATCH" | jq --raw-output '.id')"

curl --fail --silent --show-error \
  --header "Authorization: Bearer $CTOKEN" \
  "$BASE_URL/api/v1/reports/$MATCH_ID" | jq

# 7. Rotate the refresh token, then log out.
REFRESH_TOKEN="$(printf '%s' "$CANDIDATE" | jq --raw-output '.refresh_token')"

ROTATED=$(curl --fail --silent --show-error \
  --request POST "$BASE_URL/api/v1/auth/refresh" \
  --header 'Content-Type: application/json' \
  --data "{\"refresh_token\":\"$REFRESH_TOKEN\"}")
NEW_REFRESH="$(printf '%s' "$ROTATED" | jq --raw-output '.refresh_token')"

# Replaying the old token would revoke the session family (401).

curl --fail --silent --show-error \
  --request POST "$BASE_URL/api/v1/auth/logout" \
  --header 'Content-Type: application/json' \
  --data "{\"refresh_token\":\"$NEW_REFRESH\"}" \
  --output /dev/null --write-out '%{http_code}\n'

# 8. Watch the rate limiter trip (expect HTTP 429 after the configured burst).
for _ in $(seq 1 15); do
  curl --silent --output /dev/null --write-out '%{http_code}\n' \
    --request POST "$BASE_URL/api/v1/auth/login" \
    --header 'Content-Type: application/json' \
    --data '{"email":"engineer@example.com","password":"wrong-password-only"}'
done
```

## Embedding provider contract

With no endpoint configured, the local provider produces deterministic
64-value vectors and makes no network call; it exists for development and
tests. With `APP_EMBEDDING_ENDPOINT`, the service POSTs `{ "model", "input" }`
to that URL and expects an OpenAI-compatible response with a finite, non-empty
`data[0].embedding`. `APP_EMBEDDING_API_KEY`, when non-empty, is sent as a
Bearer token. Connect timeout 3 s, total timeout 10 s, no retries. Vectors
combined in one comparison must share dimensions.
