//! suslua — a suspendable Lua 5.4 interpreter.
//!
//! Scripts are compiled once and then driven by the embedder with explicit
//! fuel budgets: `Execution::step(fuel)` runs at most `fuel` units of work
//! and suspends, resumable later. See DESIGN.md.

pub mod ast;
pub mod lexer;
pub mod parser;
