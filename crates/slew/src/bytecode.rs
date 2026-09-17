//! Register-based bytecode.
//!
//! Registers are slots in the thread's register stack, relative to the
//! current frame's base. Multi-value sequences (`nargs`/`nres`/`n` fields)
//! use Lua's encoding: `0` means "up to the thread's current top" (multret),
//! otherwise the field is `count + 1`.

use crate::value::Value;
use std::rc::Rc;

/// `Close` register value for a forward `goto` that needed no scope close.
/// It is only re-patched when the jump actually leaves captured or
/// to-be-closed locals, so the VM (and the binary-chunk reader) must treat it
/// as a no-op rather than as a register number.
pub const UNPATCHED_CLOSE: u8 = 255;

/// Register ceiling per function frame. The compiler refuses to allocate
/// beyond it, and the binary-chunk reader rejects protos that claim more.
pub const MAX_REGS: u8 = 250;

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
    LoadK {
        dst: u8,
        k: u16,
    },
    /// `n` consecutive nils starting at `dst`.
    LoadNil {
        dst: u8,
        n: u8,
    },
    LoadBool {
        dst: u8,
        b: bool,
    },
    Move {
        dst: u8,
        src: u8,
    },
    GetUpval {
        dst: u8,
        up: u8,
    },
    SetUpval {
        up: u8,
        src: u8,
    },
    GetIndex {
        dst: u8,
        obj: u8,
        key: u8,
    },
    /// Indexing with a constant key (globals, dot-fields).
    GetField {
        dst: u8,
        obj: u8,
        k: u16,
    },
    SetIndex {
        obj: u8,
        key: u8,
        src: u8,
    },
    SetField {
        obj: u8,
        k: u16,
        src: u8,
    },
    NewTable {
        dst: u8,
    },
    /// `obj[start+i] = R[base+i]` for i in 0..n-1 (or up to top if n == 0).
    SetList {
        obj: u8,
        base: u8,
        n: u8,
        start: u32,
    },
    Arith {
        op: ArithOp,
        dst: u8,
        lhs: u8,
        rhs: u8,
    },
    Unary {
        op: UnaryOp,
        dst: u8,
        src: u8,
    },
    /// Comparison producing a boolean in `dst`.
    Cmp {
        op: CmpOp,
        dst: u8,
        lhs: u8,
        rhs: u8,
    },
    /// Concatenates registers `base..base+n` into `dst`.
    Concat {
        dst: u8,
        base: u8,
        n: u8,
    },
    Jump {
        off: i32,
    },
    /// Jumps if `truthy(R[src]) == if_true`.
    Test {
        src: u8,
        if_true: bool,
        off: i32,
    },
    /// Callee at `R[base]`, args follow it.
    Call {
        base: u8,
        nargs: u8,
        nres: u8,
    },
    /// Proper tail call: like `Call` with an open result count, but the
    /// current frame is replaced instead of a new one being pushed, so
    /// unbounded tail recursion runs in constant frame depth.
    TailCall {
        base: u8,
        nargs: u8,
    },
    Return {
        base: u8,
        n: u8,
    },
    Vararg {
        dst: u8,
        n: u8,
    },
    Closure {
        dst: u8,
        p: u16,
    },
    /// Closes open upvalues at register `from` and above, and runs
    /// `__close` on to-be-closed variables at or above it.
    Close {
        from: u8,
    },
    /// Marks the register as a to-be-closed variable (`local x <close>`).
    Tbc {
        reg: u8,
        /// Constant index of the variable's name, for PUC's
        /// `variable 'x' got a non-closable value` error.
        name: u16,
    },
    /// Numeric for: `base` holds (counter, limit, step); `base+3` is the
    /// visible variable. `ForPrep` validates and jumps past `ForLoop` when the
    /// loop runs zero times; `ForLoop` steps and jumps back while in range.
    ForPrep {
        base: u8,
        off: i32,
    },
    ForLoop {
        base: u8,
        off: i32,
    },
    /// Generic for: `base` holds (func, state, control, closing value); vars
    /// start at `base+4`. If `R[base+4] ~= nil`, sets control and jumps back.
    TForLoop {
        base: u8,
        off: i32,
    },
}

