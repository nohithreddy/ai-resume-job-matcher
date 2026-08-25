# Recommendation Request Sequence

This is the currently implemented `GET /api/v1/jobs/{job_id}/recommendations`
flow. Repository calls resolve to in-memory adapters in the compiled service.

```mermaid
sequenceDiagram
    autonumber
    actor Client
    participant HTTP as Axum HTTP layer
    participant Auth as AuthService
    participant Token as TokenService
    participant Users as UserRepository
    participant Match as MatchingService
    participant Jobs as JobRepository
    participant Resumes as ResumeRepository
    participant Score as CosineSimilarity

    Client->>HTTP: GET recommendations with Bearer JWT
    HTTP->>HTTP: Validate or generate X-Request-Id
    HTTP->>Auth: authenticate(token)
    Auth->>Token: decode_user_id(token)
    Token-->>Auth: user_id
    Auth->>Users: find_by_id(user_id)
    Users-->>Auth: user or none

    alt Token invalid or user absent
        Auth-->>HTTP: Unauthorized
        HTTP-->>Client: 401 problem+json
    else Authenticated
        Auth-->>HTTP: user_id
        HTTP->>HTTP: Parse UUID and validate limit 1..100
        HTTP->>Match: recommendations_for_job(job_id, user_id, limit)
        Match->>Jobs: find_by_id(job_id)
        Jobs-->>Match: job or none

        alt Job absent
            Match-->>HTTP: NotFound
            HTTP-->>Client: 404 problem+json
        else Caller does not own job
            Match-->>HTTP: Forbidden
            HTTP-->>Client: 403 problem+json
        else Caller owns job
            Match->>Resumes: list_by_user(user_id)
            Resumes-->>Match: all caller resumes
            loop Each resume
                Match->>Score: similarity(job vector, resume vector)
                Score-->>Match: cosine score
            end
            Match->>Match: Build skill differences, sort, and truncate
            Match-->>HTTP: recommendations
            HTTP-->>Client: 200 JSON and X-Request-Id
        end
    end
```

No embedding provider call occurs during recommendation reads because vectors are
created during ingestion. A future NenDB adapter should preserve this repository
contract unless a separately designed vector-search port replaces in-process
ranking.
