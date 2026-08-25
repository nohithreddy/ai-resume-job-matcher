# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e

FROM rust:1.88.0-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS builder

WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/workspace/target,sharing=locked \
    cargo build --locked --release --bin resume-job-matcher \
    && install -D -m 0755 target/release/resume-job-matcher /out/resume-job-matcher

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS runtime

LABEL org.opencontainers.image.title="Resume Job Matcher" \
      org.opencontainers.image.description="Axum service for authenticated resume and job matching" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 app \
    && useradd --system --uid 10001 --gid app --no-create-home \
        --home-dir /nonexistent --shell /usr/sbin/nologin app \
    && install -d -o app -g app -m 0750 /data \
    && install -d -o app -g app -m 0750 /data/uploads

COPY --from=builder --chown=app:app /out/resume-job-matcher /usr/local/bin/resume-job-matcher

ENV APP_BIND_ADDRESS=0.0.0.0:3000 \
    APP_LOG_FILTER=resume_job_matcher=info,tower_http=info \
    APP_JWT_TTL_SECONDS=3600 \
    APP_ARGON2_MEMORY_COST=19456 \
    APP_AUTH_RATE_LIMIT_WINDOW_SECONDS=60 \
    APP_AUTH_RATE_LIMIT_MAX_REQUESTS=10 \
    APP_EMBEDDING_MODEL=text-embedding-3-small \
    APP_PERSISTENCE=sqlite \
    APP_DATABASE_PATH=/data/matcher.db

USER app:app

EXPOSE 3000
STOPSIGNAL SIGTERM

# Health uses liveness; /health/ready reports backend status without probing.
HEALTHCHECK --interval=30s --timeout=3s --retries=3 CMD curl --fail --silent --show-error --max-time 2 http://127.0.0.1:3000/health/live

ENTRYPOINT ["/usr/local/bin/resume-job-matcher"]
