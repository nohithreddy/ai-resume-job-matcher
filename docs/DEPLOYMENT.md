# Deployment Guide

## Production readiness warning

The container and workflow artifacts package and test the current service. The
service defaults to **durable SQLite (WAL at `APP_DATABASE_PATH`)** and is
suitable for single-replica durable use; `APP_PERSISTENCE=memory` remains for
tests and explicitly ephemeral environments:

- With `memory`, all users, resumes, and jobs disappear on restart, upgrade,
  reschedule, or crash, and multiple replicas do not share data.
- `/health/ready` probes SQLite with `SELECT 1` under a 500 ms timeout (returns
  `503` when `sqlite` is configured and the probe fails) but does not probe the
  embedding endpoint.
- NenDB is an unverified external dependency. There is no verified driver,
  adapter, schema migration, transaction test, backup, or restore path here.
- For durable SQLite, mount a writable volume at the directory containing
  `APP_DATABASE_PATH` (Compose uses `matcher-data:/data`) and back up
  `matcher.db`/`matcher.db-wal` atomically.

Use the image for development or durable single-replica use until the full
multi-replica/persistence gate in this guide is complete.

## Container image

The multi-stage `Dockerfile`:

- Builds the locked release binary with Rust 1.88.0 on Debian Bookworm.
- Uses BuildKit caches without copying compiler state into the runtime image.
- Runs on `debian:bookworm-slim` with CA certificates and `curl` for health checks.
- Runs as unprivileged UID/GID `10001` with no login shell or home directory.
- Handles `SIGTERM` through the application's graceful shutdown path.
- Uses `/health/live`, not the incomplete readiness endpoint, for image health.

Build from the repository root:

```bash
docker build --pull --tag resume-job-matcher:0.1.0 .
```

Do not publish mutable production tags as the only rollback reference. Record the
image digest produced by CI and deploy immutable version or commit tags.

## Docker Compose

Prerequisites are Docker Engine with BuildKit and Docker Compose v2. Set a strong
JWT secret in the shell or an untracked `.env` file. The application checks for at
least 32 bytes; 32 random bytes represented as 64 hexadecimal characters is a
safe baseline.

```bash
export APP_JWT_SECRET="$(openssl rand -hex 32)"
docker compose config --quiet
docker compose up --build --detach
docker compose ps
```

PowerShell secret setup:

```powershell
$secret = New-Object byte[] 32
[Security.Cryptography.RandomNumberGenerator]::Fill($secret)
$env:APP_JWT_SECRET = [Convert]::ToHexString($secret)
docker compose config --quiet
docker compose up --build --detach
docker compose ps
```

`docker-compose.yml` requires `APP_JWT_SECRET`, binds host traffic to
`127.0.0.1:3000` by default, uses a read-only root filesystem, drops Linux
capabilities, blocks privilege escalation, limits process count, and rotates local
JSON logs. It does not start NenDB because no verified image, protocol, or adapter
exists for this project.

Check the service and logs:

```bash
curl --fail --silent --show-error http://127.0.0.1:3000/health/live
curl --fail --silent --show-error http://127.0.0.1:3000/health/ready
docker compose logs --follow matcher
```

Stop with the application's 30-second grace period:

```bash
docker compose down
```

The default host bind is intentionally local-only. Set `APP_HOST=0.0.0.0` only
behind a configured firewall and TLS reverse proxy. `APP_PORT` changes the host
port; the container always listens on port 3000 under Compose.

## Runtime configuration

| Variable | Required | Default | Notes |
| --- | --- | --- | --- |
| `APP_BIND_ADDRESS` | No | Native: `127.0.0.1:3000`; image: `0.0.0.0:3000` | Must be a socket address. Compose fixes the container value to `0.0.0.0:3000`. |
| `APP_LOG_FILTER` | No | `resume_job_matcher=info,tower_http=info` | `tracing_subscriber` environment-filter expression. Invalid filters prevent startup. |
| `APP_JWT_SECRET` | Yes | None | At least 32 bytes. Treat as a secret; rotating it invalidates all issued tokens. |
| `APP_JWT_TTL_SECONDS` | No | `3600` | Positive signed integer. |
| `APP_ARGON2_MEMORY_COST` | No | `19456` | KiB per Argon2 operation; minimum `8192`. Capacity-test concurrent login and registration. |
| `APP_EMBEDDING_ENDPOINT` | No | None | Exact OpenAI-compatible POST URL. Blank means deterministic local embeddings. |
| `APP_EMBEDDING_API_KEY` | No | None | Optional Bearer token for the embedding endpoint. |
| `APP_EMBEDDING_MODEL` | No | `text-embedding-3-small` | Sent to the HTTP embedding provider. It does not configure vector dimensions. |

