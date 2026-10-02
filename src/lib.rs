//! lintent: plain-language lint rules judged by Jev, scoped with tree-sitter.
//!
//! The pipeline: [`workspace`] finds files, [`extract`] cuts them into scopes
//! (any tree-sitter grammar, see [`languages`] / [`grammar`]), [`rules`]
//! decide which questions to ask, [`engine`] batches them into System One
//! requests ([`jev`]) with a per-question [`cache`], and [`report`] prints
//! the verdicts.

pub mod budget;
pub mod cache;
pub mod check;
pub mod cli;
pub mod config;
pub mod engine;
pub mod eval;
pub mod extract;
pub mod git;
pub mod grammar;
pub mod inspect;
pub mod jev;
pub mod keep;
pub mod languages;
pub mod report;
pub mod rules;
pub mod scaffold;
pub mod workspace;
