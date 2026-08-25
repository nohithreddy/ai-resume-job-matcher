# Logical Data Model ER Diagram

This diagram represents current Rust entities and the minimum intended persistent
relationships. It is not a NenDB physical schema and does not assert foreign-key,
type, or constraint support.

```mermaid
erDiagram
    USER ||--o{ RESUME : owns
    USER ||--o{ JOB : owns

    USER {
        uuid id PK
        string email UK
        string password_hash
        datetime created_at
    }

    RESUME {
        uuid id PK
        uuid user_id FK
        string title "nullable"
        text raw_text "sensitive"
        string_array skills
        float_array embedding "sensitive"
        datetime created_at
    }

    JOB {
        uuid id PK
        uuid owner_id FK
        string title
        text description "sensitive"
        string_array skills
        float_array embedding "sensitive"
        datetime created_at
    }
```

Recommendations are derived in memory from a job and resume and are not an
intended initial collection. `PK`, `UK`, and `FK` express logical requirements;
their NenDB implementation and enforcement must be verified.
