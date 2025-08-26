# System Patterns: Hobbes MVP Architecture

This document contains the high-level architecture for the Hobbes Minimum Viable Product (MVP).

```mermaid
graph TD
    subgraph User Interaction
        A[Global Hotkey] --> B{Hobbes App};
    end

    subgraph Hobbes Dioxus Application
        B -- Invokes --> C[Chat UI];
        B -- Triggers --> D[Context Service];
        C -- Sends/Receives Messages --> E[LLM Service];
        E -- Stores/Retrieves History --> F[Local Storage Service];
        D -- Provides Active Window Info --> E;
    end

    subgraph Platform Specific Services
        G[Hotkey Manager] -.-> A;
        H[Active Window Poller] -.-> D;
    end

    subgraph External Services
        I[LLM API]
    end

    E -- API Calls --> I;

    style B fill:#f9f,stroke:#333,stroke-width:2px
    style G fill:#bbf,stroke:#333,stroke-width:2px
    style H fill:#bbf,stroke:#333,stroke-width:2px