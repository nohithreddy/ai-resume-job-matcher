# Documentation

These documents describe version 0.1.0 of the Resume Job Matcher service as
implemented in this repository.

> **Production status:** the compiled service always constructs the in-memory
> repository adapter. Data is lost on restart and is not shared between replicas.
> NenDB is an unverified external dependency: this repository has no verified
> NenDB Rust driver, running adapter, migrations, or production readiness evidence.
> The Docker and CI artifacts are production-oriented, but they do not make the
> current persistence implementation production-ready.

## Guides

- [Architecture](ARCHITECTURE.md) explains boundaries, runtime flows, and known
  production gaps.
- [Data model and intended schema](DATA_MODEL.md) distinguishes current Rust
  entities from the proposed NenDB collections, indexes, and transaction rules.
- [API and developer guide](API_GUIDE.md) covers local development, HTTP
  behavior, request validation, examples, and extension points.
- [Deployment guide](DEPLOYMENT.md) covers the container image, Compose,
  configuration, operations, and the persistence go-live gate.

## Diagrams

- [Runtime architecture](diagrams/architecture.md)
- [Logical data model](diagrams/data-model-er.md)
- [Recommendation request sequence](diagrams/recommendation-sequence.md)

The generated OpenAPI document is available from a running service at
`GET /api/v1/openapi.json`. It is the machine-readable reference for the HTTP
routes included in OpenAPI; `/metrics` is intentionally documented separately.