impl Instr {
    /// Highest register index (plus one) this instruction can reference
    /// statically. Multi-value fields encoded as `0` extend to the thread's
    /// dynamic top at run time and are handled separately by the VM; here they
    /// contribute only their register base. Used as a conservative lower bound
    /// on a frame's live extent.
    #[must_use]
    pub fn reg_high(self) -> u8 {
        fn m(a: u8, b: u8) -> u8 {
            a.max(b)
        }
        match self {
            Instr::LoadK { dst, .. }
            | Instr::LoadBool { dst, .. }
            | Instr::GetUpval { dst, .. }
            | Instr::NewTable { dst }
            | Instr::Closure { dst, .. }
            | Instr::Vararg { dst, .. } => dst.saturating_add(1),
            Instr::LoadNil { dst, n } => dst.saturating_add(n),
            Instr::Move { dst, src } | Instr::Unary { dst, src, .. } => {
                m(dst, src).saturating_add(1)
            }
            Instr::SetUpval { src, .. } | Instr::Test { src, .. } => src.saturating_add(1),
            Instr::GetIndex { dst, obj, key } => m(m(dst, obj), key).saturating_add(1),
            Instr::GetField { dst, obj, .. } => m(dst, obj).saturating_add(1),
            Instr::SetIndex { obj, key, src } => m(m(obj, key), src).saturating_add(1),
            Instr::SetField { obj, src, .. } => m(obj, src).saturating_add(1),
            Instr::SetList { obj, base, n, .. } => {
                m(obj, base.saturating_add(n.max(1))).saturating_add(1)
            }
            Instr::Arith { dst, lhs, rhs, .. } | Instr::Cmp { dst, lhs, rhs, .. } => {
                m(m(dst, lhs), rhs).saturating_add(1)
            }
            Instr::Concat { dst, base, n } => m(dst.saturating_add(1), base.saturating_add(n)),
            // `UNPATCHED_CLOSE` marks a not-yet-patched forward `goto`.
            Instr::Jump { .. }
            | Instr::Close {
                from: UNPATCHED_CLOSE,
            } => 0,
            Instr::Call { base, .. } | Instr::TailCall { base, .. } => base.saturating_add(1),
            Instr::Return { base, n } => {
                if n == 0 {
                    base.saturating_add(1)
                } else {
                    base.saturating_add(n - 1).max(base.saturating_add(1))
                }
            }
            Instr::Close { from } => from.saturating_add(1),
            Instr::Tbc { reg, .. } => reg.saturating_add(1),
            Instr::ForPrep { base, .. } | Instr::ForLoop { base, .. } => base.saturating_add(4),
            Instr::TForLoop { base, .. } => base.saturating_add(5),
        }
    }
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
    /// Name of each upvalue, parallel to `upvals` (for `debug.getupvalue`).
    pub upval_names: Vec<Box<str>>,
    pub nparams: u8,
    pub is_vararg: bool,
    pub max_regs: u8,
    /// Live register extent (relative to the frame base) while each
    /// instruction executes, parallel to `code`. The GC marks only
    /// `base .. base + reg_extent[pc]` for a frame, so registers that the
    /// compiler has freed (dead temporaries) and slots of popped frames are
    /// not roots. Instructions with an open multret field (`0`) extend past
    /// this at run time; the VM accounts for those at the relevant GC points.
    pub reg_extent: Vec<u8>,
    /// For error messages.
    pub name: String,
    /// Source line of the `function` keyword (0 for the main chunk).
    pub linedefined: u32,
    /// Last source line belonging to this function.
    pub lastlinedefined: u32,
    /// Debug name of the called function for each call instruction, parallel
    /// to `code` (`None` for non-call instructions): `(namewhat, name)`,
    /// where `namewhat` is one of `global`/`local`/`upvalue`/`method`/`field`.
    /// Used by `debug.getinfo`'s `n` option.
    pub call_names: Vec<Option<(&'static str, Box<str>)>>,
}
