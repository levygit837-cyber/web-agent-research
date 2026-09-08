//! Núcleo do agente de pesquisa (Pesquisa → Sessão → Turno → Evidência).
//!
//! O binário em `src/main.rs` é só casca fina de CLI; toda lógica de
//! domínio mora aqui para a futura API HTTP (Axum) reutilizar sem rewrite.

/// Versão do formato JSONL de Sessão. Quebrar compatibilidade exige bump + migração.
pub const SESSION_FORMAT_VERSION: u32 = 1;

pub mod shared;
pub mod slices;