Inject secrets through the target platform's secret manager. Do not place real
JWT, embedding, or future database credentials in an image layer, Compose file,
workflow, command transcript, or repository. Be aware that `docker compose config`
without `--quiet` renders resolved environment values and can expose secrets in
logs.

## Single-container run

For an ephemeral host, export the secret in the invoking environment so it is not
included as a literal command argument:

```bash
export APP_JWT_SECRET="$(openssl rand -hex 32)"
docker run --detach \
  --name resume-job-matcher \
  --publish 127.0.0.1:3000:3000 \
  --env APP_JWT_SECRET \
  --env APP_BIND_ADDRESS=0.0.0.0:3000 \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=16m \
  --cap-drop ALL \
  --security-opt no-new-privileges=true \
  --pids-limit 256 \
  resume-job-matcher:0.1.0
```

The process requires a writable data directory when `APP_PERSISTENCE=sqlite`
(default: `./data` locally, `/data` in the image). Mount `matcher-data:/data`
or an equivalent volume so SQLite WAL files survive restarts. With
`APP_PERSISTENCE=memory` the adapter never writes to the filesystem.

## Health and traffic management

| Endpoint | Current meaning | Appropriate use |
| --- | --- | --- |
| `/health/live` | The Axum process can answer a request. | Container restart/liveness check. |
| `/health/ready` | Reports `persistence`/`embeddings` labels; when `sqlite` is configured it probes SQLite with `SELECT 1` (500 ms timeout, `503` on failure). It does not probe the embedding endpoint. | Readiness gate for load balancers/orchestrators (use `503` to hold traffic). |

Readiness does not probe the embedding provider. For future dependencies (e.g.
NenDB), extend readiness with bounded, non-destructive checks and account for
embedding dependency policy without turning transient provider latency into an
uncontrolled probe storm.

The application listens on plain HTTP. Terminate TLS at a trusted ingress or
reverse proxy, preserve or generate a safe `X-Request-Id`, set request and idle
timeouts, limit body size to no more than the application's 512 KiB limit, and
restrict `/metrics` and OpenAPI if they should not be public.

## Observability

The binary writes JSON logs to standard output. Collect them with the platform's
log agent and avoid logging authorization headers or request bodies at the proxy.
Correlate requests by `X-Request-Id`.

Scrape `GET /metrics` as Prometheus text. Metrics are process-local and reset on
restart. Existing names include:

- `http_requests_total`
- `http_request_duration_seconds`
- `auth_registrations_total`
- `resumes_created_total`
- `jobs_created_total`
- `recommendation_requests_total`

Alerting thresholds require load-test baselines. In particular, Argon2 work is
CPU and memory intensive, and recommendations currently scan every resume for the
requesting user in process.

## CI and security automation

`.github/workflows/ci.yml` runs formatting, compilation, Clippy with warnings as
errors, tests, a locked release build, a container build, and live/ready smoke
requests. `.github/workflows/security.yml` runs RustSec audit plus Trivy repository,
secret, misconfiguration, and image scans on changes and weekly. Third-party
actions are pinned to full commit SHAs.

Treat scanner results as gates to investigate, not proof that an image is safe.
Base image updates still require rebuilding, rescanning, and controlled rollout.

## Persistence go-live gate

Do not call a deployment durable or production-ready until all items are complete:

1. Verify NenDB's authentic distribution, supported version, license, maintenance,
   protocol/driver, TLS, authentication, and production support policy.
2. Confirm its exact uniqueness, atomic write, consistency, transaction,
   pagination, vector, timestamp, and failure semantics.
3. Implement configuration-based adapter selection and the repository adapter
   described in [Data model and intended schema](DATA_MODEL.md).
4. Apply versioned collections and indexes without relying on undocumented DDL.
5. Pass adapter contract tests against a pinned NenDB test environment, including
   concurrent duplicate registration, timeouts, partial failures, and restart.
6. Replace static readiness with bounded dependency-aware checks.
7. Define and test migrations, backups, point-in-time expectations, and restore.
8. Store embedding model/version and dimensions, then test model changes and
   re-embedding without mixing incompatible vectors.
9. Add retention/deletion handling, encryption policy, audit controls, rate limits,
   and secret rotation.
10. Load-test one and multiple replicas only after shared persistence works, then
    set CPU, memory, connection, timeout, and autoscaling limits from evidence.

Until then, keep a single ephemeral replica and communicate that restarts erase
all application data.
