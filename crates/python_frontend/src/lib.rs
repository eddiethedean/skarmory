//! Python-specific frontend boundary.
//!
//! Phase 0 will select and pin a matching Ruff parser/AST release behind a
//! project-owned adapter. No parser or semantic extraction is implemented yet.
//! See `docs/architecture.md` and `docs/roadmap.md`.

pub const LANGUAGE: &str = "Python";
