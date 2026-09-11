//! Register-based bytecode.
//!
//! Registers are slots in the thread's register stack, relative to the
//! current frame's base. Multi-value sequences (`nargs`/`nres`/`n` fields)
//! use Lua's encoding: `0` means "up to the thread's current top" (multret),
//! otherwise the field is `count + 1`.

use crate::value::Value;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    Len,
    BNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
}

/// Jump offsets are relative to the instruction *after* the jump.
#[derive(Clone, Copy, Debug)]
pub enum Instr {
    LoadK { dst: u8, k: u16 },
    /// `n` consecutive nils starting at `dst`.
    LoadNil { dst: u8, n: u8 },
    LoadBool { dst: u8, b: bool },
    Move { dst: u8, src: u8 },
    GetUpval { dst: u8, up: u8 },
    SetUpval { up: u8, src: u8 },
    GetIndex { dst: u8, obj: u8, key: u8 },
    /// Indexing with a constant key (globals, dot-fields).
    GetField { dst: u8, obj: u8, k: u16 },
    SetIndex { obj: u8, key: u8, src: u8 },
    SetField { obj: u8, k: u16, src: u8 },
    NewTable { dst: u8 },
    /// `obj[start+i] = R[base+i]` for i in 0..n-1 (or up to top if n == 0).
    SetList { obj: u8, base: u8, n: u8, start: u32 },
    Arith { op: ArithOp, dst: u8, lhs: u8, rhs: u8 },
    Unary { op: UnaryOp, dst: u8, src: u8 },
    /// Comparison producing a boolean in `dst`.
    Cmp { op: CmpOp, dst: u8, lhs: u8, rhs: u8 },
    /// Concatenates registers `base..base+n` into `dst`.
    Concat { dst: u8, base: u8, n: u8 },
    Jump { off: i32 },
    /// Jumps if `truthy(R[src]) == if_true`.
    Test { src: u8, if_true: bool, off: i32 },
    /// Callee at `R[base]`, args follow it.
    Call { base: u8, nargs: u8, nres: u8 },
    /// Proper tail call: like `Call` with an open result count, but the
    /// current frame is replaced instead of a new one being pushed, so
    /// unbounded tail recursion runs in constant frame depth.
    TailCall { base: u8, nargs: u8 },
    Return { base: u8, n: u8 },
    Vararg { dst: u8, n: u8 },
    Closure { dst: u8, p: u16 },
    /// Closes open upvalues at register `from` and above, and runs
    /// `__close` on to-be-closed variables at or above it.
    Close { from: u8 },
    /// Marks the register as a to-be-closed variable (`local x <close>`).
    Tbc { reg: u8 },
    /// Numeric for: `base` holds (counter, limit, step); `base+3` is the
    /// visible variable. ForPrep validates and jumps past ForLoop when the
    /// loop runs zero times; ForLoop steps and jumps back while in range.
    ForPrep { base: u8, off: i32 },
    ForLoop { base: u8, off: i32 },
    /// Generic for: `base` holds (func, state, control); vars start at
    /// `base+3`. If `R[base+3] ~= nil`, sets control and jumps back.
    TForLoop { base: u8, off: i32 },
}

/// How a closure captures each upvalue, relative to the enclosing function.
#[derive(Clone, Copy, Debug)]
pub enum UpvalDesc {
    /// Captures the enclosing function's register.
    Local(u8),
    /// Shares the enclosing function's upvalue.
    Upval(u8),
}

pub struct Proto {
    pub code: Vec<Instr>,
    /// Chunk name, used in error message position prefixes.
    pub source: std::rc::Rc<str>,
    /// Source line per instruction (parallel to `code`).
    pub lines: Vec<u32>,
    pub consts: Vec<Value>,
    pub protos: Vec<Rc<Proto>>,
    pub upvals: Vec<UpvalDesc>,
    pub nparams: u8,
    pub is_vararg: bool,
    pub max_regs: u8,
    /// For error messages.
    pub name: String,
}
