# Runtime Architecture Diagram

Solid lines show the currently compiled runtime. Dashed lines show the intended
NenDB path, which is not implemented or verified.

```mermaid
flowchart LR
    Client[API client] -->|HTTP and Bearer JWT| Router[Axum router and middleware]

    subgraph Process[resume-job-matcher process]
        Router --> Auth[AuthService]
        Router --> Ingest[ResumeJobService]
        Router --> Match[MatchingService]

        Auth --> UserPort[UserRepository port]
        Ingest --> ResumePort[ResumeRepository port]
        Ingest --> JobPort[JobRepository port]
        Match --> ResumePort
        Match --> JobPort

        Auth --> Security[Argon2id and HS256]
        Ingest --> Parsers[Deterministic parsers]
        Ingest --> EmbedPort[EmbeddingProvider port]
        Match --> Scorer[CosineSimilarity]

        UserPort --> Memory[In-memory repositories]
        ResumePort --> Memory
        JobPort --> Memory

        Router --> Metrics[Prometheus recorder]
        Router --> Logs[JSON tracing]
    end

    EmbedPort -->|optional HTTPS POST| Embeddings[OpenAI-compatible embedding API]

    UserPort -. intended adapter .-> NenAdapter[NenDB repository adapter]
    ResumePort -. intended adapter .-> NenAdapter
    JobPort -. intended adapter .-> NenAdapter
    NenAdapter -. protocol and capabilities to verify .-> NenDB[(NenDB)]

    classDef current fill:#e7f5ff,stroke:#1971c2,color:#102a43;
    classDef warning fill:#fff4e6,stroke:#d9480f,color:#4a1d0b;
    classDef intended fill:#f8f9fa,stroke:#868e96,stroke-dasharray:5 5,color:#343a40;
    class Router,Auth,Ingest,Match,UserPort,ResumePort,JobPort,Security,Parsers,EmbedPort,Scorer,Metrics,Logs current;
    class Memory warning;
    class NenAdapter,NenDB intended;
```

The current `Memory` node is process-local and loses all data on restart. The
dashed NenDB nodes describe an architectural seam only; the repository contains
no NenDB adapter, configuration, migrations, or verified runtime dependency.
