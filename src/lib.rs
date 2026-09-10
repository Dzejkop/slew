//! slew — a suspendable Lua 5.4 interpreter.
//!
//! Scripts are compiled once and then driven by the embedder with explicit
//! fuel budgets: `Execution::step(fuel)` runs at most `fuel` units of work
//! and suspends, resumable later. See DESIGN.md.

pub mod ast;
pub mod bytecode;
pub mod compiler;
pub mod lexer;
pub mod parser;
pub mod pattern;
pub mod stdlib;
pub mod value;
pub mod vm;

pub use value::Value;
pub use vm::{Chunk, Error, Execution, Lua, NativeFn, RuntimeError, Step};
