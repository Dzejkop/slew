//! The stackless VM and the public embedding API.
//!
//! Lua call frames live in `Thread::frames` (a Vec, never the Rust call
//! stack), so the dispatch loop can stop after any instruction and resume
//! later: that is what makes executions suspendable at arbitrary points.
//!
//! Metamethod invocations are pushed frames too, with a "return shape"
//! describing how their result feeds back (into a destination register,
//! coerced to a boolean for comparisons, prefixed with true/false for
//! pcall). `pcall` is not a callback: it marks the callee's frame as a
//! protection boundary and error unwinding walks the frame vec.

use crate::bytecode::{ArithOp, CmpOp, Instr, Proto, UnaryOp, UpvalDesc};
use crate::compiler::{CompileError, compile};
use crate::host::{Host, HostObject, Userdata};
use crate::parser::{ParseError, parse};
use crate::value::{
    ClosId, F64_TWO_POW_63, NativeId, StrRef, Strings, Table, TableId, ThreadId, UpvalId,
    UserdataId, Value, float_to_exact_int, fmt_number,
};
use std::fmt;
use std::fmt::Write as _;
use std::rc::Rc;
use strum::IntoEnumIterator as _;

/// Default cap on call-frame depth; a deliberately bounded execution profile
/// knob (recursion consumes heap, not the host stack).
const MAX_CALL_DEPTH: usize = 10_000;
/// Bound on `__index`/`__newindex`/`__call` metamethod chains. High enough
/// for the upstream suite's 100-deep `__call` chains while still catching a
/// genuine cycle; `__call` resolution keeps the argument window at a fixed
/// scratch base, so chasing N levels costs O(N) space, not O(N^2).
const MAX_META_CHAIN: usize = 10_000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error(transparent)]
    Runtime(RuntimeError),
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("runtime error: {message}")]
pub struct RuntimeError {
    /// Rendered error message (position-prefixed for VM-raised errors).
    pub message: String,
    /// The Lua error value (`error()` can raise any value).
    pub value: Value,
    /// Line where the error was raised; for errors inside a called function
    /// or a `load`ed chunk this points into that chunk, not the root script.
    pub line: u32,
    /// Line of the outermost frame (the root script) when the error escaped.
    /// Handy for progress reporting: a script that calls a helper still
    /// reports its own call site here.
    pub root_line: u32,
}

/// Internal in-flight error: either a raw Lua value (from `error()`) or a
/// message that gets position-prefixed when materialized.
pub(crate) enum ErrVal {
    Msg(String),
    Val(Value),
}

pub(crate) struct VmError {
    pub val: ErrVal,
    pub line: u32,
    /// Line of the outermost frame at raise time (the script's own call
    /// site). Set by the dispatcher before unwinding pops frames, since
    /// `recover` discards them.
    pub root_line: u32,
    /// Chunk name for the position prefix; `None` for native errors,
    /// which carry no position (matching PUC).
    pub source: Option<Rc<str>>,
}

/// A native function installed with [`Lua::register_native`].
///
/// The `Err(String)` is a Lua error *value*, raised verbatim (no position
/// prefix), matching PUC-Lua's native errors — not a Rust control-flow error.
/// Slew-internal failures use [`Error`].
pub type NativeFn<C = ()> = fn(&mut Lua<C>, &[Value]) -> Result<Vec<Value>, String>;

/// A native function that may suspend; `Err(String)` is a Lua error value, as
/// for [`NativeFn`].
pub type SuspendableNativeFn<C> =
    fn(&mut NativeContext<'_, C>, &[Value]) -> Result<NativeOutcome, String>;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ExecutionId(u64);

/// A host-chosen token naming one or more native waits.
///
/// The embedder completes waits with [`Execution::complete_native`]. Tokens
/// are scoped to an execution; completing a token completes every wait parked
/// under it in that execution, so a native that must complete calls
/// individually should mint a token unique among its simultaneously parked
/// calls.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NativeWait(pub u64);

#[derive(Debug)]
pub enum NativeOutcome {
    Return(Vec<Value>),
    /// Suspends the calling thread until the embedder completes the wait with
    /// [`Execution::complete_native`].
    ///
    /// On the execution's root thread — or on a coroutine wait that the VM
    /// cannot park (a coroutine with no resumer, a non-yieldable C-call
    /// boundary, a staged `coroutine.yield`, or an active multi-step driver
    /// such as `print`/`string.format`/`coroutine.close`, an xpcall error
    /// handler, a `__gc` finalizer) — the whole execution blocks and
    /// [`Execution::step`] returns
    /// [`Step::Waiting`]. On a parkable coroutine only that coroutine sleeps:
    /// it yields to its resumer while every other coroutine keeps running. A
    /// `coroutine.resume` of a parked coroutine returns `true` with no values;
    /// a `coroutine.wrap` call returns no values at all.
    Wait(NativeWait),
}

pub struct NativeContext<'a, C> {
    lua: &'a mut Lua<C>,
    execution: ExecutionId,
    context: &'a mut C,
}

impl<C> NativeContext<'_, C> {
    #[must_use]
    pub fn execution_id(&self) -> ExecutionId {
        self.execution
    }

    #[must_use]
    pub fn context(&self) -> &C {
        self.context
    }

    pub fn context_mut(&mut self) -> &mut C {
        self.context
    }

    #[must_use]
    pub fn str_bytes(&self, value: Value) -> Option<&[u8]> {
        self.lua.str_bytes(value)
    }

    pub fn new_string(&mut self, value: &[u8]) -> Value {
        self.lua.new_string(value)
    }

    pub fn new_table(&mut self) -> Value {
        self.lua.new_table()
    }

    #[must_use]
    pub fn table_get(&self, table: Value, key: Value) -> Value {
        self.lua.table_get(table, key)
    }

    #[must_use]
    pub fn display_value(&self, value: Value) -> String {
        self.lua.display_value(value)
    }
}

/// Host-provided module byte source. `Ok(None)` means "not found"; `Err` is
/// a hard failure. See [`Lua::set_file_reader`].
type FileReaderFn = dyn FnMut(&str) -> Result<Option<Vec<u8>>, String>;

/// Builtins that must interact with the frame machinery (raise error
/// values, set up protected calls, call metamethods, switch threads).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Intrinsic {
    Pcall,
    Xpcall,
    Error,
    Assert,
    ToString,
    Resume,
    Yield,
    /// The function returned by `coroutine.wrap`: resumes its thread,
    /// returns results bare, propagates errors.
    WrapResume(ThreadId),
    /// Increment / decrement the thread's non-yieldable depth. The prelude
    /// brackets a callback that PUC's C library invokes with `lua_call`
    /// (non-yieldable), so `coroutine.yield` inside it errors with
    /// "attempt to yield across a C-call boundary" and `isyieldable` is false.
    EnterNonYieldable,
    LeaveNonYieldable,
    IsYieldable,
    Running,
    CoroutineClose,
    CollectGarbage,
    Print,
    /// `string.format`: parses the format string and renders arguments,
    /// suspending to run `__tostring` for `%s` where present.
    Format,
    DebugGetinfo,
    DebugTraceback,
    DebugGetupvalue,
    DebugSetupvalue,
    DebugUpvalueid,
    DebugUpvaluejoin,
    DebugGetmetatable,
    DebugSetmetatable,
    DebugGetregistry,
    DebugGethook,
    DebugSethook,
}

pub(crate) enum NativeKind<C> {
    Plain(NativeFn<C>),
    Suspendable(SuspendableNativeFn<C>),
    Intrinsic(Intrinsic),
}

// Manual impls: `#[derive(Copy, Clone)]` would wrongly require `C: Copy`/`C: Clone`
// even though every variant is a plain `Copy` payload (fn pointers and `Intrinsic`).
impl<C> Copy for NativeKind<C> {}

impl<C> Clone for NativeKind<C> {
    fn clone(&self) -> Self {
        *self
    }
}

pub(crate) struct Native<C> {
    pub name: String,
    pub kind: NativeKind<C>,
}

pub(crate) struct LuaClosure {
    pub proto: Rc<Proto>,
    pub upvals: Vec<UpvalId>,
}

pub(crate) enum Upval {
    /// Live in a thread's register stack.
    Open(ThreadId, usize),
    Closed(Value),
}

/// How a frame's return values are delivered to `ret_to`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RetShape {
    Normal,
    /// First result coerced to a boolean (comparison metamethods).
    ToBool,
    ToNotBool,
    /// `true` prepended (protected call succeeded).
    PrependTrue,
    /// `false` prepended (xpcall message handler result).
    PrependFalse,
}

/// Immutable call shape shared by the intrinsic dispatch arms. Reading
/// arguments through it keeps the dispatcher itself a flat jump table.
#[derive(Clone, Copy)]
struct IntrinsicCall {
    func_abs: usize,
    argc: usize,
    ret_to: usize,
    nres: u8,
    shape: RetShape,
    native_caller: bool,
}

impl IntrinsicCall {
    /// The `i`-th argument, or `None` when absent (PUC distinguishes an
    /// absent argument from an explicit `nil`).
    fn arg_opt(self, th: &Thread, i: usize) -> Option<Value> {
        (i < self.argc).then(|| th.stack[self.func_abs + 1 + i])
    }

    /// The `i`-th argument, or `nil` when absent.
    fn arg(self, th: &Thread, i: usize) -> Value {
        self.arg_opt(th, i).unwrap_or(Value::Nil)
    }
}

/// A continuation the frame must run when it becomes the top of the stack
/// again (after a metamethod call it triggered returns). Processed LIFO,
/// one per dispatch step, before the next instruction fetch.
#[derive(Clone, Copy, Debug)]
enum Pending {
    /// Re-run concatenation over registers `base..base+n` into `dst`; the
    /// metamethod result was stored at `base+n-1`.
    Concat { dst: u8, base: u8, n: u8 },
    /// Run `__close` on to-be-closed variables at register `from` and
    /// above (one call per step; re-arms itself).
    CloseTbc { from: u8, err: Value },
    /// Call `__close(v, err)` for a value collected during error unwind.
    CallClose { v: Value, err: Value },
    /// Final step of error recovery: deliver `false, err` (or run the
    /// xpcall handler) at the protected call's result slots.
    DeliverError {
        ret_to: usize,
        nres: u8,
        err: Value,
        handler: Option<Value>,
    },
    /// A message handler errored while handling an error: deliver PUC's
    /// `false, "error in error handling"` at the protected call's slots.
    DeliverErrErr { ret_to: usize, nres: u8 },
    /// Final step of a return that had to run `__close` handlers first.
    FinishReturn { start: usize, count: usize },
    /// Final step of a tail call into a native/intrinsic: the callee's
    /// results now sit at `start` (with `th.top` already updated); complete
    /// the frame's return as usual.
    TailReturn { start: usize },
    /// Advance the active `coroutine.close` driver by one `__close` handler.
    CloseStep,
    /// Advance the active `print` driver by one `__tostring` call.
    PrintStep,
    /// Validate and convert the result of a `tostring` `__tostring` call.
    FinishTostring {
        ret_to: usize,
        nres: u8,
        shape: RetShape,
        result_slot: usize,
    },
    /// Advance the active `string.format` driver by one format item.
    FormatStep,
    /// After an xpcall message handler called at the error point returns,
    /// unwind the frames that were kept alive for it.
    UnwindAfterHandler { result_slot: usize },
    /// Re-apply a return shape to results a nested protected call (pcall /
    /// xpcall) already delivered at `ret_to`. PUC's `xpcall(pcall, ...)` and
    /// `pcall(pcall, ...)` each add their own success flag on top of the
    /// inner call's results; slew's intrinsics delegate to `protected_call`,
    /// which stamps only the inner flag, so the outer one is applied here.
    PrependShape {
        ret_to: usize,
        nres: u8,
        shape: RetShape,
    },
    /// Emit the "return" hook for the frame that is about to be popped, then
    /// let the following `FinishReturn` complete the return. Used when a
    /// function with to-be-closed variables returns: closes run first, then
    /// the hook, then the frame is popped.
    ReturnHookFire,
    /// Advance a deferred yield through its hook stages, then switch threads.
    YieldStep,
    /// Re-raise an error after the collected `__close` handlers have run. Used
    /// when an error escapes a thread with no protection boundary: the closes
    /// are staged on a boundary frame, then the error propagates.
    Reraise { err: Value },
}

struct LuaFrame {
    closure: ClosId,
    proto: Rc<Proto>,
    pc: usize,
    base: usize,
    /// Absolute stack slot where results go.
    ret_to: usize,
    /// Results expected by the caller: count+1, or 0 for multret.
    nres: u8,
    shape: RetShape,
    /// Error-protection boundary (set by pcall/xpcall on the callee frame).
    protected: bool,
    /// xpcall message handler.
    handler: Option<Value>,
    /// This frame is an xpcall message handler: if it errors, PUC reports
    /// `error in error handling` instead of propagating.
    handler_guard: bool,
    pending: Vec<Pending>,
    /// Registers holding active to-be-closed variables (ascending).
    tbc: Vec<u8>,
    varargs: Vec<Value>,
    /// True if this frame was entered via a proper tail call.
    tailcall: bool,
    /// When this frame is a metamethod call (e.g. `__close`), the PUC
    /// `namewhat`/`name` to report for it (`("metamethod", "close")`).
    call_meta: Option<(&'static str, &'static str)>,
    /// Last source line reported for a line hook (`-1` before the first).
    last_line: i64,
    /// This frame is a running debug hook: hooks are disabled while it (or
    /// anything it calls) is on the stack, matching PUC's `allowhook`.
    is_hook: bool,
}

/// Hook event bits for [`Thread::hook_mask`].
const HOOK_CALL: u8 = 1;
const HOOK_RETURN: u8 = 2;
const HOOK_LINE: u8 = 4;

/// A boundary frame: no bytecode, never executed. It exists so a protected
/// call has somewhere to stage the continuations it must run before the
/// caller resumes (`__close` handlers, error delivery). It is deliberately
/// invisible to `debug.getinfo`/`traceback`.
struct BoundaryFrame {
    /// First stack slot safely above the caller's live data (scratch base at
    /// push time); used while this frame is the innermost frame.
    base: usize,
    /// `__close`/error-delivery continuations staged on this boundary.
    pending: Vec<Pending>,
    /// Present when this frame is itself the error-protection boundary: its
    /// protected callee was a native/intrinsic with no Lua frame of its own
    /// (e.g. `pcall(tostring, v)` where `__tostring` runs later). Carries the
    /// result destination and message handler for error delivery.
    boundary: Option<CBoundary>,
}

/// Protection metadata for a [`BoundaryFrame`] that guards a native callee.
#[derive(Clone, Copy)]
struct CBoundary {
    ret_to: usize,
    nres: u8,
    handler: Option<Value>,
}

/// Set while an xpcall message handler runs *before* unwinding (so it can see
/// the erroring frames, as PUC's `luaG_errormsg` does). Cleared once the
/// handler returns and the deferred unwind begins.
struct HandlerUnwind;

enum Frame {
    Lua(LuaFrame),
    Boundary(BoundaryFrame),
}

impl Frame {
    fn as_lua(&self) -> &LuaFrame {
        match self {
            Frame::Lua(f) => f,
            Frame::Boundary(_) => unreachable!("boundary frame accessed as a Lua frame"),
        }
    }

    fn as_lua_mut(&mut self) -> &mut LuaFrame {
        match self {
            Frame::Lua(f) => f,
            Frame::Boundary(_) => unreachable!("boundary frame accessed as a Lua frame"),
        }
    }

    fn is_boundary(&self) -> bool {
        matches!(self, Frame::Boundary(_))
    }

    fn lua(&self) -> Option<&LuaFrame> {
        match self {
            Frame::Lua(f) => Some(f),
            Frame::Boundary(_) => None,
        }
    }

    /// Whether this frame is an error-protection boundary. A Lua frame carries
    /// the flag when it is a protected callee; a boundary frame carries it when
    /// the protected callee was a native (see [`BoundaryFrame::boundary`]).
    fn is_protected(&self) -> bool {
        match self {
            Frame::Lua(f) => f.protected,
            Frame::Boundary(c) => c.boundary.is_some(),
        }
    }

    fn pending(&self) -> &[Pending] {
        match self {
            Frame::Lua(f) => &f.pending,
            Frame::Boundary(f) => &f.pending,
        }
    }

    fn pending_mut(&mut self) -> &mut Vec<Pending> {
        match self {
            Frame::Lua(f) => &mut f.pending,
            Frame::Boundary(f) => &mut f.pending,
        }
    }
}

/// The message handler and handler-guard of a protection boundary frame.
fn frame_boundary(f: &Frame) -> (Option<Value>, bool) {
    match f {
        Frame::Lua(f) => (f.handler, f.handler_guard),
        Frame::Boundary(c) => (c.boundary.as_ref().and_then(|b| b.handler), false),
    }
}

/// Whether a frame still holds to-be-closed variables to run.
fn frame_has_tbc(f: &Frame) -> bool {
    matches!(f, Frame::Lua(f) if !f.tbc.is_empty())
}

/// Whether a frame is a directly-invoked `__close` metamethod call.
fn frame_close_meta(f: &Frame) -> bool {
    matches!(f, Frame::Lua(f) if f.call_meta == Some(("metamethod", "close")))
}

/// Whether a frame already carries a staged recovery chain: its `__close`
/// handlers and final error delivery are in flight, so a fresh error folds
/// into that chain instead of unwinding past it.
fn frame_staged_recovery(f: &Frame) -> bool {
    f.pending()
        .iter()
        .any(|p| matches!(p, Pending::DeliverError { .. } | Pending::Reraise { .. }))
}

/// Appends PUC's `(metamethod 'name')` suffix to a non-callable metamethod
/// error message (`attempt to call a number value (metamethod 'close')`).
fn annotate_metamethod_error(e: &mut VmError, name: &str) {
    if let ErrVal::Msg(m) = &mut e.val
        && !m.contains("(metamethod '")
    {
        let _ = write!(m, " (metamethod '{name}')");
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CoStatus {
    /// Created, never resumed.
    #[default]
    Start,
    Suspended,
    Running,
    /// Resumed another coroutine and is waiting for it.
    Normal,
    Dead,
}

/// Where a coroutine's yield/return values go in the thread that resumed it.
#[derive(Clone, Copy)]
struct ResumeRet {
    ret_to: usize,
    nres: u8,
    shape: RetShape,
    /// Prepend the ok/fail boolean (resume) or not (wrap).
    status_bool: bool,
    /// Propagate errors into the resumer (wrap) instead of returning them.
    wrap: bool,
}

#[derive(Default)]
pub(crate) struct Thread {
    stack: Vec<Value>,
    frames: Vec<Frame>,
    /// Open upvalues, sorted by stack index.
    open_upvals: Vec<(usize, UpvalId)>,
    /// Top of the last multret sequence (absolute).
    top: usize,
    pub(crate) status: CoStatus,
    /// Nesting depth of non-yieldable C-call boundaries currently active on
    /// this thread (PUC's `nny`). While non-zero, `coroutine.yield` errors
    /// and `coroutine.isyieldable()` is false. Reset/kept per thread, so a
    /// coroutine created and resumed *inside* such a boundary is yieldable.
    non_yieldable: u32,
    /// The thread that resumed this one.
    parent: Option<ThreadId>,
    /// Result-delivery info in the parent (set at each resume).
    resume_ret: Option<ResumeRet>,
    /// Where the next resume's arguments land (set at each yield).
    yield_ret: Option<(usize, u8, RetShape)>,
    /// True for a root execution's thread. The main thread cannot yield;
    /// every coroutine created by `coroutine.create`/`wrap` can.
    is_main: bool,
    // ---- debug hooks (`debug.sethook`) ----
    /// Hook function, or `None` when no hook is set.
    hook: Option<Value>,
    /// Which events fire: bit 0 call, bit 1 return, bit 2 line.
    hook_mask: u8,
    /// Instruction interval for count events (0 disables them).
    hook_count: i64,
    /// Countdown to the next count event.
    hook_counter: i64,
    /// Non-zero while a coroutine body's start-of-execution call hook is
    /// still owed (the body frame is pushed outside `do_call`).
    pending_call_hook: bool,
    /// A yield whose hook events (call/return) have not all fired yet.
    yield_job: Option<YieldJob>,
    /// For a dead coroutine: the error it died with, returned once by
    /// `coroutine.close`. `Some(Nil)` means "died cleanly".
    close_error: Option<Value>,
    pending_native: Option<PendingNative>,
}

struct PendingNative {
    wait: NativeWait,
    /// Execution whose `complete_native` completes this wait. It changes when
    /// another execution resumes the parked coroutine (see [`Lua::dispatch`]).
    exec: ExecutionId,
    func: Value,
    ret_to: usize,
    nres: u8,
    shape: RetShape,
    completion: Option<Result<Vec<Value>, String>>,
}

/// Hook events to emit before a suspended yield completes. `coroutine.yield`
/// is a C function, so it owes a "call" event when invoked and a "return"
/// event when it yields; both run as ordinary hook frames, so the actual
/// suspension is deferred through [`Pending::YieldStep`].
struct YieldJob {
    parent: ThreadId,
    rr: ResumeRet,
    ret_to: usize,
    nres: u8,
    shape: RetShape,
    args: Vec<Value>,
    /// The `coroutine.yield` native, for `debug.getinfo(2)` in hook events.
    func: Value,
    /// 0: fire call, 1: fire return, 2: perform the yield.
    stage: u8,
}

/// A compiled script, reusable across executions.
#[derive(Clone)]
pub struct Chunk {
    pub(crate) proto: Rc<Proto>,
}

impl fmt::Debug for Chunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Chunk({})", self.proto.name)
    }
}

/// Result of driving an execution by one fuel budget.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// The script finished and returned these values.
    Done(Vec<Value>),
    /// Fuel ran out; call `step` again to continue.
    Pending,
    /// A native call on the root thread — or on a coroutine that cannot yield
    /// where it was called — is waiting for the host to complete it. A wait on
    /// a yieldable coroutine does not surface here: it parks the coroutine and
    /// keeps the execution runnable (see [`Execution::pending_waits`]).
    Waiting(NativeWait),
}

/// A suspendable run of a chunk. Created by [`Lua::execute`]; all VM state
/// lives in the `Lua`, this is a handle plus fuel-debt bookkeeping.
pub struct Execution<C = ()> {
    /// Root thread of this execution (a GC root while the execution lives).
    thread: ThreadId,
    /// Thread to resume on the next step (a coroutine may have been
    /// running when fuel ran out).
    current: ThreadId,
    /// Fuel overdrawn by the last step (surcharges can overshoot), repaid
    /// from the next budget.
    debt: i64,
    finished: bool,
    id: ExecutionId,
    context: Option<C>,
}

/// Metamethod identifiers; indexes into `Lua::mm_names`.
///
/// `mm_names` is built from [`Mm::iter`], so the name list and the
/// `mm as usize` discriminant indexes must stay in declaration order: do not
/// add explicit discriminants or reorder variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug, strum::EnumIter, strum::IntoStaticStr)]
#[repr(usize)]
pub(crate) enum Mm {
    #[strum(serialize = "__index")]
    Index,
    #[strum(serialize = "__newindex")]
    NewIndex,
    #[strum(serialize = "__call")]
    Call,
    #[strum(serialize = "__add")]
    Add,
    #[strum(serialize = "__sub")]
    Sub,
    #[strum(serialize = "__mul")]
    Mul,
    #[strum(serialize = "__div")]
    Div,
    #[strum(serialize = "__mod")]
    Mod,
    #[strum(serialize = "__pow")]
    Pow,
    #[strum(serialize = "__unm")]
    Unm,
    #[strum(serialize = "__idiv")]
    IDiv,
    #[strum(serialize = "__band")]
    BAnd,
    #[strum(serialize = "__bor")]
    BOr,
    #[strum(serialize = "__bxor")]
    BXor,
    #[strum(serialize = "__bnot")]
    BNot,
    #[strum(serialize = "__shl")]
    Shl,
    #[strum(serialize = "__shr")]
    Shr,
    #[strum(serialize = "__concat")]
    Concat,
    #[strum(serialize = "__len")]
    Len,
    #[strum(serialize = "__eq")]
    Eq,
    #[strum(serialize = "__lt")]
    Lt,
    #[strum(serialize = "__le")]
    Le,
    #[strum(serialize = "__tostring")]
    ToString,
    #[strum(serialize = "__close")]
    Close,
}

fn mm_of_arith(op: ArithOp) -> Mm {
    match op {
        ArithOp::Add => Mm::Add,
        ArithOp::Sub => Mm::Sub,
        ArithOp::Mul => Mm::Mul,
        ArithOp::Div => Mm::Div,
        ArithOp::IDiv => Mm::IDiv,
        ArithOp::Mod => Mm::Mod,
        ArithOp::Pow => Mm::Pow,
        ArithOp::BAnd => Mm::BAnd,
        ArithOp::BOr => Mm::BOr,
        ArithOp::BXor => Mm::BXor,
        ArithOp::Shl => Mm::Shl,
        ArithOp::Shr => Mm::Shr,
    }
}

pub struct Lua<C = ()> {
    pub(crate) strings: Strings,
    pub(crate) tables: Vec<Table>,
    pub(crate) closures: Vec<LuaClosure>,
    pub(crate) natives: Vec<Native<C>>,
    pub(crate) upvals: Vec<Upval>,
    pub(crate) threads: Vec<Thread>,
    /// Userdata arena: host objects with optional metatables.
    pub(crate) userdata: Vec<Userdata>,
    pub(crate) globals: TableId,
    pub(crate) string_meta: Option<TableId>,
    mm_names: Vec<StrRef>,
    /// Thread being dispatched right now (its `Thread` is temporarily
    /// moved out of the arena).
    pub(crate) current_thread: ThreadId,
    /// Per-type metatables set through `debug.setmetatable` for values that
    /// cannot carry a per-value metatable: nil, boolean, number, function,
    /// thread (in that slot order). Tables and strings use their own storage.
    pub(crate) type_metas: [Option<TableId>; 5],
    /// Set by resume/yield intrinsics; the dispatch loop performs the
    /// actual thread switch.
    switch_to: Option<ThreadId>,
    /// math.random state (xoshiro256**); deterministically seeded so
    /// scripts behave identically run-to-run unless reseeded.
    rng: [u64; 4],
    // ---- GC state ----
    tables_live: Vec<bool>,
    tables_free: Vec<u32>,
    closures_live: Vec<bool>,
    closures_free: Vec<u32>,
    upvals_live: Vec<bool>,
    upvals_free: Vec<u32>,
    threads_live: Vec<bool>,
    threads_free: Vec<u32>,
    userdata_live: Vec<bool>,
    userdata_free: Vec<u32>,
    natives_live: Vec<bool>,
    natives_free: Vec<u32>,
    /// Root + current thread per live (unfinished) execution.
    exec_roots: std::collections::HashMap<u32, u32>,
    /// Host-pinned values (see [`Lua::anchor`]).
    anchors: Vec<Value>,
    /// Placeholder proto for swept closure slots.
    empty_proto: Rc<Proto>,
    next_execution_id: u64,
    current_execution: Option<ExecutionId>,
    active_context: Option<C>,
    /// Transient signal from a native wait that could not park a coroutine:
    /// the dispatch loop turns it into `DispatchEnd::Waiting`, blocking the
    /// whole execution. Yieldable coroutine waits park instead and do not set
    /// this (see [`Lua::wait_can_park`]).
    suspended_wait: Option<NativeWait>,
    /// True while `service_finalizers` invokes a `__gc` handler inline, before
    /// it knows whether the handler left a frame. A wait from a native handler
    /// must block rather than park: the finalizer call has no frame to resume.
    finalizer_running: bool,
    allocs_since_gc: usize,
    str_bytes_at_gc: usize,
    /// Auto-GC after this many allocations (0 disables auto collection).
    pub gc_alloc_threshold: usize,
    /// Memory ceiling in (approximate) bytes; allocation past it raises a
    /// Lua error once detected at a collection point.
    pub memory_limit: Option<usize>,
    /// Host-supplied source reader for `loadfile`/`dofile` and
    /// `package.searchpath`. Absent by default: the core has no filesystem
    /// authority of its own.
    file_reader: Option<Box<FileReaderFn>>,
    // ---- GC controls (`collectgarbage`) ----
    /// False after `collectgarbage("stop")`: automatic collection pauses
    /// (explicit `collect`/`step` still run).
    pub(crate) gc_running: bool,
    /// 0 = incremental, 1 = generational. Only reported by the mode switches;
    /// the collector itself is a stop-the-world mark-sweep.
    pub(crate) gc_mode: u8,
    pub(crate) gc_pause: i64,
    pub(crate) gc_stepmul: i64,
    /// Objects selected for `__gc`, awaiting a dispatch safe point. A GC root.
    pending_finalizers: Vec<Value>,
    /// Frame depth at which the in-flight finalizer was started; suppresses
    /// nested finalizer dispatch until that frame returns.
    finalizer_depth: Option<usize>,
    /// In-progress `coroutine.close` driver (see [`CloseJob`]).
    close_job: Option<CloseJob>,
    /// In-progress `print` driver (see [`PrintJob`]).
    print_job: Option<PrintJob>,
    /// In-progress `string.format` driver (see [`FormatJob`]).
    format_job: Option<FormatJob>,
    /// An xpcall message handler running at the error point; see
    /// [`HandlerUnwind`].
    handler_unwind: Option<HandlerUnwind>,
    /// Non-zero while [`Lua::fire_hook`] installs a hook frame: suppresses the
    /// call hook that the hook invocation would otherwise emit.
    hook_suppress: u32,
    /// Embedder-installed capability host. `None` means `io`/`os` are absent.
    pub(crate) host: Option<Box<dyn Host>>,
    /// The file userdata metatable (set when a host is installed).
    pub(crate) file_meta: Option<TableId>,
    /// Default `io.input()` / `io.output()` handles (GC roots).
    pub(crate) io_input: Option<UserdataId>,
    pub(crate) io_output: Option<UserdataId>,
    /// Set by `os.exit`: the requested exit code, for the embedder to observe.
    /// Never terminates the process here.
    pub(crate) exit_request: Option<i64>,
}

/// State for the incremental `coroutine.close` driver: `__close` handlers are
/// invoked one per dispatch step, protected, until every pending value has
/// been closed. Driven by [`Pending::CloseStep`].
struct CloseJob {
    target: ThreadId,
    /// Values to close, innermost-first.
    items: Vec<Value>,
    idx: usize,
    /// Error accumulated so far (passed to later handlers).
    err: Value,
    failed: bool,
    /// Where the eventual `true` / `false, err` result lands in the caller.
    ret_to: usize,
    nres: u8,
    shape: RetShape,
    /// Scratch slot where the last protected `__close` result was delivered.
    result_slot: usize,
    /// True when `result_slot` holds an unread result.
    awaiting: bool,
}

/// State for the incremental `print` driver: arguments are rendered one per
/// dispatch step, invoking `__tostring` where present. Driven by
/// [`Pending::PrintStep`].
struct PrintJob {
    items: Vec<Value>,
    idx: usize,
    /// Rendered argument bytes, in order.
    out: Vec<Vec<u8>>,
    ret_to: usize,
    nres: u8,
    shape: RetShape,
    result_slot: usize,
    awaiting: bool,
}

/// State for the incremental `string.format` driver. Mirrors PUC's
/// `str_format` loop, but suspends to resolve `%s` via `__tostring`.
struct FormatJob {
    fmt: Vec<u8>,
    /// Next byte of `fmt` to consume.
    pos: usize,
    /// PUC's `arg`: 1-based index into `args` (1 is the format string).
    arg: usize,
    /// `[format, ...variadic args]`.
    args: Vec<Value>,
    out: Vec<u8>,
    ret_to: usize,
    nres: u8,
    shape: RetShape,
    result_slot: usize,
    awaiting: bool,
    /// While awaiting a `%s` `__tostring` result: the spec, its argument,
    /// and the 1-based arg index to report in errors.
    saved: Option<(crate::stdlib::string::FmtSpec, Value, usize)>,
}

/// How a table participates in the weak-reference machinery (`__mode`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WeakKind {
    Strong,
    /// `k`: keys weak, values strong (ephemeron).
    Keys,
    /// `v`: keys strong, values weak.
    Values,
    /// `kv`: both weak.
    Both,
}

impl<C> Default for Lua<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl Lua {
    /// Instantiates a chunk as a suspended execution. Nothing runs until
    /// [`Execution::step`] is called.
    pub fn execute(&mut self, chunk: &Chunk) -> Execution {
        self.execute_inner(chunk, ())
    }
}

fn n_dead<C>(_: &mut Lua<C>, _: &[Value]) -> Result<Vec<Value>, String> {
    Err("attempt to call a collected function".into())
}

struct Marks {
    strings: Vec<bool>,
    tables: Vec<bool>,
    closures: Vec<bool>,
    natives: Vec<bool>,
    upvals: Vec<bool>,
    threads: Vec<bool>,
    userdata: Vec<bool>,
}

/// Marks the values reachable only from a frame's staged continuations.
fn mark_pending(pending: &[Pending], th: &Thread, work: &mut Vec<Value>, stack_len: usize) {
    for p in pending {
        match *p {
            Pending::CallClose { v, err } => {
                work.push(v);
                work.push(err);
            }
            Pending::DeliverError { err, handler, .. } => {
                work.push(err);
                if let Some(h) = handler {
                    work.push(h);
                }
            }
            Pending::CloseTbc { err, .. } | Pending::Reraise { err } => work.push(err),
            // Return values staged above the register window must survive
            // while their `__close` handlers run.
            Pending::FinishReturn { start, count } => {
                let end = (start + count).min(stack_len);
                if start < end {
                    work.extend_from_slice(&th.stack[start..end]);
                }
            }
            Pending::TailReturn { start } => {
                let end = th.top.min(stack_len);
                if start < end {
                    work.extend_from_slice(&th.stack[start..end]);
                }
            }
            Pending::Concat { .. }
            | Pending::DeliverErrErr { .. }
            | Pending::CloseStep
            | Pending::PrintStep
            | Pending::FinishTostring { .. }
            | Pending::FormatStep
            | Pending::UnwindAfterHandler { .. }
            | Pending::PrependShape { .. }
            | Pending::ReturnHookFire
            | Pending::YieldStep => {}
        }
    }
}

fn mark_upval(uid: UpvalId, m: &mut Marks, work: &mut Vec<Value>, upvals: &[Upval]) {
    let i = uid.0 as usize;
    if m.upvals[i] {
        return;
    }
    m.upvals[i] = true;
    match upvals[i] {
        Upval::Closed(v) => work.push(v),
        Upval::Open(t, _) => work.push(Value::Thread(t)),
    }
}

struct CallSpec {
    func_abs: usize,
    argc: usize,
    ret_to: usize,
    nres: u8,
    shape: RetShape,
    protected: bool,
    handler: Option<Value>,
    /// True when the call comes from a native (e.g. pcall invoking its
    /// callee): `error` at level 1 then has no Lua position, like PUC's
    /// `luaL_where` at a C boundary.
    native_caller: bool,
}

enum Flow {
    Continue,
    Finished(Vec<Value>),
}

enum DispatchEnd {
    Pending,
    Waiting(NativeWait),
    Finished(Vec<Value>),
    Switch(ThreadId),
}

pub(crate) enum RunOutcome {
    Done(Vec<Value>),
    Pending(ThreadId),
    Waiting(ThreadId, NativeWait),
}

/// Delivers a coroutine's yield/return values (or failure) into the thread
/// that resumed it.
fn deliver_resume(parent: &mut Thread, rr: ResumeRet, ok: bool, vals: &[Value]) {
    if rr.status_bool {
        let mut all = Vec::with_capacity(vals.len() + 1);
        all.push(Value::Bool(ok));
        all.extend_from_slice(vals);
        place_shaped(parent, rr.ret_to, rr.nres, rr.shape, &all);
    } else {
        place_shaped(parent, rr.ret_to, rr.nres, rr.shape, vals);
    }
}

// ---- free helpers ----

/// A resolved `debug.getinfo` level: the running C function (current thread
/// level 0) or a Lua frame's snapshot.
enum LevelFrame {
    Native,
    Lua {
        proto: Rc<Proto>,
        pc: usize,
        closure: ClosId,
        tailcall: bool,
        name: Option<(&'static str, Box<str>)>,
    },
}

/// PUC's `luaO_chunkid`-style short source name, used by `debug.getinfo` and
/// `debug.traceback`. `LUA_IDSIZE` is 60.
fn short_source(src: &str) -> String {
    const MAX_ID: usize = 60;
    if let Some(rest) = src.strip_prefix('=') {
        rest.chars().take(MAX_ID - 1).collect()
    } else if let Some(rest) = src.strip_prefix('@') {
        let n = rest.chars().count();
        if n < MAX_ID {
            rest.to_string()
        } else {
            let tail: String = rest.chars().skip(n - (MAX_ID - 4)).collect();
            format!("...{tail}")
        }
    } else {
        let first = src.lines().next().unwrap_or("");
        let max = MAX_ID.saturating_sub(15);
        let truncated: String = first.chars().take(max).collect();
        format!("[string \"{truncated}\"]")
    }
}

/// Primary destination register of an instruction, if it writes one. Used by
/// the best-effort `varinfo` naming for "number has no integer
/// representation" errors.
fn instr_dst(i: Instr) -> Option<u8> {
    match i {
        Instr::LoadK { dst, .. }
        | Instr::LoadNil { dst, .. }
        | Instr::LoadBool { dst, .. }
        | Instr::Move { dst, .. }
        | Instr::GetUpval { dst, .. }
        | Instr::GetIndex { dst, .. }
        | Instr::GetField { dst, .. }
        | Instr::NewTable { dst }
        | Instr::Arith { dst, .. }
        | Instr::Unary { dst, .. }
        | Instr::Cmp { dst, .. }
        | Instr::Concat { dst, .. }
        | Instr::Vararg { dst, .. }
        | Instr::Closure { dst, .. } => Some(dst),
        Instr::Call { base, .. } | Instr::TailCall { base, .. } => Some(base),
        Instr::ForLoop { base, .. } => Some(base + 3),
        Instr::TForLoop { base, .. } => Some(base + 4),
        _ => None,
    }
}

/// Best-effort PUC `varinfo`: name the register `reg` as it was last written
/// before `before_pc`, returning e.g. `field 'huge'`. Only constant-key
/// fields, upvalues, and constants are recognized; locals and dynamic keys
/// yield `None` (in which case callers keep the plain message).
fn name_for_register(
    strings: &Strings,
    proto: &Proto,
    before_pc: usize,
    reg: u8,
) -> Option<String> {
    let mut pc = before_pc.min(proto.code.len());
    while pc > 0 {
        pc -= 1;
        let instr = proto.code[pc];
        match instr {
            Instr::GetField { dst, k, .. } if dst == reg => {
                return match proto.consts.get(k as usize) {
                    Some(Value::Str(s)) => Some(format!("field '{}'", strings.get_str_lossy(*s))),
                    _ => None,
                };
            }
            Instr::GetUpval { dst, up } if dst == reg => {
                return proto
                    .upval_names
                    .get(up as usize)
                    .map(|n| format!("upvalue '{n}'"));
            }
            Instr::LoadK { dst, .. } if dst == reg => return Some("constant".to_string()),
            _ => {
                if instr_dst(instr) == Some(reg) {
                    return None;
                }
            }
        }
    }
    None
}

fn kval(th: &Thread, k: u16) -> Value {
    th.frames.last().unwrap().as_lua().proto.consts[k as usize]
}

fn jump(th: &mut Thread, off: i32) {
    let f = th.frames.last_mut().unwrap().as_lua_mut();
    f.pc = (f.pc as i64 + off as i64) as usize;
}

fn line_of(th: &Thread) -> u32 {
    // Boundary frames carry no line info; report the nearest Lua frame's line.
    th.frames
        .iter()
        .rev()
        .find_map(|f| f.lua())
        .map_or(0, frame_line)
}

/// Source line the frame's next instruction belongs to.
fn frame_line(f: &LuaFrame) -> u32 {
    f.proto
        .lines
        .get(f.pc.wrapping_sub(1))
        .copied()
        .unwrap_or(0)
}

fn ensure_len(stack: &mut Vec<Value>, len: usize) {
    if stack.len() < len {
        stack.resize(len, Value::Nil);
    }
}

/// Live register extent (relative to `f.base`) of the instruction a frame is
/// currently executing. `pc` points at the *next* instruction, so the live one
/// is `pc - 1`. Slots at or above the returned extent are dead temporaries and
/// must not be treated as roots.
fn frame_reg_extent(f: &LuaFrame) -> usize {
    if f.pc == 0 {
        // Frame pushed but not yet executing (or a chunk that has not run
        // yet): nothing above the declared window can be live.
        return f.proto.max_regs as usize;
    }
    let idx = f.pc - 1;
    let recorded = f
        .proto
        .reg_extent
        .get(idx)
        .copied()
        .unwrap_or(f.proto.max_regs) as usize;
    // Belt and braces: never drop a register the instruction itself names,
    // even if a future emit order were to under-report `free_reg`.
    let named = f.proto.code.get(idx).map_or(0, |i| i.reg_high()) as usize;
    recorded.max(named)
}

/// First stack slot safely above all live data of the current frame.
/// Bounded by the frame's register window (plus any active multret run),
/// so repeated metamethod calls reuse the same scratch space instead of
/// growing the stack.
fn scratch_base(th: &Thread) -> usize {
    match th.frames.last().unwrap() {
        Frame::Lua(f) => (f.base + f.proto.max_regs as usize).max(th.top),
        // A boundary frame keeps the scratch base recorded at push time.
        Frame::Boundary(c) => c.base.max(th.top),
    }
}

fn is_concatable(v: Value) -> bool {
    matches!(v, Value::Str(_) | Value::Int(_) | Value::Float(_))
}

/// Places `results` at `ret_to` per the multret encoding in `nres`.
fn place_results(th: &mut Thread, ret_to: usize, nres: u8, results: &[Value]) {
    if nres == 0 {
        ensure_len(&mut th.stack, ret_to + results.len());
        th.stack[ret_to..ret_to + results.len()].copy_from_slice(results);
        th.top = ret_to + results.len();
    } else {
        let want = (nres - 1) as usize;
        ensure_len(&mut th.stack, ret_to + want);
        for i in 0..want {
            th.stack[ret_to + i] = results.get(i).copied().unwrap_or(Value::Nil);
        }
    }
}

fn place_shaped(th: &mut Thread, ret_to: usize, nres: u8, shape: RetShape, res: &[Value]) {
    match shape {
        RetShape::Normal => place_results(th, ret_to, nres, res),
        RetShape::ToBool => {
            let b = res.first().copied().unwrap_or(Value::Nil).truthy();
            place_results(th, ret_to, nres, &[Value::Bool(b)]);
        }
        RetShape::ToNotBool => {
            let b = res.first().copied().unwrap_or(Value::Nil).truthy();
            place_results(th, ret_to, nres, &[Value::Bool(!b)]);
        }
        RetShape::PrependTrue | RetShape::PrependFalse => {
            let flag = Value::Bool(shape == RetShape::PrependTrue);
            let mut all = Vec::with_capacity(res.len() + 1);
            all.push(flag);
            all.extend_from_slice(res);
            place_results(th, ret_to, nres, &all);
        }
    }
}

/// Delivers a returning Lua frame's values (in `stack[start..start+count]`)
/// to its caller per the frame's shape and expected count.
fn deliver_return(th: &mut Thread, frame: &LuaFrame, start: usize, count: usize) {
    let ret_to = frame.ret_to;
    match frame.shape {
        RetShape::Normal => {
            if frame.nres == 0 {
                ensure_len(&mut th.stack, ret_to + count);
                th.stack.copy_within(start..start + count, ret_to);
                th.top = ret_to + count;
            } else {
                let want = (frame.nres - 1) as usize;
                ensure_len(&mut th.stack, ret_to + want);
                for i in 0..want {
                    th.stack[ret_to + i] = if i < count {
                        th.stack[start + i]
                    } else {
                        Value::Nil
                    };
                }
            }
        }
        RetShape::ToBool | RetShape::ToNotBool => {
            let v = if count > 0 {
                th.stack[start]
            } else {
                Value::Nil
            };
            let b = if frame.shape == RetShape::ToBool {
                v.truthy()
            } else {
                !v.truthy()
            };
            place_results(th, ret_to, frame.nres, &[Value::Bool(b)]);
        }
        RetShape::PrependTrue | RetShape::PrependFalse => {
            let flag = Value::Bool(frame.shape == RetShape::PrependTrue);
            if frame.nres == 0 {
                ensure_len(&mut th.stack, ret_to + 1 + count);
                // copy backward-safe: results sit above ret_to
                th.stack.copy_within(start..start + count, ret_to + 1);
                th.stack[ret_to] = flag;
                th.top = ret_to + 1 + count;
            } else {
                let want = (frame.nres - 1) as usize;
                ensure_len(&mut th.stack, ret_to + want);
                if want > 0 {
                    let n = count.min(want - 1);
                    th.stack.copy_within(start..start + n, ret_to + 1);
                    for i in n..want - 1 {
                        th.stack[ret_to + 1 + i] = Value::Nil;
                    }
                    th.stack[ret_to] = flag;
                }
            }
        }
    }
}

/// Lua `==` (without metamethods): raw equality plus cross int/float.
pub(crate) fn values_equal(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => {
            float_to_exact_int(y) == Some(x)
        }
        _ => a == b,
    }
}

/// Converts to an integer for bitwise ops (floats with exact integral value).
fn to_int(v: Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(i),
        Value::Float(f) => float_to_exact_int(f),
        _ => None,
    }
}

fn to_float(v: Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(i as f64),
        Value::Float(f) => Some(f),
        _ => None,
    }
}

/// Compares an integer against a float exactly across the full i64/f64 ranges.
/// `or_equal` selects `i <= f`; otherwise `i < f`.
fn int_cmp_float(i: i64, f: f64, or_equal: bool) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= F64_TWO_POW_63 {
        return true; // f >= 2^63 > any i64
    }
    if f < -F64_TWO_POW_63 {
        return false;
    }
    let ff = f.floor();
    let fi = ff as i64;
    if or_equal {
        // `ff` is the floor, so `i <= f` iff `i <= ff`.
        i <= fi
    } else {
        i < fi || (i == fi && f > ff)
    }
}

/// `i < f`.
fn int_lt_float(i: i64, f: f64) -> bool {
    int_cmp_float(i, f, false)
}

/// `i <= f`.
fn int_le_float(i: i64, f: f64) -> bool {
    int_cmp_float(i, f, true)
}

/// Converts a float limit of an integer `for` loop per Lua 5.4 (floor/ceil
/// toward the loop interior, clamped). `None` means the loop is empty.
fn for_int_limit(f: f64, step_positive: bool) -> Option<i64> {
    if f.is_nan() {
        return None;
    }
    if step_positive {
        if f < -F64_TWO_POW_63 {
            None
        } else if f >= F64_TWO_POW_63 {
            Some(i64::MAX)
        } else {
            Some(f.floor() as i64)
        }
    } else if f >= F64_TWO_POW_63 {
        None
    } else if f < -F64_TWO_POW_63 {
        Some(i64::MIN)
    } else {
        Some(f.ceil() as i64)
    }
}

/// Numeric coercion for arithmetic operators: numbers pass through and
/// numeric strings are parsed. Lua 5.4 delegates string coercion to the
/// string library's arithmetic metamethods; bitwise operators stay strict.
fn to_arith_number(strings: &Strings, v: Value) -> Option<Value> {
    match v {
        Value::Int(_) | Value::Float(_) => Some(v),
        Value::Str(id) => crate::stdlib::parse_number(strings.get(id)),
        _ => None,
    }
}

fn arith(strings: &Strings, op: ArithOp, a: Value, b: Value) -> Result<Value, String> {
    use ArithOp::{Add, BAnd, BOr, BXor, Div, IDiv, Mod, Mul, Pow, Shl, Shr, Sub};
    let num_err = |v: Value| format!("attempt to perform arithmetic on a {} value", v.type_name());
    let int_err = |v: Value| match v {
        Value::Float(_) => "number has no integer representation".to_string(),
        _ => format!(
            "attempt to perform bitwise operation on a {} value",
            v.type_name()
        ),
    };
    let na = to_arith_number(strings, a);
    let nb = to_arith_number(strings, b);
    let as_float = |n: Option<Value>, v: Value| -> Result<f64, String> {
        to_float(n.ok_or_else(|| num_err(v))?).ok_or_else(|| num_err(v))
    };
    match op {
        Add | Sub | Mul => {
            if let (Some(Value::Int(x)), Some(Value::Int(y))) = (na, nb) {
                Ok(Value::Int(match op {
                    Add => x.wrapping_add(y),
                    Sub => x.wrapping_sub(y),
                    Mul => x.wrapping_mul(y),
                    _ => unreachable!(),
                }))
            } else {
                let x = as_float(na, a)?;
                let y = as_float(nb, b)?;
                Ok(Value::Float(match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    _ => unreachable!(),
                }))
            }
        }
        Div => {
            let x = as_float(na, a)?;
            let y = as_float(nb, b)?;
            Ok(Value::Float(x / y))
        }
        Pow => {
            let x = as_float(na, a)?;
            let y = as_float(nb, b)?;
            Ok(Value::Float(x.powf(y)))
        }
        IDiv => {
            if let (Some(Value::Int(x)), Some(Value::Int(y))) = (na, nb) {
                if y == 0 {
                    return Err("attempt to divide by zero".into());
                }
                let q = x.wrapping_div(y);
                let q = if x.wrapping_rem(y) != 0 && (x < 0) != (y < 0) {
                    q - 1
                } else {
                    q
                };
                Ok(Value::Int(q))
            } else {
                let x = as_float(na, a)?;
                let y = as_float(nb, b)?;
                Ok(Value::Float((x / y).floor()))
            }
        }
        Mod => {
            if let (Some(Value::Int(x)), Some(Value::Int(y))) = (na, nb) {
                if y == 0 {
                    return Err("attempt to perform 'n%0'".into());
                }
                let r = x.wrapping_rem(y);
                Ok(Value::Int(if r != 0 && (r < 0) != (y < 0) {
                    r + y
                } else {
                    r
                }))
            } else {
                let x = as_float(na, a)?;
                let y = as_float(nb, b)?;
                let r = x % y;
                Ok(Value::Float(if r != 0.0 && (r < 0.0) != (y < 0.0) {
                    r + y
                } else {
                    r
                }))
            }
        }
        BAnd | BOr | BXor => {
            let x = to_int(a).ok_or_else(|| int_err(a))?;
            let y = to_int(b).ok_or_else(|| int_err(b))?;
            Ok(Value::Int(match op {
                BAnd => x & y,
                BOr => x | y,
                BXor => x ^ y,
                _ => unreachable!(),
            }))
        }
        Shl | Shr => {
            let x = to_int(a).ok_or_else(|| int_err(a))?;
            let y = to_int(b).ok_or_else(|| int_err(b))?;
            // Lua shifts are logical; a negative count shifts the other way,
            // and counts >= 64 produce zero
            let n = if op == Shr {
                y.checked_neg().unwrap_or(i64::MAX)
            } else {
                y
            };
            Ok(Value::Int(shift_left_logical(x, n)))
        }
    }
}

fn shift_left_logical(x: i64, n: i64) -> i64 {
    if n <= -64 || n >= 64 {
        0
    } else if n >= 0 {
        ((x as u64) << n) as i64
    } else {
        ((x as u64) >> -n) as i64
    }
}

impl<C> Lua<C> {
    #[must_use]
    pub fn new() -> Self {
        let mut strings = Strings::default();
        let mm_names = Mm::iter()
            .map(|m| strings.intern_fixed(<&str>::from(m).as_bytes()))
            .collect();
        let mut lua = Lua {
            strings,
            tables: vec![Table::default()],
            closures: Vec::new(),
            natives: Vec::new(),
            upvals: Vec::new(),
            threads: Vec::new(),
            userdata: Vec::new(),
            globals: TableId(0),
            string_meta: None,
            mm_names,
            current_thread: ThreadId(u32::MAX),
            type_metas: [None; 5],
            switch_to: None,
            rng: [0; 4],
            tables_live: vec![true],
            tables_free: Vec::new(),
            closures_live: Vec::new(),
            closures_free: Vec::new(),
            upvals_live: Vec::new(),
            upvals_free: Vec::new(),
            threads_live: Vec::new(),
            threads_free: Vec::new(),
            userdata_live: Vec::new(),
            userdata_free: Vec::new(),
            natives_live: Vec::new(),
            natives_free: Vec::new(),
            exec_roots: std::collections::HashMap::new(),
            anchors: Vec::new(),
            next_execution_id: 0,
            current_execution: None,
            active_context: None,
            suspended_wait: None,
            empty_proto: Rc::new(Proto {
                code: Vec::new(),
                source: "<empty>".into(),
                lines: Vec::new(),
                consts: Vec::new(),
                protos: Vec::new(),
                upvals: Vec::new(),
                upval_names: Vec::new(),
                nparams: 0,
                is_vararg: false,
                max_regs: 0,
                reg_extent: Vec::new(),
                name: String::new(),
                linedefined: 0,
                lastlinedefined: 0,
                call_names: Vec::new(),
            }),
            allocs_since_gc: 0,
            str_bytes_at_gc: 0,
            gc_alloc_threshold: 50_000,
            memory_limit: None,
            file_reader: None,
            gc_running: true,
            gc_mode: 1, // PUC 5.4 defaults to generational
            gc_pause: 200,
            gc_stepmul: 100,
            pending_finalizers: Vec::new(),
            finalizer_depth: None,
            finalizer_running: false,
            close_job: None,
            print_job: None,
            format_job: None,
            handler_unwind: None,
            hook_suppress: 0,
            host: None,
            file_meta: None,
            io_input: None,
            io_output: None,
            exit_request: None,
        };
        lua.seed_random(0x536c_6577_5f5f_5f31); // "Slew____1"
        crate::stdlib::install(&mut lua);
        lua
    }

    /// Reseeds the PRNG with PUC 5.4's `setseed`, using `0` for the second
    /// 64-bit word. Equivalent to `seed_random_pair(seed, 0)`.
    pub fn seed_random(&mut self, seed: u64) {
        self.seed_random_pair(seed, 0);
    }

    /// Seeds the PRNG with two 64-bit words exactly like PUC 5.4's `setseed`:
    /// state `{n1, 0xff, n2, 0}`, then 16 discarded `nextrand` calls to spread
    /// the seed. (`0xff` avoids the all-zero state.)
    pub fn seed_random_pair(&mut self, n1: u64, n2: u64) {
        self.rng = [n1, 0xff, n2, 0];
        for _ in 0..16 {
            self.next_random();
        }
    }

    pub fn next_random(&mut self) -> u64 {
        // xoshiro256**
        let s = &mut self.rng;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Parses and compiles a script. No code runs.
    ///
    /// # Errors
    ///
    /// Returns an error if the source fails to parse or compile.
    pub fn load(&mut self, src: impl AsRef<[u8]>) -> Result<Chunk, Error> {
        self.load_named("chunk", src)
    }

    /// Like [`Lua::load`] with an explicit chunk name for error messages.
    ///
    /// # Errors
    ///
    /// Returns an error if the source fails to parse or compile.
    pub fn load_named(&mut self, name: &str, src: impl AsRef<[u8]>) -> Result<Chunk, Error> {
        let block = parse(src.as_ref())?;
        let proto = compile(&block, &mut self.strings, name)?;
        Ok(Chunk { proto })
    }

    /// Instantiates a chunk with an execution context. Nothing runs until
    /// [`Execution::step`] is called.
    pub fn execute_with_context(&mut self, chunk: &Chunk, context: C) -> Execution<C> {
        self.execute_inner(chunk, context)
    }

    /// Instantiates a compiled chunk as a fresh main thread with its frame
    /// stack built, registering it as a GC root for as long as it is live.
    fn instantiate_thread(&mut self, chunk: &Chunk) -> ThreadId {
        let env = self.new_upval(Upval::Closed(Value::Table(self.globals)));
        let cid = self.alloc_closure(LuaClosure {
            proto: chunk.proto.clone(),
            upvals: vec![env],
        });
        let mut th = Thread {
            is_main: true,
            ..Default::default()
        };
        th.stack.resize(chunk.proto.max_regs as usize, Value::Nil);
        th.frames.push(Frame::Lua(LuaFrame {
            closure: cid,
            proto: chunk.proto.clone(),
            pc: 0,
            base: 0,
            ret_to: 0,
            nres: 0,
            shape: RetShape::Normal,
            protected: false,
            handler: None,
            handler_guard: false,
            pending: Vec::new(),
            tbc: Vec::new(),
            varargs: Vec::new(),
            tailcall: false,
            call_meta: None,
            last_line: -1,
            is_hook: false,
        }));
        let tid = self.alloc_thread(th);
        // GC root for as long as the execution is live
        self.exec_roots.insert(tid.0, tid.0);
        tid
    }

    fn execute_inner(&mut self, chunk: &Chunk, context: C) -> Execution<C> {
        let tid = self.instantiate_thread(chunk);
        let id = ExecutionId(self.next_execution_id);
        self.next_execution_id = self.next_execution_id.wrapping_add(1);
        Execution {
            thread: tid,
            current: tid,
            debt: 0,
            finished: false,
            id,
            context: Some(context),
        }
    }

    /// Runs a compiled chunk to completion with no execution context. The
    /// stdlib preludes use this at installation time: they are plain Lua that
    /// calls no suspendable natives, so they need no host context.
    ///
    /// # Errors
    ///
    /// Returns the runtime error if the chunk raises one.
    ///
    /// # Panics
    ///
    /// Panics if the chunk suspends on a native wait (the preludes never do).
    pub(crate) fn run_chunk(&mut self, chunk: &Chunk) -> Result<(), Error> {
        let tid = self.instantiate_thread(chunk);
        let result = loop {
            let mut fuel = i64::MAX;
            match self.run(tid, &mut fuel) {
                Ok(RunOutcome::Done(_)) => break Ok(()),
                Ok(RunOutcome::Pending(_)) => {}
                Ok(RunOutcome::Waiting(..)) => {
                    self.exec_roots.remove(&tid.0);
                    panic!("prelude cannot wait on the host");
                }
                Err(e) => break Err(Error::Runtime(e)),
            }
        };
        self.exec_roots.remove(&tid.0);
        result
    }

    /// Instantiates a compiled chunk as a function value whose `_ENV` is
    /// `env`: `None` means the globals table, while `Some(Value::Nil)` is a
    /// real nil environment (as `load(chunk, name, mode, nil)` produces).
    /// The result is a normal Lua function: callable, closure-capturing, and
    /// GC-managed, which is what embedders need to populate
    /// `package.preload` with compiled modules.
    pub fn make_function(&mut self, chunk: &Chunk, env: Option<Value>) -> Value {
        let env = env.unwrap_or(Value::Table(self.globals));
        let up = self.new_upval(Upval::Closed(env));
        let cid = self.alloc_closure(LuaClosure {
            proto: chunk.proto.clone(),
            upvals: vec![up],
        });
        Value::Closure(cid)
    }

    /// Builds a closure from an already-decoded `Proto` (binary chunks). The
    /// first upvalue receives `env` (the globals table when absent) and any
    /// further upvalues start closed over `nil`, matching PUC's `lua_load`.
    pub(crate) fn make_function_from_proto(
        &mut self,
        proto: Rc<Proto>,
        env: Option<Value>,
    ) -> Value {
        let first = env.unwrap_or(Value::Table(self.globals));
        let mut upvals = Vec::with_capacity(proto.upvals.len());
        for i in 0..proto.upvals.len() {
            let v = if i == 0 { first } else { Value::Nil };
            upvals.push(self.new_upval(Upval::Closed(v)));
        }
        let cid = self.alloc_closure(LuaClosure { proto, upvals });
        Value::Closure(cid)
    }

    /// Installs the host reader behind `loadfile`, `dofile`, and
    /// `package.searchpath`. `Ok(None)` means "not found"; `Err` is a hard
    /// failure. Without a reader those functions report "cannot open" and
    /// `require` resolves only `package.preload` and already-loaded modules:
    /// the interpreter has no ambient filesystem access.
    pub fn set_file_reader(
        &mut self,
        reader: impl FnMut(&str) -> Result<Option<Vec<u8>>, String> + 'static,
    ) {
        self.file_reader = Some(Box::new(reader));
    }

    /// Reads a module source through the installed reader; no reader means
    /// "not found".
    pub(crate) fn read_file(&mut self, path: &str) -> Result<Option<Vec<u8>>, String> {
        match &mut self.file_reader {
            Some(reader) => reader(path),
            None => Ok(None),
        }
    }

    /// Registers a native function as a global.
    ///
    /// # Panics
    ///
    /// Panics if the global table rejects the new entry.
    pub fn register_native(&mut self, name: &str, f: NativeFn<C>) -> Value {
        let v = self.add_native(name, f);
        self.set_global(name, v);
        v
    }

    /// Adds a native function without binding it to a global.
    pub fn add_native(&mut self, name: &str, f: NativeFn<C>) -> Value {
        self.add_native_kind(name, NativeKind::Plain(f))
    }

    /// Registers a native that may suspend its calling thread by returning
    /// [`NativeOutcome::Wait`]. The wait is completed with
    /// [`Execution::complete_native`]; on a yieldable coroutine it parks only
    /// that coroutine (see [`NativeOutcome::Wait`]).
    ///
    /// # Panics
    ///
    /// Panics if the global table rejects the new entry.
    pub fn register_suspendable_native(&mut self, name: &str, f: SuspendableNativeFn<C>) -> Value {
        let v = self.add_suspendable_native(name, f);
        self.set_global(name, v);
        v
    }

    /// Adds a suspendable native without binding it to a global. See
    /// [`Lua::register_suspendable_native`].
    pub fn add_suspendable_native(&mut self, name: &str, f: SuspendableNativeFn<C>) -> Value {
        self.add_native_kind(name, NativeKind::Suspendable(f))
    }

    pub(crate) fn add_native_kind(&mut self, name: &str, kind: NativeKind<C>) -> Value {
        self.allocs_since_gc += 1;
        let n = Native {
            name: name.into(),
            kind,
        };
        let id = if let Some(i) = self.natives_free.pop() {
            self.natives[i as usize] = n;
            self.natives_live[i as usize] = true;
            NativeId(i)
        } else {
            self.natives.push(n);
            self.natives_live.push(true);
            NativeId(self.natives.len() as u32 - 1)
        };
        Value::Native(id)
    }

    pub(crate) fn register_intrinsic(&mut self, name: &str, i: Intrinsic) -> Value {
        let v = self.add_native_kind(name, NativeKind::Intrinsic(i));
        self.set_global(name, v);
        v
    }

    pub fn new_string(&mut self, s: &[u8]) -> Value {
        self.allocs_since_gc += 1;
        Value::Str(self.strings.new_string(s))
    }

    pub fn new_table(&mut self) -> Value {
        self.allocs_since_gc += 1;
        let id = if let Some(i) = self.tables_free.pop() {
            self.tables[i as usize] = Table::default();
            self.tables_live[i as usize] = true;
            TableId(i)
        } else {
            self.tables.push(Table::default());
            self.tables_live.push(true);
            TableId(self.tables.len() as u32 - 1)
        };
        Value::Table(id)
    }

    /// Allocates a userdata slot. The payload/metatable are supplied by the
    /// caller; GC liveness is tracked like any other arena object.
    pub(crate) fn alloc_userdata(&mut self, u: Userdata) -> UserdataId {
        self.allocs_since_gc += 1;
        if let Some(i) = self.userdata_free.pop() {
            self.userdata[i as usize] = u;
            self.userdata_live[i as usize] = true;
            UserdataId(i)
        } else {
            self.userdata.push(u);
            self.userdata_live.push(true);
            UserdataId(self.userdata.len() as u32 - 1)
        }
    }

    /// True when a capability host has been installed (`io`/`os` exist).
    #[must_use]
    pub fn has_host(&self) -> bool {
        self.host.is_some()
    }

    /// Installs the embedder's capability host and registers `io`/`os`.
    /// Without a host those globals are absent, so the core has no authority.
    pub fn set_host(&mut self, host: impl Host + 'static) {
        if self.host.is_some() {
            self.clear_host();
        }
        self.host = Some(Box::new(host));
        crate::stdlib::install_host_libs(self);
    }

    /// Removes the host and unsets `io`/`os` (globals and `package.loaded`).
    pub fn clear_host(&mut self) {
        self.host = None;
        self.file_meta = None;
        self.io_input = None;
        self.io_output = None;
        self.set_global("io", Value::Nil);
        self.set_global("os", Value::Nil);
        let loaded_key = self.new_string(b"loaded");
        let pkg = self.get_global("package");
        let loaded = self.table_get(pkg, loaded_key);
        if let Value::Table(id) = loaded {
            let k = self.new_string(b"io");
            let _ = self.tables[id.0 as usize].set(k, Value::Nil);
            let k = self.new_string(b"os");
            let _ = self.tables[id.0 as usize].set(k, Value::Nil);
        }
    }

    /// Takes (and clears) an `os.exit` request, if the script asked to exit.
    pub fn take_exit_request(&mut self) -> Option<i64> {
        self.exit_request.take()
    }

    pub(crate) fn alloc_closure(&mut self, c: LuaClosure) -> ClosId {
        self.allocs_since_gc += 1;
        if let Some(i) = self.closures_free.pop() {
            self.closures[i as usize] = c;
            self.closures_live[i as usize] = true;
            ClosId(i)
        } else {
            self.closures.push(c);
            self.closures_live.push(true);
            ClosId(self.closures.len() as u32 - 1)
        }
    }

    fn alloc_thread(&mut self, th: Thread) -> ThreadId {
        self.allocs_since_gc += 1;
        if let Some(i) = self.threads_free.pop() {
            self.threads[i as usize] = th;
            self.threads_live[i as usize] = true;
            ThreadId(i)
        } else {
            self.threads.push(th);
            self.threads_live.push(true);
            ThreadId(self.threads.len() as u32 - 1)
        }
    }

    /// Creates a coroutine from a function value (for `coroutine.create`).
    pub(crate) fn create_coroutine(&mut self, f: Value) -> Value {
        let mut th = Thread::default();
        th.stack.push(f); // consumed on first resume
        th.top = 1;
        th.status = CoStatus::Start;
        Value::Thread(self.alloc_thread(th))
    }

    fn new_upval(&mut self, u: Upval) -> UpvalId {
        self.allocs_since_gc += 1;
        if let Some(i) = self.upvals_free.pop() {
            self.upvals[i as usize] = u;
            self.upvals_live[i as usize] = true;
            UpvalId(i)
        } else {
            self.upvals.push(u);
            self.upvals_live.push(true);
            UpvalId(self.upvals.len() as u32 - 1)
        }
    }

    #[must_use]
    pub fn get_global(&self, name: &str) -> Value {
        match self.strings.lookup(name.as_bytes()) {
            Some(id) => self.tables[self.globals.0 as usize].get(Value::Str(id)),
            None => Value::Nil, // a name never interned can't be a set global
        }
    }

    /// Sets a global variable.
    ///
    /// # Panics
    ///
    /// Panics if the global table rejects the new entry.
    pub fn set_global(&mut self, name: &str, v: Value) {
        let k = self.new_string(name.as_bytes());
        self.tables[self.globals.0 as usize].set(k, v).unwrap();
    }

    /// Raw table read (no metamethods).
    #[must_use]
    pub fn table_get(&self, t: Value, k: Value) -> Value {
        match t {
            Value::Table(id) => self.tables[id.0 as usize].get(k),
            _ => Value::Nil,
        }
    }

    #[must_use]
    pub fn str_bytes(&self, v: Value) -> Option<&[u8]> {
        match v {
            Value::Str(id) => Some(self.strings.get(id)),
            _ => None,
        }
    }

    /// `luaL_tolstring`'s fallback path, used after `__tostring` has been
    /// ruled out: a string `__name` field replaces the type tag, then the
    /// default rendering (`name: 0x...`). Strings keep their raw rendering
    /// (PUC only consults `__tostring` for them), and `display_value`'s
    /// output is used when there is no `__name`.
    pub(crate) fn tostring_default(&mut self, v: Value) -> String {
        let custom = match v {
            Value::Table(_)
            | Value::Closure(_)
            | Value::Native(_)
            | Value::Thread(_)
            | Value::Userdata(_) => match self.metamethod_pub(v, "__name") {
                Value::Str(id) => Some(self.strings.get_str_lossy(id).into_owned()),
                _ => None,
            },
            _ => None,
        };
        match custom {
            Some(name) => {
                let ptr = match v {
                    Value::Table(t) => t.0,
                    Value::Closure(c) => c.0,
                    Value::Thread(t) => t.0,
                    Value::Native(n) => n.0,
                    Value::Userdata(u) => u.0,
                    _ => unreachable!(),
                };
                format!("{name}: 0x{ptr:08x}")
            }
            None => self.display_value(v),
        }
    }

    /// Human-readable rendering of a value (like raw `tostring`, lossy for
    /// non-UTF-8 strings; does not invoke `__tostring`).
    #[must_use]
    pub fn display_value(&self, v: Value) -> String {
        match v {
            Value::Nil => "nil".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(_) | Value::Float(_) => fmt_number(v),
            Value::Str(id) => self.strings.get_str_lossy(id).into_owned(),
            Value::Table(t) => format!("table: 0x{:08x}", t.0),
            Value::Closure(c) => format!("function: 0x{:08x}", c.0),
            Value::Native(n) => format!("function: builtin: {}", self.natives[n.0 as usize].name),
            Value::Thread(t) => format!("thread: 0x{:08x}", t.0),
            Value::Userdata(u) => format!("userdata: 0x{:08x}", u.0),
        }
    }

    // ---- metatables ----

    #[must_use]
    pub fn get_metatable(&self, v: Value) -> Option<TableId> {
        match v {
            Value::Table(t) => self.tables[t.0 as usize].metatable,
            Value::Str(_) => self.string_meta,
            Value::Nil => self.type_metas[0],
            Value::Bool(_) => self.type_metas[1],
            Value::Int(_) | Value::Float(_) => self.type_metas[2],
            Value::Closure(_) | Value::Native(_) => self.type_metas[3],
            Value::Thread(_) => self.type_metas[4],
            Value::Userdata(u) => self.userdata[u.0 as usize].metatable,
        }
    }

    /// Raw metatable assignment (no `__metatable` guard), used by
    /// `debug.setmetatable`. Tables and strings have per-value storage; all
    /// other types share a per-type metatable.
    pub(crate) fn set_raw_metatable(&mut self, v: Value, mt: Option<TableId>) {
        match v {
            Value::Table(t) => self.tables[t.0 as usize].metatable = mt,
            Value::Str(_) => self.string_meta = mt,
            Value::Nil => self.type_metas[0] = mt,
            Value::Bool(_) => self.type_metas[1] = mt,
            Value::Int(_) | Value::Float(_) => self.type_metas[2] = mt,
            Value::Closure(_) | Value::Native(_) => self.type_metas[3] = mt,
            Value::Thread(_) => self.type_metas[4] = mt,
            Value::Userdata(u) => self.userdata[u.0 as usize].metatable = mt,
        }
    }

    // ---- debug library (intrinsics) ----

    /// `debug.getinfo([thread,] f [, what])`.
    fn debug_getinfo(
        &mut self,
        th: &Thread,
        target: Option<ThreadId>,
        f: Option<Value>,
        what: Value,
        argno: usize,
    ) -> Result<Value, String> {
        const WHO: &str = "debug.getinfo";
        let t = self.new_table();
        let Value::Table(tid) = t else { unreachable!() };
        match f {
            Some(Value::Closure(cid)) => {
                let what = self.debug_what(what, argno + 1, WHO)?;
                let proto = self.closures[cid.0 as usize].proto.clone();
                self.fill_lua_info(tid, &proto, Value::Closure(cid), -1, false, None, &what);
            }
            Some(Value::Native(_)) => {
                let what = self.debug_what(what, argno + 1, WHO)?;
                self.fill_native_info(tid, f.unwrap(), &what);
            }
            _ => {
                // Non-function: the argument is a stack level. PUC coerces it
                // (rejecting fractions) before validating `what`.
                let level = self.debug_check_int(f, argno, WHO)?;
                let what = self.debug_what(what, argno + 1, WHO)?;
                if !self.fill_level_info(tid, th, target, level, &what)? {
                    return Ok(Value::Nil);
                }
            }
        }
        Ok(t)
    }

    /// Parses `debug.getinfo`'s `what` option string, validating each letter.
    fn debug_what(&self, what: Value, argno: usize, who: &str) -> Result<Vec<u8>, String> {
        let bytes = match what {
            // PUC 5.4's default option string; 'r' yields ftransfer/ntransfer.
            Value::Nil => b"flnSrtu".to_vec(),
            Value::Str(s) => self.strings.get(s).to_vec(),
            other => {
                return Err(format!(
                    "bad argument #{argno} to '{who}' (string expected, got {})",
                    other.type_name()
                ));
            }
        };
        for &c in &bytes {
            if !matches!(c, b'S' | b'l' | b'u' | b't' | b'n' | b'f' | b'L' | b'r') {
                return Err(format!("bad argument #{argno} to '{who}' (invalid option)"));
            }
        }
        Ok(bytes)
    }

    /// `luaL_checkinteger`-style coercion for the debug APIs, with PUC's
    /// argument-numbered errors. `None` means the argument was absent (which
    /// PUC distinguishes from an explicit `nil`).
    fn debug_check_int(&self, v: Option<Value>, argno: usize, who: &str) -> Result<i64, String> {
        let Some(v) = v else {
            return Err(format!(
                "bad argument #{argno} to '{who}' (number expected, got no value)"
            ));
        };
        let n = match v {
            Value::Int(i) => return Ok(i),
            Value::Float(f) => Value::Float(f),
            Value::Str(s) => match crate::stdlib::parse_number(self.strings.get(s)) {
                Some(n) => n,
                None => {
                    return Err(format!(
                        "bad argument #{argno} to '{who}' (number expected, got string)"
                    ));
                }
            },
            other => {
                return Err(format!(
                    "bad argument #{argno} to '{who}' (number expected, got {})",
                    other.type_name()
                ));
            }
        };
        match n {
            Value::Int(i) => Ok(i),
            Value::Float(f) => crate::value::float_to_exact_int(f).ok_or_else(|| {
                format!("bad argument #{argno} to '{who}' (number has no integer representation)")
            }),
            _ => unreachable!(),
        }
    }

    fn fill_level_info(
        &mut self,
        tid: TableId,
        th: &Thread,
        target: Option<ThreadId>,
        level: i64,
        what: &[u8],
    ) -> Result<bool, String> {
        let snap = self.debug_level_snapshot(th, target, level);
        let Some(snap) = snap else {
            return Ok(false);
        };
        match snap {
            LevelFrame::Native => {
                let func = self.debug_getinfo_fn();
                self.fill_native_info(tid, func, what);
            }
            LevelFrame::Lua {
                proto,
                pc,
                closure,
                tailcall,
                name,
            } => {
                let cur = proto.lines.get(pc.wrapping_sub(1)).copied().unwrap_or(0) as i64;
                self.fill_lua_info(
                    tid,
                    &proto,
                    Value::Closure(closure),
                    cur,
                    tailcall,
                    name,
                    what,
                );
            }
        }
        Ok(true)
    }

    /// Lua frame at `level` in `target` (or the running thread), or the C
    /// function `getinfo` at current level 0. Boundary frames are invisible to
    /// the debug API. `None` when out of range.
    fn debug_level_snapshot(
        &self,
        th: &Thread,
        target: Option<ThreadId>,
        level: i64,
    ) -> Option<LevelFrame> {
        let current = target.is_none() || target == Some(self.current_thread);
        if level < 0 {
            return None;
        }
        let frames: &[Frame] = match target {
            Some(t) if t != self.current_thread => {
                self.threads.get(t.0 as usize).map(|x| &x.frames[..])
            }
            _ => Some(&th.frames[..]),
        }?;
        let level = level as usize;
        // Boundary frames are invisible to the debug API, so levels are counted
        // over Lua frames only. They are indexed by their position in the raw
        // stack so call-site name lookup below still sees the real caller.
        let lua: Vec<usize> = frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.lua().is_some())
            .map(|(i, _)| i)
            .collect();
        let idx = if current {
            if level == 0 {
                return Some(LevelFrame::Native);
            }
            if level > lua.len() {
                return None;
            }
            lua[lua.len() - level]
        } else {
            if level >= lua.len() {
                return None;
            }
            lua[lua.len() - 1 - level]
        };
        let lf = frames.get(idx)?.lua()?;
        let name = lf
            .call_meta
            .map(|(nw, nm)| (nw, Box::from(nm)))
            .or_else(|| {
                if idx > 0 {
                    frames[idx - 1].lua().and_then(|caller| {
                        caller
                            .proto
                            .call_names
                            .get(caller.pc.wrapping_sub(1))
                            .cloned()
                            .flatten()
                    })
                } else {
                    None
                }
            });
        Some(LevelFrame::Lua {
            proto: lf.proto.clone(),
            pc: lf.pc,
            closure: lf.closure,
            tailcall: lf.tailcall,
            name,
        })
    }

    fn info_set(&mut self, tid: TableId, name: &str, v: Value) {
        let k = self.new_string(name.as_bytes());
        self.tables[tid.0 as usize].set(k, v).unwrap();
    }

    fn fill_native_info(&mut self, tid: TableId, func: Value, what: &[u8]) {
        if what.contains(&b'S') {
            let src = self.new_string(b"=[C]");
            self.info_set(tid, "source", src);
            let ss = self.new_string(b"[C]");
            self.info_set(tid, "short_src", ss);
            self.info_set(tid, "linedefined", Value::Int(-1));
            self.info_set(tid, "lastlinedefined", Value::Int(-1));
            let w = self.new_string(b"C");
            self.info_set(tid, "what", w);
        }
        if what.contains(&b'l') {
            self.info_set(tid, "currentline", Value::Int(-1));
        }
        if what.contains(&b'u') {
            self.info_set(tid, "nups", Value::Int(0));
            self.info_set(tid, "nparams", Value::Int(0));
            self.info_set(tid, "isvararg", Value::Bool(true));
        }
        if what.contains(&b't') {
            self.info_set(tid, "istailcall", Value::Bool(false));
        }
        if what.contains(&b'r') {
            // No hook/transfer model: like a plain PUC call, both are zero.
            self.info_set(tid, "ftransfer", Value::Int(0));
            self.info_set(tid, "ntransfer", Value::Int(0));
        }
        if what.contains(&b'n') {
            self.info_set(tid, "name", Value::Nil);
            let nw = self.new_string(b"");
            self.info_set(tid, "namewhat", nw);
        }
        if what.contains(&b'f') {
            self.info_set(tid, "func", func);
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn fill_lua_info(
        &mut self,
        tid: TableId,
        proto: &Proto,
        func: Value,
        currentline: i64,
        istailcall: bool,
        name: Option<(&'static str, Box<str>)>,
        what: &[u8],
    ) {
        if what.contains(&b'S') {
            let src = self.new_string(proto.source.as_bytes());
            self.info_set(tid, "source", src);
            let ss = self.new_string(short_source(&proto.source).as_bytes());
            self.info_set(tid, "short_src", ss);
            self.info_set(tid, "linedefined", Value::Int(proto.linedefined as i64));
            self.info_set(
                tid,
                "lastlinedefined",
                Value::Int(proto.lastlinedefined as i64),
            );
            let w = if proto.linedefined == 0 {
                "main"
            } else {
                "Lua"
            };
            let wv = self.new_string(w.as_bytes());
            self.info_set(tid, "what", wv);
        }
        if what.contains(&b'l') {
            self.info_set(tid, "currentline", Value::Int(currentline));
        }
        if what.contains(&b'u') {
            self.info_set(tid, "nups", Value::Int(proto.upvals.len() as i64));
            self.info_set(tid, "nparams", Value::Int(proto.nparams as i64));
            self.info_set(tid, "isvararg", Value::Bool(proto.is_vararg));
        }
        if what.contains(&b't') {
            self.info_set(tid, "istailcall", Value::Bool(istailcall));
        }
        if what.contains(&b'r') {
            // Transfers describe hook/call argument movement; slew has no
            // hooks and never sets CIST_TRAN, so PUC reports zero here.
            self.info_set(tid, "ftransfer", Value::Int(0));
            self.info_set(tid, "ntransfer", Value::Int(0));
        }
        if what.contains(&b'n') {
            let namev = match &name {
                Some((_, nm)) => self.new_string(nm.as_bytes()),
                None => Value::Nil,
            };
            self.info_set(tid, "name", namev);
            let nw = self.new_string(name.map_or("", |(nw, _)| nw).as_bytes());
            self.info_set(tid, "namewhat", nw);
        }
        if what.contains(&b'f') {
            self.info_set(tid, "func", func);
        }
        if what.contains(&b'L') {
            let t = self.new_table();
            let Value::Table(at) = t else { unreachable!() };
            for &line in &proto.lines {
                self.tables[at.0 as usize]
                    .set(Value::Int(line as i64), Value::Bool(true))
                    .ok();
            }
            self.info_set(tid, "activelines", t);
        }
    }

    fn debug_getinfo_fn(&mut self) -> Value {
        let d = self.get_global("debug");
        let k = self.new_string(b"getinfo");
        self.table_get(d, k)
    }

    /// `debug.traceback([thread,] [message [, level]])`.
    fn debug_traceback(
        &mut self,
        th: &Thread,
        target: Option<ThreadId>,
        message: Value,
        level: Option<Value>,
        argno: usize,
    ) -> Result<Value, String> {
        let (msg, has_msg) = match message {
            Value::Nil => (String::new(), false),
            Value::Str(s) => (self.strings.get_str_lossy(s).into_owned(), true),
            // A non-string message is returned untouched; PUC skips level
            // validation in that case.
            other => return Ok(other),
        };
        let level = match level {
            None | Some(Value::Nil) => i64::from(target.is_none()),
            Some(v) => self.debug_check_int(Some(v), argno, "debug.traceback")?,
        };
        let current = target.is_none() || target == Some(self.current_thread);
        let frames: &[Frame] = match target {
            Some(t) if t != self.current_thread => match self.threads.get(t.0 as usize) {
                Some(x) => &x.frames,
                None => &[],
            },
            _ => &th.frames,
        };
        let mut out = String::new();
        if has_msg {
            out.push_str(&msg);
            out.push('\n');
        }
        out.push_str("stack traceback:");
        // Level 0 is the running frame of `target`; for the current thread it
        // behaves like level 1. Boundary frames are invisible to tracebacks, so
        // levels are counted over Lua frames only. A level past the stack shows
        // no frames (PUC `luaL_traceback`).
        let lua: Vec<usize> = frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.lua().is_some())
            .map(|(i, _)| i)
            .collect();
        let first = if current {
            lua.len().checked_sub(level.max(1) as usize)
        } else {
            lua.len().checked_sub(1 + level.max(0) as usize)
        };
        let Some(first) = first else {
            return Ok(self.new_string(out.as_bytes()));
        };
        for pos in (0..=first).rev() {
            let idx = lua[pos];
            let Frame::Lua(fr) = &frames[idx] else {
                continue;
            };
            let line = fr
                .proto
                .lines
                .get(fr.pc.wrapping_sub(1))
                .copied()
                .unwrap_or(0);
            let src = short_source(&fr.proto.source);
            let _ = write!(out, "\n\t{src}:{line}: in ");
            let name: Option<(&'static str, Box<str>)> = fr
                .call_meta
                .map(|(nw, nm)| (nw, Box::from(nm)))
                .or_else(|| {
                    if idx > 0 {
                        frames[idx - 1].lua().and_then(|caller| {
                            caller
                                .proto
                                .call_names
                                .get(caller.pc.wrapping_sub(1))
                                .cloned()
                                .flatten()
                        })
                    } else {
                        None
                    }
                });
            match name {
                Some(("metamethod", nm)) => {
                    let _ = write!(out, "metamethod '{nm}'");
                }
                Some((nw, nm)) => {
                    let _ = write!(out, "function '{nm}' ({nw})");
                }
                None if fr.proto.linedefined == 0 => out.push_str("main chunk"),
                None => {
                    let _ = write!(out, "function <{src}:{}>", fr.proto.linedefined);
                }
            }
        }
        Ok(self.new_string(out.as_bytes()))
    }

    fn debug_getupvalue(&mut self, th: &Thread, cid: ClosId, n: i64) -> Option<(String, Value)> {
        let c = &self.closures[cid.0 as usize];
        if n < 1 || n as usize > c.upvals.len() {
            return None;
        }
        let name = c
            .proto
            .upval_names
            .get(n as usize - 1)
            .map_or_else(|| "(...)".to_string(), std::string::ToString::to_string);
        let uid = c.upvals[n as usize - 1];
        let val = self.read_upval(self.current_thread, th, uid);
        Some((name, val))
    }

    fn debug_setupvalue(
        &mut self,
        th: &mut Thread,
        cid: ClosId,
        n: i64,
        v: Value,
    ) -> Option<String> {
        let c = &self.closures[cid.0 as usize];
        if n < 1 || n as usize > c.upvals.len() {
            return None;
        }
        let name = c
            .proto
            .upval_names
            .get(n as usize - 1)
            .map_or_else(|| "(...)".to_string(), std::string::ToString::to_string);
        let uid = c.upvals[n as usize - 1];
        let tid = self.current_thread;
        self.write_upval(tid, th, uid, v);
        Some(name)
    }

    fn debug_upvalueid(&self, cid: ClosId, n: i64) -> Value {
        let c = &self.closures[cid.0 as usize];
        if n < 1 || n as usize > c.upvals.len() {
            return Value::Nil;
        }
        Value::Int(c.upvals[n as usize - 1].0 as i64)
    }

    /// PUC's `checkupval` for the upvalue APIs: the index has already been
    /// coerced; a native (C) function is a valid function but has no
    /// upvalues, so any index on it is "invalid upvalue index". `argf` and
    /// `argnup` are 1-based argument positions for PUC's error wording.
    fn debug_check_upval(
        &self,
        f: Value,
        n: i64,
        argf: usize,
        argnup: usize,
        who: &str,
    ) -> Result<ClosId, String> {
        let cid = match f {
            Value::Closure(cid) => cid,
            Value::Native(_) => {
                return Err(format!(
                    "bad argument #{argnup} to '{who}' (invalid upvalue index)"
                ));
            }
            other => {
                return Err(format!(
                    "bad argument #{argf} to '{who}' (function expected, got {})",
                    other.type_name()
                ));
            }
        };
        if n < 1 || n as usize > self.closures[cid.0 as usize].upvals.len() {
            return Err(format!(
                "bad argument #{argnup} to '{who}' (invalid upvalue index)"
            ));
        }
        Ok(cid)
    }

    fn debug_upvaluejoin(
        &mut self,
        c1: ClosId,
        n1: i64,
        c2: ClosId,
        n2: i64,
    ) -> Result<(), String> {
        if n1 < 1 || n1 as usize > self.closures[c1.0 as usize].upvals.len() {
            return Err("bad argument #2 to 'debug.upvaluejoin' (invalid upvalue index)".into());
        }
        if n2 < 1 || n2 as usize > self.closures[c2.0 as usize].upvals.len() {
            return Err("bad argument #4 to 'debug.upvaluejoin' (invalid upvalue index)".into());
        }
        let uid = self.closures[c2.0 as usize].upvals[n2 as usize - 1];
        self.closures[c1.0 as usize].upvals[n1 as usize - 1] = uid;
        Ok(())
    }

    /// Metatable field lookup by name (for natives like setmetatable).
    pub(crate) fn metamethod_pub(&mut self, v: Value, name: &str) -> Value {
        match self.get_metatable(v) {
            Some(mt) => {
                let k = self.new_string(name.as_bytes());
                self.tables[mt.0 as usize].get(k)
            }
            None => Value::Nil,
        }
    }

    pub(crate) fn metamethod(&self, v: Value, mm: Mm) -> Value {
        match self.get_metatable(v) {
            Some(mt) => self.tables[mt.0 as usize].get(Value::Str(self.mm_names[mm as usize])),
            None => Value::Nil,
        }
    }

    /// Metatable field lookup by name that never interns, so it is safe to
    /// call while a collection is in progress (`__gc`, `__close`).
    fn raw_metafield(&self, v: Value, name: &str) -> Value {
        let Some(k) = self.strings.lookup(name.as_bytes()) else {
            return Value::Nil;
        };
        match self.get_metatable(v) {
            Some(mt) => self.tables[mt.0 as usize].get(Value::Str(k)),
            None => Value::Nil,
        }
    }

    fn binary_mm(&self, a: Value, b: Value, mm: Mm) -> Value {
        let m = self.metamethod(a, mm);
        if m != Value::Nil {
            return m;
        }
        self.metamethod(b, mm)
    }

    // ---- debug hooks (`debug.sethook`) ----

    /// Whether any hook event should fire right now: a hook is set and no
    /// hook frame is currently on the stack (PUC's `allowhook`).
    fn hook_any(th: &Thread) -> bool {
        th.hook.is_some()
            && !th
                .frames
                .iter()
                .any(|f| matches!(f, Frame::Lua(lf) if lf.is_hook))
    }

    /// Whether a specific event class (call/return/line) is enabled.
    fn hook_on(th: &Thread, bit: u8) -> bool {
        Self::hook_any(th) && th.hook_mask & bit != 0
    }

    /// Calls the current hook as an ordinary function, so it runs through the
    /// regular (suspendable) machinery and can itself call `pcall` etc.
    fn fire_hook(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        event: &str,
        line: i64,
    ) -> Result<(), VmError> {
        let Some(hook) = th.hook else {
            return Ok(());
        };
        let ev = self.new_string(event.as_bytes());
        let ln = if line < 0 {
            Value::Nil
        } else {
            Value::Int(line)
        };
        let before = th.frames.len();
        let scratch = scratch_base(th);
        // Installing the hook frame must not itself trigger a call hook.
        self.hook_suppress += 1;
        let r = self.call_value(th, hook, &[ev, ln], scratch, 1, RetShape::Normal, fuel);
        self.hook_suppress -= 1;
        r?;
        if th.frames.len() > before
            && let Some(Frame::Lua(lf)) = th.frames.last_mut()
        {
            lf.is_hook = true;
        }
        Ok(())
    }

    /// Completes a deferred `coroutine.yield`: marks the thread suspended,
    /// delivers its values to the resumer, and schedules the switch.
    fn perform_yield(&mut self, th: &mut Thread, job: &YieldJob) {
        th.status = CoStatus::Suspended;
        th.yield_ret = Some((job.ret_to, job.nres, job.shape));
        let parent = job.parent;
        let parent_th = &mut self.threads[parent.0 as usize];
        parent_th.status = CoStatus::Running;
        deliver_resume(parent_th, job.rr, true, &job.args);
        self.switch_to = Some(parent);
    }

    /// Whether `th` may yield to a resumer right now: a coroutine outside a
    /// non-yieldable C-call boundary. This is the base predicate shared with
    /// `coroutine.isyieldable`; see also [`Lua::wait_can_park`].
    fn thread_can_yield(th: &Thread) -> bool {
        !th.is_main && th.non_yieldable == 0
    }

    /// Whether a `NativeOutcome::Wait` on `th` may suspend just this coroutine
    /// (a yield to its resumer) rather than blocking the whole execution.
    ///
    /// False when `th` is the root thread or otherwise cannot yield
    /// ([`Lua::thread_can_yield`]), has no resumer to yield to, has a staged
    /// `coroutine.yield` in flight (a hook-fired wait must not interleave with
    /// the deferred yield), or is inside an active single-slot driver
    /// (`print`/`string.format`/`coroutine.close`/`__close`, an xpcall handler,
    /// a `__gc` finalizer). Interleaving another coroutine in those states
    /// would clobber VM state, so such waits conservatively block the
    /// execution.
    fn wait_can_park(&self, th: &Thread) -> bool {
        Self::thread_can_yield(th)
            && th.parent.is_some()
            && th.yield_job.is_none()
            && !self.finalizer_running
            && self.close_job.is_none()
            && self.print_job.is_none()
            && self.format_job.is_none()
            && self.handler_unwind.is_none()
            && self.finalizer_depth.is_none()
    }

    /// Suspends `th` (a coroutine parked on a native wait), marks its resumer
    /// runnable, delivers an empty success result to it, and returns the
    /// resumer's id so the caller can schedule the switch.
    ///
    /// Unlike [`Lua::perform_yield`], this leaves `th.yield_ret` unset: a
    /// parked thread resumes by completing its native call, not by receiving
    /// yield results, so [`Lua::resume_thread`] discards any resume arguments.
    fn park_thread(&mut self, th: &mut Thread) -> ThreadId {
        let parent = th.parent.expect("parkable thread has a parent");
        let rr = th.resume_ret.expect("resumed thread has resume_ret");
        th.status = CoStatus::Suspended;
        let parent_th = &mut self.threads[parent.0 as usize];
        parent_th.status = CoStatus::Running;
        deliver_resume(parent_th, rr, true, &[]);
        parent
    }

    // ---- dispatch ----

    /// Drives execution starting at `start`, following coroutine switches,
    /// until fuel runs out, the root thread finishes, or an error escapes
    /// every protection boundary.
    fn run(&mut self, start: ThreadId, fuel: &mut i64) -> Result<RunOutcome, RuntimeError> {
        let mut cur = start;
        let mut th = std::mem::take(&mut self.threads[cur.0 as usize]);
        th.status = CoStatus::Running;
        loop {
            self.current_thread = cur;
            match self.dispatch(cur, &mut th, fuel) {
                Ok(DispatchEnd::Pending) => {
                    self.threads[cur.0 as usize] = th;
                    return Ok(RunOutcome::Pending(cur));
                }

                Ok(DispatchEnd::Waiting(wait)) => {
                    self.threads[cur.0 as usize] = th;
                    return Ok(RunOutcome::Waiting(cur, wait));
                }

                Ok(DispatchEnd::Switch(next)) => {
                    self.threads[cur.0 as usize] = th;
                    cur = next;
                    th = std::mem::take(&mut self.threads[cur.0 as usize]);
                }

                Ok(DispatchEnd::Finished(vals)) => {
                    // thread ran to completion
                    th.status = CoStatus::Dead;
                    let parent = th.parent.take();
                    let rr = th.resume_ret.take();
                    self.threads[cur.0 as usize] = th;
                    match parent {
                        None => return Ok(RunOutcome::Done(vals)),
                        Some(p) => {
                            cur = p;
                            th = std::mem::take(&mut self.threads[cur.0 as usize]);
                            th.status = CoStatus::Running;
                            deliver_resume(&mut th, rr.unwrap(), true, &vals);
                        }
                    }
                }

                Err(mut e) => {
                    let mut root_line = None;
                    loop {
                        // capture the root script's own position before the
                        // frames are cleared below (on coroutine propagation
                        // this is the resume call site; for root-origin
                        // errors the frames are already gone and the
                        // dispatcher's `root_line` stands)
                        if root_line.is_none() && th.parent.is_none() {
                            root_line = Some(
                                th.frames
                                    .first()
                                    .and_then(|f| f.lua())
                                    .map_or(e.root_line, frame_line),
                            );
                        }
                        // error escaped this thread entirely; close its open
                        // upvalues (closures may outlive the thread) and kill it
                        self.close_upvals(&mut th, 0);
                        th.frames.clear();
                        th.status = CoStatus::Dead;
                        let parent = th.parent.take();
                        let rr = th.resume_ret.take();
                        th.stack.clear();
                        th.pending_native = None;
                        let death_err = self.err_value(&e);
                        self.threads[cur.0 as usize] = th;
                        self.threads[cur.0 as usize].close_error = Some(death_err);
                        match parent {
                            None => {
                                let root_line = root_line.unwrap_or(e.line);
                                return Err(self.materialize_error(&e, root_line));
                            }
                            Some(p) => {
                                cur = p;
                                self.current_thread = cur;
                                th = std::mem::take(&mut self.threads[cur.0 as usize]);
                                th.status = CoStatus::Running;
                                let rr = rr.unwrap();
                                if rr.wrap {
                                    // wrap propagates the error into the resumer
                                    match self.recover(&mut th, fuel, e) {
                                        Ok(()) => break,
                                        Err(e2) => {
                                            e = e2;
                                        }
                                    }
                                } else {
                                    let errv = self.err_value(&e);
                                    deliver_resume(&mut th, rr, false, &[errv]);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn materialize_error(&mut self, e: &VmError, root_line: u32) -> RuntimeError {
        let value = self.err_value(e);
        RuntimeError {
            message: self.display_value(value),
            value,
            line: e.line,
            root_line,
        }
    }

    fn err_value(&mut self, e: &VmError) -> Value {
        match &e.val {
            ErrVal::Val(v) => *v,
            ErrVal::Msg(m) => {
                let s = match &e.source {
                    Some(src) => format!("{src}:{}: {m}", e.line),
                    None => m.clone(),
                };
                self.new_string(s.as_bytes())
            }
        }
    }

    fn dispatch(
        &mut self,
        tid: ThreadId,
        th: &mut Thread,
        fuel: &mut i64,
    ) -> Result<DispatchEnd, VmError> {
        loop {
            if let Some(mut pending) = th.pending_native.take() {
                match pending.completion.take() {
                    None => {
                        let wait = pending.wait;
                        // The waiting execution may have changed if another
                        // execution resumed this coroutine while it was parked.
                        pending.exec = self
                            .current_execution
                            .expect("dispatch outside an execution");
                        th.pending_native = Some(pending);
                        if self.wait_can_park(th) {
                            // Still waiting, but we are on a coroutine: yield
                            // back to the resumer so other coroutines run.
                            let parent = self.park_thread(th);
                            return Ok(DispatchEnd::Switch(parent));
                        }
                        return Ok(DispatchEnd::Waiting(wait));
                    }
                    Some(Ok(values)) => {
                        place_shaped(th, pending.ret_to, pending.nres, pending.shape, &values);
                        if self.hook_suppress == 0 && Self::hook_on(th, HOOK_RETURN) {
                            self.fire_hook(th, fuel, "return", -1)?;
                        }
                    }
                    Some(Err(message)) => {
                        let e = VmError {
                            val: ErrVal::Msg(message),
                            line: line_of(th),
                            root_line: 0,
                            source: None,
                        };
                        self.recover(th, fuel, e)?;
                    }
                }
            }
            if *fuel <= 0 {
                return Ok(DispatchEnd::Pending);
            }
            self.service_finalizers(tid, th, fuel)?;
            if *fuel <= 0 {
                return Ok(DispatchEnd::Pending);
            }
            match self.exec_one(tid, th, fuel) {
                Ok(Flow::Continue) => {
                    if let Some(wait) = self.suspended_wait.take() {
                        return Ok(DispatchEnd::Waiting(wait));
                    }
                }
                Ok(Flow::Finished(vals)) => return Ok(DispatchEnd::Finished(vals)),

                Err(mut e) => {
                    // capture the script's own call site before `recover`
                    // pops the frames it unwinds
                    if e.root_line == 0 {
                        e.root_line = th
                            .frames
                            .first()
                            .and_then(|f| f.lua())
                            .map_or(0, frame_line);
                    }
                    self.recover(th, fuel, e)?;
                }
            }
            if let Some(next) = self.switch_to.take() {
                return Ok(DispatchEnd::Switch(next));
            }
        }
    }

    /// Handles an error: runs the nearest message handler at the error point
    /// when it can (so the handler sees the erroring frames, as PUC's
    /// `luaG_errormsg` does), otherwise unwinds to the nearest protected frame.
    fn recover(&mut self, th: &mut Thread, fuel: &mut i64, e: VmError) -> Result<(), VmError> {
        // A frame that already carries a staged `DeliverError` is a recovery
        // chain in progress: the error just raised came from a `__close`
        // handler while unwinding. PUC's `luaD_closeprotected` keeps closing
        // instead of unwinding past the chain, passing the newest error object
        // to the remaining handlers and the final delivery. Fold it in.
        let staged = th.frames.last().is_some_and(frame_staged_recovery);
        if staged {
            self.fold_staged(th, &e, Vec::new());
            return Ok(());
        }
        // An error raised *by* a message handler: PUC reports
        // "error in error handling" at the boundary that owns it.
        if self.handler_unwind.take().is_some() {
            let msg = self.new_string(b"error in error handling");
            let e2 = VmError {
                val: ErrVal::Val(msg),
                line: 0,
                root_line: 0,
                source: None,
            };
            return self.unwind(th, e2, true);
        }
        // Run the message handler now, before unwinding, when the error came
        // from a directly-invoked `__close` metamethod and no `__close`
        // handlers remain: PUC calls the handler at the error site, so
        // `debug.traceback` shows the close frame as `metamethod 'close'`.
        if let Some(idx) = th.frames.iter().rposition(Frame::is_protected) {
            let (handler, guard) = frame_boundary(&th.frames[idx]);
            if let Some(h) = handler
                && !guard
                && !th.frames[idx..].iter().any(frame_has_tbc)
                && th.frames[idx..].iter().any(frame_close_meta)
            {
                let errv = self.err_value(&e);
                let result_slot = scratch_base(th);
                ensure_len(&mut th.stack, result_slot + 1);
                th.frames
                    .last_mut()
                    .unwrap()
                    .pending_mut()
                    .push(Pending::UnwindAfterHandler { result_slot });
                self.handler_unwind = Some(HandlerUnwind);
                *fuel -= 2;
                return self.call_value(th, h, &[errv], result_slot, 2, RetShape::Normal, fuel);
            }
        }
        self.unwind(th, e, false)
    }

    /// Folds a `__close`-during-unwind error into an already staged recovery
    /// chain, refreshing the error object and prepending the newly unwound
    /// to-be-closed values.
    fn fold_staged(&mut self, th: &mut Thread, e: &VmError, to_close: Vec<Value>) {
        let new_err = self.err_value(e);
        let f = th.frames.last_mut().unwrap();
        for p in f.pending_mut().iter_mut() {
            match p {
                Pending::CallClose { err, .. }
                | Pending::DeliverError { err, .. }
                | Pending::Reraise { err } => {
                    *err = new_err;
                }
                _ => {}
            }
        }
        for v in to_close.into_iter().rev() {
            f.pending_mut().push(Pending::CallClose { v, err: new_err });
        }
    }

    /// Unwinds to the nearest protected frame, staging `__close` handlers and
    /// error delivery on the frame below. With `skip_handler` the error is
    /// delivered directly (`false, err`) instead of via the boundary handler.
    fn unwind(&mut self, th: &mut Thread, e: VmError, skip_handler: bool) -> Result<(), VmError> {
        // values of to-be-closed variables in unwound frames, innermost first
        let mut to_close: Vec<Value> = Vec::new();
        loop {
            // A frame carrying a staged `DeliverError` is a recovery chain in
            // progress: the error came from a `__close` handler while
            // unwinding. Fold it into the chain and keep closing.
            let staged = th.frames.last().is_some_and(frame_staged_recovery);
            if staged {
                self.fold_staged(th, &e, to_close);
                return Ok(());
            }
            match th.frames.last() {
                None => {
                    if to_close.is_empty() {
                        return Err(e);
                    }
                    // The error escaped every frame: PUC still runs the
                    // to-be-closed handlers before the thread dies. There is no
                    // frame below to stage them on, so synthesize one, run the
                    // closes, then re-raise the error.
                    let errv = self.err_value(&e);
                    th.frames.push(Frame::Boundary(BoundaryFrame {
                        base: th.top,
                        pending: Vec::new(),
                        boundary: None,
                    }));
                    let f = th.frames.last_mut().unwrap();
                    f.pending_mut().push(Pending::Reraise { err: errv });
                    for v in to_close.into_iter().rev() {
                        f.pending_mut().push(Pending::CallClose { v, err: errv });
                    }
                    return Ok(());
                }
                Some(f) if f.is_protected() => break,
                Some(_) => {
                    let f = th.frames.pop().unwrap();
                    if let Frame::Lua(lf) = &f {
                        for &r in lf.tbc.iter().rev() {
                            to_close.push(th.stack[lf.base + r as usize]);
                        }
                        self.close_upvals(th, lf.base);
                    }
                }
            }
        }
        let pf = th.frames.pop().unwrap();
        let (pf_ret_to, pf_nres, pf_handler, pf_guard) = match &pf {
            Frame::Lua(f) => {
                for &r in f.tbc.iter().rev() {
                    to_close.push(th.stack[f.base + r as usize]);
                }
                self.close_upvals(th, f.base);
                (f.ret_to, f.nres, f.handler, f.handler_guard)
            }
            Frame::Boundary(c) => {
                let b = c
                    .boundary
                    .as_ref()
                    .expect("protected boundary frame carries a boundary");
                (b.ret_to, b.nres, b.handler, false)
            }
        };
        let errv = self.err_value(&e);
        let Some(below) = th.frames.last_mut() else {
            // a protected root frame shouldn't exist (pcall always pushes
            // below an existing frame), but fail safe
            return Err(e);
        };
        if pf_guard {
            // The error came from a message handler: PUC reports
            // "error in error handling" rather than running another handler.
            below.pending_mut().push(Pending::DeliverErrErr {
                ret_to: pf_ret_to,
                nres: pf_nres,
            });
        } else {
            below.pending_mut().push(Pending::DeliverError {
                ret_to: pf_ret_to,
                nres: pf_nres,
                err: errv,
                handler: if skip_handler { None } else { pf_handler },
            });
        }
        // outermost closes are pushed first so the innermost pops first
        for v in to_close.into_iter().rev() {
            below
                .pending_mut()
                .push(Pending::CallClose { v, err: errv });
        }
        Ok(())
    }

    fn exec_one(
        &mut self,
        tid: ThreadId,
        th: &mut Thread,
        fuel: &mut i64,
    ) -> Result<Flow, VmError> {
        *fuel -= 1;
        // A boundary frame whose continuations have all run is finished: pop it
        // so the Lua frame below (owner of the protected call's result slots)
        // resumes. Boundary frames are only ever innermost while they carry
        // continuations.
        while th.frames.last().is_some_and(Frame::is_boundary)
            && th.frames.last().unwrap().pending().is_empty()
        {
            th.frames.pop();
        }
        // A coroutine whose body was a protected call (`coroutine.create(pcall)`)
        // has no Lua frame below its boundary: the delivered results
        // at slot 0 are the coroutine's return values.
        if th.frames.is_empty() {
            let top = th.top.min(th.stack.len());
            let vals = th.stack[..top].to_vec();
            th.stack.clear();
            th.top = 0;
            return Ok(Flow::Finished(vals));
        }
        // run continuations (e.g. a concat interrupted by a metamethod call,
        // staged __close handlers) before fetching the next instruction
        if !th.frames.last().unwrap().pending().is_empty() {
            let p = th.frames.last_mut().unwrap().pending_mut().pop().unwrap();
            match p {
                Pending::Concat { dst, base, n } => self.concat_run(th, fuel, dst, base, n)?,
                Pending::CloseTbc { from, err } => self.run_close_tbc(th, fuel, from, err)?,
                Pending::CallClose { v, err } => {
                    let mm = self.metamethod(v, Mm::Close);
                    if mm != Value::Nil {
                        let scratch = scratch_base(th);
                        self.call_value(th, mm, &[v, err], scratch, 1, RetShape::Normal, fuel)?;
                    }
                }
                Pending::DeliverError {
                    ret_to,
                    nres,
                    err,
                    handler,
                } => match handler {
                    None => {
                        place_results(th, ret_to, nres, &[Value::Bool(false), err]);
                    }
                    Some(h) => {
                        // Run the message handler under its own protection so
                        // that an error *inside* it (e.g. a stack overflow
                        // while it recurses) reports PUC's "error in error
                        // handling" instead of escaping the xpcall.
                        let before = th.frames.len();
                        let wb = scratch_base(th);
                        ensure_len(&mut th.stack, wb + 2);
                        th.stack[wb] = h;
                        th.stack[wb + 1] = err;
                        let r = self.do_call(
                            th,
                            fuel,
                            &CallSpec {
                                func_abs: wb,
                                argc: 1,
                                ret_to,
                                nres,
                                shape: RetShape::PrependFalse,
                                protected: true,
                                handler: None,
                                native_caller: true,
                            },
                        );
                        if let Ok(()) = r {
                            if th.frames.len() > before
                                && let Some(Frame::Lua(lf)) = th.frames.last_mut()
                            {
                                lf.handler_guard = true;
                            }
                        } else {
                            let msg = self.new_string(b"error in error handling");
                            place_results(th, ret_to, nres, &[Value::Bool(false), msg]);
                        }
                    }
                },
                Pending::DeliverErrErr { ret_to, nres } => {
                    let msg = self.new_string(b"error in error handling");
                    place_results(th, ret_to, nres, &[Value::Bool(false), msg]);
                }
                Pending::FinishReturn { start, count } => {
                    let frame = th.frames.pop().unwrap();
                    let frame = frame.as_lua();
                    self.close_upvals(th, frame.base);
                    if th.frames.is_empty() {
                        let vals = th.stack[start..start + count].to_vec();
                        th.stack.clear();
                        th.top = 0;
                        return Ok(Flow::Finished(vals));
                    }
                    deliver_return(th, frame, start, count);
                }
                Pending::TailReturn { start } => {
                    let count = th.top.saturating_sub(start);
                    let frame = th.frames.pop().unwrap();
                    let frame = frame.as_lua();
                    self.close_upvals(th, frame.base);
                    if th.frames.is_empty() {
                        let vals = th.stack[start..start + count].to_vec();
                        th.stack.clear();
                        th.top = 0;
                        return Ok(Flow::Finished(vals));
                    }
                    deliver_return(th, frame, start, count);
                }
                Pending::CloseStep => self.close_step(th, fuel)?,
                Pending::PrintStep => self.print_step(th, fuel)?,
                Pending::FinishTostring {
                    ret_to,
                    nres,
                    shape,
                    result_slot,
                } => {
                    let v = th.stack[result_slot];
                    let sv = match v {
                        Value::Str(_) => v,
                        Value::Int(_) | Value::Float(_) => {
                            let s = crate::value::fmt_number(v);
                            self.new_string(s.as_bytes())
                        }
                        _ => {
                            return Err(Self::rt_err(
                                th,
                                "'__tostring' must return a string".into(),
                            ));
                        }
                    };
                    place_shaped(th, ret_to, nres, shape, &[sv]);
                }
                Pending::FormatStep => self.format_step(th, fuel)?,
                Pending::PrependShape {
                    ret_to,
                    nres,
                    shape,
                } => {
                    let count = if nres == 0 {
                        th.top.saturating_sub(ret_to)
                    } else {
                        (nres - 1) as usize
                    };
                    let vals: Vec<Value> = th.stack[ret_to..ret_to + count].to_vec();
                    place_shaped(th, ret_to, nres, shape, &vals);
                }
                Pending::ReturnHookFire => {
                    if self.hook_suppress == 0 && Self::hook_on(th, HOOK_RETURN) {
                        self.fire_hook(th, fuel, "return", -1)?;
                    }
                }
                Pending::Reraise { err } => {
                    return Err(VmError {
                        val: ErrVal::Val(err),
                        line: 0,
                        root_line: 0,
                        source: None,
                    });
                }
                Pending::YieldStep => {
                    let mut job = th.yield_job.take().expect("yield job staged");
                    match job.stage {
                        0 => {
                            job.stage = 1;
                            th.yield_job = Some(job);
                            // re-arm below the hook frame so the next stage (or
                            // the yield itself) runs once the hook returns
                            th.frames
                                .last_mut()
                                .unwrap()
                                .pending_mut()
                                .push(Pending::YieldStep);
                            if self.hook_suppress == 0 && Self::hook_on(th, HOOK_CALL) {
                                self.fire_hook(th, fuel, "call", -1)?;
                            }
                        }
                        1 => {
                            job.stage = 2;
                            th.yield_job = Some(job);
                            th.frames
                                .last_mut()
                                .unwrap()
                                .pending_mut()
                                .push(Pending::YieldStep);
                            if self.hook_suppress == 0 && Self::hook_on(th, HOOK_RETURN) {
                                self.fire_hook(th, fuel, "return", -1)?;
                            }
                        }
                        _ => self.perform_yield(th, &job),
                    }
                }
                Pending::UnwindAfterHandler { result_slot } => {
                    if self.handler_unwind.take().is_some() {
                        let v = th.stack[result_slot];
                        let e = VmError {
                            val: ErrVal::Val(v),
                            line: 0,
                            root_line: 0,
                            source: None,
                        };
                        self.unwind(th, e, true)?;
                    }
                }
            }
            return Ok(Flow::Continue);
        }
        // Debug hooks that fire before the next instruction: the call hook an
        // asynchronously-started coroutine body still owes, then count and
        // line events. Each runs the hook as a frame, so this returns; the
        // instruction is only fetched on the following dispatch.
        if th.pending_call_hook {
            th.pending_call_hook = false;
            if Self::hook_on(th, HOOK_CALL) {
                self.fire_hook(th, fuel, "call", -1)?;
                return Ok(Flow::Continue);
            }
        }
        if th.hook_count > 0 && Self::hook_any(th) {
            th.hook_counter -= 1;
            if th.hook_counter <= 0 {
                th.hook_counter = th.hook_count;
                let line = line_of(th) as i64;
                self.fire_hook(th, fuel, "count", line)?;
                return Ok(Flow::Continue);
            }
        }
        if Self::hook_on(th, HOOK_LINE) {
            let line = line_of(th) as i64;
            // Line 0 means "no line info" (e.g. a synthetic prologue
            // instruction); PUC only reports real source lines.
            if line > 0
                && let Frame::Lua(lf) = th.frames.last_mut().unwrap()
                && lf.last_line != line
            {
                lf.last_line = line;
                self.fire_hook(th, fuel, "line", line)?;
                return Ok(Flow::Continue);
            }
        }
        let (instr, base) = {
            let f = th.frames.last_mut().unwrap().as_lua_mut();
            if f.pc >= f.proto.code.len() {
                return Err(Self::rt_err(
                    th,
                    "control fell off the end of a function".into(),
                ));
            }
            let i = f.proto.code[f.pc];
            f.pc += 1;
            (i, f.base)
        };
        match instr {
            Instr::LoadK { dst, k } => {
                th.stack[base + dst as usize] = kval(th, k);
            }
            Instr::LoadNil { dst, n } => {
                for i in 0..n as usize {
                    th.stack[base + dst as usize + i] = Value::Nil;
                }
            }
            Instr::LoadBool { dst, b } => {
                th.stack[base + dst as usize] = Value::Bool(b);
            }
            Instr::Move { dst, src } => {
                th.stack[base + dst as usize] = th.stack[base + src as usize];
            }
            Instr::GetUpval { dst, up } => {
                let id = self.frame_upval(th, up);
                th.stack[base + dst as usize] = self.read_upval(tid, th, id);
            }
            Instr::SetUpval { up, src } => {
                let id = self.frame_upval(th, up);
                let v = th.stack[base + src as usize];
                self.write_upval(tid, th, id, v);
            }
            Instr::GetIndex { dst, obj, key } => {
                let o = th.stack[base + obj as usize];
                let k = th.stack[base + key as usize];
                if let Some(v) = self.index_chain(th, fuel, o, k, base + dst as usize)? {
                    th.stack[base + dst as usize] = v;
                }
            }
            Instr::GetField { dst, obj, k } => {
                let o = th.stack[base + obj as usize];
                let key = kval(th, k);
                if let Some(v) = self.index_chain(th, fuel, o, key, base + dst as usize)? {
                    th.stack[base + dst as usize] = v;
                }
            }
            Instr::SetIndex { obj, key, src } => {
                let o = th.stack[base + obj as usize];
                let k = th.stack[base + key as usize];
                let v = th.stack[base + src as usize];
                self.newindex_chain(th, fuel, o, k, v)?;
            }
            Instr::SetField { obj, k, src } => {
                let o = th.stack[base + obj as usize];
                let key = kval(th, k);
                let v = th.stack[base + src as usize];
                self.newindex_chain(th, fuel, o, key, v)?;
            }
            Instr::NewTable { dst } => {
                *fuel -= 2;
                th.stack[base + dst as usize] = self.new_table();
                self.maybe_gc(tid, th)?;
            }
            Instr::SetList {
                obj,
                base: b,
                n,
                start,
            } => return self.exec_set_list(th, fuel, base, obj, b, n, start),
            Instr::Arith { op, dst, lhs, rhs } => {
                return self.exec_arith(th, fuel, base, op, dst, lhs, rhs);
            }
            Instr::Unary { op, dst, src } => {
                let v = th.stack[base + src as usize];
                self.unary(th, fuel, op, v, base + dst as usize)?;
            }
            Instr::Cmp { op, dst, lhs, rhs } => {
                let a = th.stack[base + lhs as usize];
                let b = th.stack[base + rhs as usize];
                self.compare(th, fuel, op, a, b, base + dst as usize)?;
            }
            Instr::Concat { dst, base: b, n } => {
                self.concat_run(th, fuel, dst, b, n)?;
                self.maybe_gc(tid, th)?;
            }
            Instr::Jump { off } => {
                jump(th, off);
            }
            Instr::Test { src, if_true, off } => {
                if th.stack[base + src as usize].truthy() == if_true {
                    jump(th, off);
                }
            }
            Instr::Call {
                base: b,
                nargs,
                nres,
            } => {
                return self.exec_call(th, fuel, base, b, nargs, nres);
            }
            Instr::TailCall { base: b, nargs } => {
                return self.exec_tail_call(th, fuel, b, nargs);
            }
            Instr::Return { base: b, n } => return self.exec_return(th, fuel, b, n),
            Instr::Vararg { dst, n } => return self.exec_vararg(th, base, dst, n),
            Instr::Closure { dst, p } => {
                return self.exec_closure(th, fuel, tid, base, dst, p);
            }
            Instr::Close { from } => {
                self.close_upvals(th, base + from as usize);
                self.run_close_tbc(th, fuel, from, Value::Nil)?;
            }
            Instr::Tbc { reg, name } => return self.exec_tbc(th, base, reg, name),
            Instr::ForPrep { base: b, off } => {
                self.for_prep(th, base + b as usize, off)?;
            }
            Instr::ForLoop { base: b, off } => return self.exec_for_loop(th, base, b, off),
            Instr::TForLoop { base: b, off } => {
                let a = base + b as usize;
                let v = th.stack[a + 4];
                if v != Value::Nil {
                    th.stack[a + 2] = v;
                    jump(th, off);
                }
            }
        }
        Ok(Flow::Continue)
    }

    #[expect(clippy::too_many_arguments)]
    fn exec_set_list(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        base: usize,
        obj: u8,
        b: u8,
        n: u8,
        start: u32,
    ) -> Result<Flow, VmError> {
        let obj_val = th.stack[base + obj as usize];
        let Value::Table(t) = obj_val else {
            // Only reachable from a crafted binary chunk (`SetList` is always
            // emitted on a freshly built table).
            return Err(Self::rt_err(
                th,
                format!("attempt to index a {} value", obj_val.type_name()),
            ));
        };
        let first = base + b as usize;
        let count = if n == 0 {
            th.top.saturating_sub(first)
        } else {
            n as usize
        };
        *fuel -= count as i64;
        for i in 0..count {
            let v = th.stack[first + i];
            self.tables[t.0 as usize]
                .set(Value::Int(start as i64 + i as i64), v)
                .map_err(|m| Self::rt_err(th, m.to_string()))?;
        }
        Ok(Flow::Continue)
    }

    #[expect(clippy::too_many_arguments)]
    fn exec_arith(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        base: usize,
        op: ArithOp,
        dst: u8,
        lhs: u8,
        rhs: u8,
    ) -> Result<Flow, VmError> {
        let a = th.stack[base + lhs as usize];
        let b = th.stack[base + rhs as usize];
        match arith(&self.strings, op, a, b) {
            Ok(v) => th.stack[base + dst as usize] = v,
            Err(msg) => {
                let mm = self.binary_mm(a, b, mm_of_arith(op));
                if mm == Value::Nil {
                    let msg = if msg == "number has no integer representation"
                        && matches!(
                            op,
                            ArithOp::BAnd
                                | ArithOp::BOr
                                | ArithOp::BXor
                                | ArithOp::Shl
                                | ArithOp::Shr
                        ) {
                        // PUC-style varinfo: name the offending operand
                        // when it was loaded from a constant field.
                        let reg = if matches!(a, Value::Float(_)) {
                            Some(lhs)
                        } else if matches!(b, Value::Float(_)) {
                            Some(rhs)
                        } else {
                            None
                        };
                        let f = th.frames.last().unwrap().as_lua();
                        match reg.and_then(|r| {
                            name_for_register(&self.strings, &f.proto, f.pc.wrapping_sub(1), r)
                        }) {
                            Some(v) => format!("number ({v}) has no integer representation"),
                            None => msg,
                        }
                    } else {
                        msg
                    };
                    return Err(Self::rt_err(th, msg));
                }
                self.call_value(
                    th,
                    mm,
                    &[a, b],
                    base + dst as usize,
                    2,
                    RetShape::Normal,
                    fuel,
                )?;
            }
        }
        Ok(Flow::Continue)
    }

    fn exec_call(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        base: usize,
        b: u8,
        nargs: u8,
        nres: u8,
    ) -> Result<Flow, VmError> {
        let func_abs = base + b as usize;
        let argc = if nargs == 0 {
            th.top.saturating_sub(func_abs + 1)
        } else {
            (nargs - 1) as usize
        };
        self.do_call(
            th,
            fuel,
            &CallSpec {
                func_abs,
                argc,
                ret_to: func_abs,
                nres,
                shape: RetShape::Normal,
                protected: false,
                handler: None,
                native_caller: false,
            },
        )?;
        Ok(Flow::Continue)
    }

    fn exec_tail_call(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        b: u8,
        nargs: u8,
    ) -> Result<Flow, VmError> {
        let fb = th.frames.last().unwrap().as_lua().base;
        let func_abs = fb + b as usize;
        let argc = if nargs == 0 {
            th.top.saturating_sub(func_abs + 1)
        } else {
            (nargs - 1) as usize
        };
        let (callee, func_abs, argc) = self.resolve_callable(th, func_abs, argc)?;
        match callee {
            Value::Closure(cid) => {
                self.tail_replace_frame(th, cid, func_abs, argc)?;
            }
            Value::Native(nid) => {
                // Natives do not own a Lua frame, so there is nothing
                // to reuse: run the call with an open result window,
                // then finish the frame's return via a continuation
                // (the callee may itself push frames, e.g. pcall).
                let ret_slot = fb + b as usize;
                th.frames
                    .last_mut()
                    .unwrap()
                    .pending_mut()
                    .push(Pending::TailReturn { start: ret_slot });
                self.call_native(
                    th,
                    fuel,
                    nid,
                    func_abs,
                    argc,
                    ret_slot,
                    0,
                    RetShape::Normal,
                    false,
                )?;
            }
            _ => unreachable!("resolve_callable returns a callable"),
        }
        Ok(Flow::Continue)
    }

    fn exec_return(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        b: u8,
        n: u8,
    ) -> Result<Flow, VmError> {
        if !th.frames.last().unwrap().as_lua().tbc.is_empty() {
            // run __close handlers before completing the return;
            // snapshot the value window now (closes may clobber top)
            let fb = th.frames.last().unwrap().as_lua().base;
            let start = fb + b as usize;
            let count = if n == 0 {
                th.top.saturating_sub(start)
            } else {
                (n - 1) as usize
            };
            let f = th.frames.last_mut().unwrap().as_lua_mut();
            f.pending.push(Pending::FinishReturn { start, count });
            // Fire the return hook after every __close handler has run
            // (PUC's order): the hook may even be *set* by one of them.
            f.pending.push(Pending::ReturnHookFire);
            f.pending.push(Pending::CloseTbc {
                from: 0,
                err: Value::Nil,
            });
            return Ok(Flow::Continue);
        }
        let frame_base = th.frames.last().unwrap().as_lua().base;
        let start = frame_base + b as usize;
        let count = if n == 0 {
            th.top.saturating_sub(start)
        } else {
            (n - 1) as usize
        };
        if Self::hook_on(th, HOOK_RETURN) {
            let f = th.frames.last_mut().unwrap().as_lua_mut();
            f.pending.push(Pending::FinishReturn { start, count });
            self.fire_hook(th, fuel, "return", -1)?;
            return Ok(Flow::Continue);
        }
        let frame = th.frames.pop().unwrap();
        let frame = frame.as_lua();
        self.close_upvals(th, frame.base);
        if th.frames.is_empty() {
            let vals = th.stack[start..start + count].to_vec();
            th.stack.clear();
            th.top = 0;
            return Ok(Flow::Finished(vals));
        }
        deliver_return(th, frame, start, count);
        Ok(Flow::Continue)
    }

    #[expect(clippy::unused_self)]
    fn exec_vararg(
        &mut self,
        th: &mut Thread,
        base: usize,
        dst: u8,
        n: u8,
    ) -> Result<Flow, VmError> {
        let varargs = std::mem::take(&mut th.frames.last_mut().unwrap().as_lua_mut().varargs);
        let first = base + dst as usize;
        if n == 0 {
            ensure_len(&mut th.stack, first + varargs.len());
            th.stack[first..first + varargs.len()].copy_from_slice(&varargs);
            th.top = first + varargs.len();
        } else {
            let want = (n - 1) as usize;
            ensure_len(&mut th.stack, first + want);
            for i in 0..want {
                th.stack[first + i] = varargs.get(i).copied().unwrap_or(Value::Nil);
            }
        }
        th.frames.last_mut().unwrap().as_lua_mut().varargs = varargs;
        Ok(Flow::Continue)
    }

    fn exec_closure(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        tid: ThreadId,
        base: usize,
        dst: u8,
        p: u16,
    ) -> Result<Flow, VmError> {
        *fuel -= 2;
        let proto = th.frames.last().unwrap().as_lua().proto.protos[p as usize].clone();
        let mut ups = Vec::with_capacity(proto.upvals.len());
        for d in &proto.upvals {
            match *d {
                UpvalDesc::Local(r) => {
                    let abs = base + r as usize;
                    ups.push(self.find_or_create_open(tid, th, abs));
                }
                UpvalDesc::Upval(i) => {
                    let cur = th.frames.last().unwrap().as_lua().closure;
                    ups.push(self.closures[cur.0 as usize].upvals[i as usize]);
                }
            }
        }
        let cid = self.alloc_closure(LuaClosure { proto, upvals: ups });
        th.stack[base + dst as usize] = Value::Closure(cid);
        self.maybe_gc(tid, th)?;
        Ok(Flow::Continue)
    }

    fn exec_tbc(
        &mut self,
        th: &mut Thread,
        base: usize,
        reg: u8,
        name: u16,
    ) -> Result<Flow, VmError> {
        let v = th.stack[base + reg as usize];
        match v {
            Value::Nil | Value::Bool(false) => {}
            _ if self.metamethod(v, Mm::Close) != Value::Nil => {
                th.frames.last_mut().unwrap().as_lua_mut().tbc.push(reg);
            }
            _ => {
                let vname = match kval(th, name) {
                    Value::Str(id) => self.strings.get_str_lossy(id).into_owned(),
                    _ => "?".to_string(),
                };
                return Err(Self::rt_err(
                    th,
                    format!("variable '{vname}' got a non-closable value"),
                ));
            }
        }
        Ok(Flow::Continue)
    }

    #[expect(clippy::unused_self)]
    fn exec_for_loop(
        &mut self,
        th: &mut Thread,
        base: usize,
        b: u8,
        off: i32,
    ) -> Result<Flow, VmError> {
        let a = base + b as usize;
        match (th.stack[a], th.stack[a + 1], th.stack[a + 2]) {
            (Value::Int(i), Value::Int(l), Value::Int(s)) => {
                if let Some(ni) = i.checked_add(s)
                    && ((s > 0 && ni <= l) || (s < 0 && ni >= l))
                {
                    th.stack[a] = Value::Int(ni);
                    th.stack[a + 3] = Value::Int(ni);
                    jump(th, off);
                }
            }
            (Value::Float(i), Value::Float(l), Value::Float(s)) => {
                let ni = i + s;
                if (s > 0.0 && ni <= l) || (s < 0.0 && ni >= l) {
                    th.stack[a] = Value::Float(ni);
                    th.stack[a + 3] = Value::Float(ni);
                    jump(th, off);
                }
            }
            _ => {
                // Only reachable from a crafted binary chunk: the compiler
                // always emits `ForPrep` before `ForLoop`, which normalizes
                // the control/limit/step registers to all-int or all-float.
                return Err(Self::rt_err(
                    th,
                    "'for' loop control/limit/step must be a number".into(),
                ));
            }
        }
        Ok(Flow::Continue)
    }

    /// Calls `f` with `args` copied to a scratch window above the current
    /// frame. Results are delivered to `ret_to` (immediately for natives,
    /// after the pushed frame returns for Lua closures).
    #[expect(clippy::too_many_arguments)]
    fn call_value(
        &mut self,
        th: &mut Thread,
        f: Value,
        args: &[Value],
        ret_to: usize,
        nres: u8,
        shape: RetShape,
        fuel: &mut i64,
    ) -> Result<(), VmError> {
        let wb = scratch_base(th);
        ensure_len(&mut th.stack, wb + 1 + args.len());
        th.stack[wb] = f;
        th.stack[wb + 1..wb + 1 + args.len()].copy_from_slice(args);
        self.do_call(
            th,
            fuel,
            &CallSpec {
                func_abs: wb,
                argc: args.len(),
                ret_to,
                nres,
                shape,
                protected: false,
                handler: None,
                native_caller: false,
            },
        )
    }

    fn do_call(&mut self, th: &mut Thread, fuel: &mut i64, spec: &CallSpec) -> Result<(), VmError> {
        *fuel -= 2;
        let CallSpec {
            mut func_abs,
            mut argc,
            ret_to,
            nres,
            shape,
            protected,
            handler,
            native_caller,
        } = *spec;
        for _ in 0..MAX_META_CHAIN {
            match th.stack[func_abs] {
                Value::Closure(cid) => {
                    if th.frames.len() >= MAX_CALL_DEPTH {
                        return Err(Self::rt_err(
                            th,
                            "stack overflow (too many nested calls)".into(),
                        ));
                    }
                    let proto = self.closures[cid.0 as usize].proto.clone();
                    let new_base = func_abs + 1;
                    let np = proto.nparams as usize;
                    let mut varargs = Vec::new();
                    if proto.is_vararg && argc > np {
                        varargs.extend_from_slice(&th.stack[new_base + np..new_base + argc]);
                    }
                    ensure_len(&mut th.stack, new_base + proto.max_regs as usize);
                    for i in argc..np {
                        th.stack[new_base + i] = Value::Nil;
                    }
                    th.frames.push(Frame::Lua(LuaFrame {
                        closure: cid,
                        proto,
                        pc: 0,
                        base: new_base,
                        ret_to,
                        nres,
                        shape,
                        protected,
                        handler,
                        handler_guard: false,
                        pending: Vec::new(),
                        tbc: Vec::new(),
                        varargs,
                        tailcall: false,
                        call_meta: None,
                        last_line: -1,
                        is_hook: false,
                    }));
                    if self.hook_suppress == 0 && Self::hook_on(th, HOOK_CALL) {
                        self.fire_hook(th, fuel, "call", -1)?;
                    }
                    return Ok(());
                }
                Value::Native(nid) => {
                    return self.call_native(
                        th,
                        fuel,
                        nid,
                        func_abs,
                        argc,
                        ret_to,
                        nres,
                        shape,
                        native_caller,
                    );
                }
                other => {
                    // __call: f(args...) becomes mm(f, args...)
                    let mm = self.metamethod(other, Mm::Call);
                    if mm == Value::Nil {
                        return Err(Self::rt_err(
                            th,
                            format!("attempt to call a {} value", other.type_name()),
                        ));
                    }
                    // Prepend `mm`, keeping the window pinned at the frame's
                    // scratch base. Repeated chases then shift in place (the
                    // window stays put) instead of growing the stack by one
                    // slot per level.
                    let sb = scratch_base(th);
                    let wb = func_abs.max(sb);
                    ensure_len(&mut th.stack, wb + 2 + argc);
                    th.stack.copy_within(func_abs..func_abs + 1 + argc, wb + 1);
                    th.stack[wb] = mm;
                    func_abs = wb;
                    argc += 1;
                }
            }
        }
        Err(Self::rt_err(th, "'__call' chain too long".into()))
    }

    /// Resolves the callee at `func_abs`, chasing `__call` metamethod
    /// chains, without starting any call. Returns the final callable and the
    /// (possibly relocated) argument window.
    fn resolve_callable(
        &mut self,
        th: &mut Thread,
        mut func_abs: usize,
        mut argc: usize,
    ) -> Result<(Value, usize, usize), VmError> {
        for _ in 0..MAX_META_CHAIN {
            match th.stack[func_abs] {
                v @ (Value::Closure(_) | Value::Native(_)) => return Ok((v, func_abs, argc)),
                other => {
                    // __call: f(args...) becomes mm(f, args...)
                    let mm = self.metamethod(other, Mm::Call);
                    if mm == Value::Nil {
                        return Err(Self::rt_err(
                            th,
                            format!("attempt to call a {} value", other.type_name()),
                        ));
                    }
                    // Prepend `mm`, keeping the window pinned at the frame's
                    // scratch base. Repeated chases then shift in place (the
                    // window stays put) instead of growing the stack by one
                    // slot per level.
                    let sb = scratch_base(th);
                    let wb = func_abs.max(sb);
                    ensure_len(&mut th.stack, wb + 2 + argc);
                    th.stack.copy_within(func_abs..func_abs + 1 + argc, wb + 1);
                    th.stack[wb] = mm;
                    func_abs = wb;
                    argc += 1;
                }
            }
        }
        Err(Self::rt_err(th, "'__call' chain too long".into()))
    }

    /// Replaces the current frame with a call to `cid`, moving the callee and
    /// its arguments down over the old frame's base so the stack does not
    /// grow across tail calls. The new frame inherits the replaced frame's
    /// return destination, expected count, shape, and protection boundary.
    fn tail_replace_frame(
        &mut self,
        th: &mut Thread,
        cid: ClosId,
        func_abs: usize,
        argc: usize,
    ) -> Result<(), VmError> {
        let old = th.frames.last().unwrap().as_lua();
        let fb = old.base;
        let ret_to = old.ret_to;
        let nres = old.nres;
        let shape = old.shape;
        let protected = old.protected;
        let handler = old.handler;
        // upvalues captured by the discarded frame must be closed before its
        // registers are overwritten
        self.close_upvals(th, fb);
        let proto = self.closures[cid.0 as usize].proto.clone();
        let np = proto.nparams as usize;
        // Move `[func, args...]` down to occupy the old frame's function slot
        // (if there is one), so tail recursion reuses the same stack region.
        let new_func_abs = match fb.checked_sub(1) {
            Some(dst) => {
                th.stack.copy_within(func_abs..func_abs + 1 + argc, dst);
                dst
            }
            None => func_abs,
        };
        let new_base = new_func_abs + 1;
        let mut varargs = Vec::new();
        if proto.is_vararg && argc > np {
            varargs.extend_from_slice(&th.stack[new_base + np..new_base + argc]);
        }
        ensure_len(&mut th.stack, new_base + proto.max_regs as usize);
        for i in argc..np {
            th.stack[new_base + i] = Value::Nil;
        }
        th.frames.pop();
        th.frames.push(Frame::Lua(LuaFrame {
            closure: cid,
            proto,
            pc: 0,
            base: new_base,
            ret_to,
            nres,
            shape,
            protected,
            handler,
            handler_guard: false,
            pending: Vec::new(),
            tbc: Vec::new(),
            varargs,
            tailcall: true,
            call_meta: None,
            last_line: -1,
            is_hook: false,
        }));
        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    fn call_native(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        nid: NativeId,
        func_abs: usize,
        argc: usize,
        ret_to: usize,
        nres: u8,
        shape: RetShape,
        native_caller: bool,
    ) -> Result<(), VmError> {
        match self.natives[nid.0 as usize].kind {
            NativeKind::Plain(f) => {
                let args = th.stack[func_abs + 1..func_abs + 1 + argc].to_vec();
                let res = f(self, &args).map_err(|message| VmError {
                    val: ErrVal::Msg(message),
                    line: line_of(th),
                    root_line: 0,
                    source: None,
                })?;
                place_shaped(th, ret_to, nres, shape, &res);
                if self.hook_suppress == 0 && Self::hook_on(th, HOOK_RETURN) {
                    self.fire_hook(th, fuel, "return", -1)?;
                }
                Ok(())
            }
            NativeKind::Suspendable(f) => {
                let fv = th.stack[func_abs];
                let args = th.stack[func_abs + 1..func_abs + 1 + argc].to_vec();
                let execution = self
                    .current_execution
                    .expect("native called outside execution");
                let mut context = self
                    .active_context
                    .take()
                    .expect("execution context missing");
                let outcome = f(
                    &mut NativeContext {
                        lua: self,
                        execution,
                        context: &mut context,
                    },
                    &args,
                );
                self.active_context = Some(context);
                match outcome.map_err(|message| VmError {
                    val: ErrVal::Msg(message),
                    line: line_of(th),
                    root_line: 0,
                    source: None,
                })? {
                    NativeOutcome::Return(res) => {
                        place_shaped(th, ret_to, nres, shape, &res);
                        if self.hook_suppress == 0 && Self::hook_on(th, HOOK_RETURN) {
                            self.fire_hook(th, fuel, "return", -1)?;
                        }
                    }
                    NativeOutcome::Wait(wait) => {
                        th.pending_native = Some(PendingNative {
                            wait,
                            exec: self
                                .current_execution
                                .expect("native called outside an execution"),
                            func: fv,
                            ret_to,
                            nres,
                            shape,
                            completion: None,
                        });
                        if self.wait_can_park(th) {
                            // Suspend only this coroutine; the resumer carries on.
                            self.switch_to = Some(self.park_thread(th));
                        } else {
                            // Root thread (or an unparkable coroutine): block the
                            // whole execution until the host completes the wait.
                            self.suspended_wait = Some(wait);
                        }
                    }
                }
                Ok(())
            }
            NativeKind::Intrinsic(i) => self.call_intrinsic(
                th,
                fuel,
                i,
                IntrinsicCall {
                    func_abs,
                    argc,
                    ret_to,
                    nres,
                    shape,
                    native_caller,
                },
            ),
        }
    }

    fn call_intrinsic(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        i: Intrinsic,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        match i {
            Intrinsic::Error => self.intrinsic_error(th, call),
            Intrinsic::Assert => self.intrinsic_assert(th, call),
            Intrinsic::ToString => self.intrinsic_to_string(th, fuel, call),
            Intrinsic::Format => self.intrinsic_format(th, call),
            Intrinsic::Pcall => self.intrinsic_pcall(th, fuel, call),
            Intrinsic::Xpcall => self.intrinsic_xpcall(th, fuel, call),
            Intrinsic::Resume => self.intrinsic_resume(th, fuel, call),
            Intrinsic::WrapResume(co) => self.intrinsic_wrap_resume(th, fuel, co, call),
            Intrinsic::Yield => self.intrinsic_yield(th, fuel, call),
            Intrinsic::EnterNonYieldable => {
                th.non_yieldable = th.non_yieldable.saturating_add(1);
                place_shaped(th, call.ret_to, call.nres, call.shape, &[]);
                Ok(())
            }
            Intrinsic::LeaveNonYieldable => {
                th.non_yieldable = th.non_yieldable.saturating_sub(1);
                place_shaped(th, call.ret_to, call.nres, call.shape, &[]);
                Ok(())
            }
            Intrinsic::IsYieldable => self.intrinsic_is_yieldable(th, call),
            Intrinsic::CollectGarbage => self.intrinsic_collect_garbage(th, call),
            Intrinsic::Print => self.intrinsic_print(th, call),
            Intrinsic::CoroutineClose => self.intrinsic_coroutine_close(th, fuel, call),
            Intrinsic::Running => {
                let cur = Value::Thread(self.current_thread);
                let is_main = Value::Bool(th.parent.is_none());
                place_shaped(th, call.ret_to, call.nres, call.shape, &[cur, is_main]);
                Ok(())
            }
            Intrinsic::DebugGetinfo => self.intrinsic_debug_getinfo(th, call),
            Intrinsic::DebugTraceback => self.intrinsic_debug_traceback(th, call),
            Intrinsic::DebugGetupvalue => self.intrinsic_debug_getupvalue(th, call),
            Intrinsic::DebugSetupvalue => self.intrinsic_debug_setupvalue(th, call),
            Intrinsic::DebugUpvalueid => self.intrinsic_debug_upvalueid(th, call),
            Intrinsic::DebugUpvaluejoin => self.intrinsic_debug_upvaluejoin(th, call),
            Intrinsic::DebugGetmetatable => self.intrinsic_debug_getmetatable(th, call),
            Intrinsic::DebugSetmetatable => self.intrinsic_debug_setmetatable(th, call),
            Intrinsic::DebugGetregistry => {
                place_shaped(
                    th,
                    call.ret_to,
                    call.nres,
                    call.shape,
                    &[Value::Table(self.globals)],
                );
                Ok(())
            }
            Intrinsic::DebugGethook => self.intrinsic_debug_gethook(th, call),
            Intrinsic::DebugSethook => self.intrinsic_debug_sethook(th, fuel, call),
        }
    }

    fn intrinsic_error(&mut self, th: &mut Thread, call: IntrinsicCall) -> Result<(), VmError> {
        let v = call.arg(th, 0);
        let level = match call.arg(th, 1) {
            Value::Int(l) => l,
            Value::Float(f) => f as i64,
            _ => 1,
        };
        // level 1 names the caller of error(); when that caller is a
        // native (pcall(error, ...)), there is no Lua position
        let val = match v {
            Value::Str(s) if level > 0 && !call.native_caller => {
                let fidx = th.frames.len().saturating_sub(level as usize);
                let (line, src) = match th.frames.get(fidx).and_then(|f| f.lua()) {
                    Some(f) => (
                        f.proto
                            .lines
                            .get(f.pc.wrapping_sub(1))
                            .copied()
                            .unwrap_or(0),
                        f.proto.source.clone(),
                    ),
                    None => (
                        line_of(th),
                        th.frames
                            .last()
                            .and_then(|f| f.lua())
                            .map(|f| f.proto.source.clone())
                            .unwrap_or_default(),
                    ),
                };
                let msg = format!("{src}:{line}: {}", self.strings.get_str_lossy(s));
                self.new_string(msg.as_bytes())
            }
            _ => v,
        };
        Err(VmError {
            val: ErrVal::Val(val),
            line: line_of(th),
            root_line: 0,
            source: None,
        })
    }

    #[expect(clippy::unused_self)]
    fn intrinsic_assert(&mut self, th: &mut Thread, call: IntrinsicCall) -> Result<(), VmError> {
        if call.argc == 0 {
            return Err(Self::rt_err(
                th,
                "bad argument #1 to 'assert' (value expected)".into(),
            ));
        }
        if call.arg(th, 0).truthy() {
            let res = th.stack[call.func_abs + 1..call.func_abs + 1 + call.argc].to_vec();
            place_shaped(th, call.ret_to, call.nres, call.shape, &res);
            Ok(())
        } else {
            match call.arg(th, 1) {
                Value::Nil => {
                    let source = if call.native_caller {
                        None
                    } else {
                        th.frames
                            .last()
                            .and_then(|f| f.lua())
                            .map(|f| f.proto.source.clone())
                    };
                    Err(VmError {
                        val: ErrVal::Msg("assertion failed!".into()),
                        line: line_of(th),
                        root_line: 0,
                        source,
                    })
                }
                v => Err(VmError {
                    val: ErrVal::Val(v),
                    line: line_of(th),
                    root_line: 0,
                    source: None,
                }),
            }
        }
    }

    fn intrinsic_to_string(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let v = call.arg(th, 0);
        let mm = self.metamethod(v, Mm::ToString);
        if mm == Value::Nil {
            let s = self.tostring_default(v);
            let sv = self.new_string(s.as_bytes());
            place_shaped(th, call.ret_to, call.nres, call.shape, &[sv]);
            Ok(())
        } else {
            // PUC's `luaL_tolstring` requires the metamethod result to
            // be a string (numbers are accepted by `lua_isstring` and
            // converted). Validate after it returns.
            let result_slot = scratch_base(th);
            ensure_len(&mut th.stack, result_slot + 1);
            th.frames
                .last_mut()
                .unwrap()
                .pending_mut()
                .push(Pending::FinishTostring {
                    ret_to: call.ret_to,
                    nres: call.nres,
                    shape: call.shape,
                    result_slot,
                });
            self.call_value(th, mm, &[v], result_slot, 2, RetShape::Normal, fuel)
        }
    }

    fn intrinsic_format(&mut self, th: &mut Thread, call: IntrinsicCall) -> Result<(), VmError> {
        let nargs = call.argc;
        let fmt = if nargs == 0 {
            return Err(Self::rt_err(
                th,
                "bad argument #1 to 'format' (string expected, got no value)".into(),
            ));
        } else {
            match call.arg(th, 0) {
                Value::Str(id) => self.strings.get(id).to_vec(),
                v @ (Value::Int(_) | Value::Float(_)) => crate::value::fmt_number(v).into_bytes(),
                other => {
                    return Err(Self::rt_err(
                        th,
                        format!(
                            "bad argument #1 to 'format' (string expected, got {})",
                            other.type_name()
                        ),
                    ));
                }
            }
        };
        let args: Vec<Value> = (0..nargs).map(|i| call.arg(th, i)).collect();
        self.format_job = Some(FormatJob {
            fmt,
            pos: 0,
            arg: 1,
            args,
            out: Vec::new(),
            ret_to: call.ret_to,
            nres: call.nres,
            shape: call.shape,
            result_slot: 0,
            awaiting: false,
            saved: None,
        });
        th.frames
            .last_mut()
            .unwrap()
            .pending_mut()
            .push(Pending::FormatStep);
        Ok(())
    }

    fn intrinsic_pcall(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        if call.argc == 0 {
            return Err(Self::rt_err(
                th,
                "bad argument #1 to 'pcall' (value expected)".into(),
            ));
        }
        if call.shape == RetShape::PrependTrue
            && let Some(f @ Frame::Boundary(_)) = th.frames.last_mut()
        {
            f.pending_mut().push(Pending::PrependShape {
                ret_to: call.ret_to,
                nres: call.nres,
                shape: call.shape,
            });
        }
        self.protected_call(
            th,
            fuel,
            call.func_abs + 1,
            call.argc - 1,
            call.ret_to,
            call.nres,
            None,
            true,
        )
    }

    fn intrinsic_xpcall(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        if call.argc < 2 {
            return Err(Self::rt_err(
                th,
                "bad argument #2 to 'xpcall' (value expected)".into(),
            ));
        }
        let handler = call.arg(th, 1);
        // rebuild a contiguous window: [f, args...] (handler sits
        // between f and the args in the original window)
        let f = call.arg(th, 0);
        let wb = scratch_base(th).max(call.func_abs + 1 + call.argc);
        let n_args = call.argc - 2;
        ensure_len(&mut th.stack, wb + 1 + n_args);
        th.stack[wb] = f;
        th.stack
            .copy_within(call.func_abs + 3..call.func_abs + 1 + call.argc, wb + 1);
        if call.shape == RetShape::PrependTrue
            && let Some(cf @ Frame::Boundary(_)) = th.frames.last_mut()
        {
            cf.pending_mut().push(Pending::PrependShape {
                ret_to: call.ret_to,
                nres: call.nres,
                shape: call.shape,
            });
        }
        self.protected_call(
            th,
            fuel,
            wb,
            n_args,
            call.ret_to,
            call.nres,
            Some(handler),
            true,
        )
    }

    fn intrinsic_resume(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let co = call.arg(th, 0);
        let Value::Thread(co) = co else {
            return Err(Self::rt_err(
                th,
                format!(
                    "bad argument #1 to 'resume' (coroutine expected, got {})",
                    co.type_name()
                ),
            ));
        };
        let args: Vec<Value> = th.stack[call.func_abs + 2..call.func_abs + 1 + call.argc].to_vec();
        self.resume_thread(
            th,
            fuel,
            co,
            &args,
            call.ret_to,
            call.nres,
            call.shape,
            false,
        )
    }

    fn intrinsic_wrap_resume(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        co: ThreadId,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let args: Vec<Value> = th.stack[call.func_abs + 1..call.func_abs + 1 + call.argc].to_vec();
        self.resume_thread(
            th,
            fuel,
            co,
            &args,
            call.ret_to,
            call.nres,
            call.shape,
            true,
        )
    }

    fn intrinsic_yield(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        // Non-yieldable C boundary (e.g. inside a `table.sort`
        // comparator or `string.gsub` replacement): PUC reports a
        // cross-boundary yield, unless we are on the main thread, which
        // reports "outside a coroutine" instead.
        if th.non_yieldable > 0 && !th.is_main {
            return Err(Self::rt_err(
                th,
                "attempt to yield across a C-call boundary".into(),
            ));
        }
        let Some(parent) = th.parent else {
            return Err(Self::rt_err(
                th,
                "attempt to yield from outside a coroutine".into(),
            ));
        };
        *fuel -= 3;
        let args: Vec<Value> = th.stack[call.func_abs + 1..call.func_abs + 1 + call.argc].to_vec();
        let rr = th.resume_ret.expect("resumed thread has resume_ret");
        let yield_fn = th.stack[call.func_abs];
        let want_call = self.hook_suppress == 0 && Self::hook_on(th, HOOK_CALL);
        let want_ret = self.hook_suppress == 0 && Self::hook_on(th, HOOK_RETURN);
        let job = YieldJob {
            parent,
            rr,
            ret_to: call.ret_to,
            nres: call.nres,
            shape: call.shape,
            args,
            func: yield_fn,
            stage: 0,
        };
        if want_call || want_ret {
            // `yield` is a C function: emit its call/return hook events
            // before the thread actually suspends. Each hook runs as a
            // frame, so the suspension is deferred through `YieldStep`.
            th.yield_job = Some(job);
            th.frames
                .last_mut()
                .unwrap()
                .pending_mut()
                .push(Pending::YieldStep);
            return Ok(());
        }
        self.perform_yield(th, &job);
        Ok(())
    }

    fn intrinsic_is_yieldable(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        // Optional `co` argument: true for any coroutine (even a dead
        // or never-started one), false for the main thread or a thread
        // currently inside a non-yieldable C boundary.
        let r = match call.arg_opt(th, 0) {
            None => Self::thread_can_yield(th),
            // the running thread is taken out of the arena, so read
            // its flag from the live `th`, not the placeholder
            Some(Value::Thread(t)) if t == self.current_thread => Self::thread_can_yield(th),
            Some(Value::Thread(t)) => Self::thread_can_yield(&self.threads[t.0 as usize]),
            Some(v) => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #1 to 'isyieldable' (thread expected, got {})",
                        v.type_name()
                    ),
                ));
            }
        };
        place_shaped(th, call.ret_to, call.nres, call.shape, &[Value::Bool(r)]);
        Ok(())
    }

    fn intrinsic_collect_garbage(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        // An intrinsic so the running thread (taken out of the arena
        // during dispatch) can be passed as a GC root.
        let r = self
            .gc_command(
                th,
                call.arg(th, 0),
                (call.argc > 1).then(|| call.arg(th, 1)),
                call.func_abs + 1 + call.argc,
            )
            .map_err(|m| Self::rt_err(th, m))?;
        place_shaped(th, call.ret_to, call.nres, call.shape, &r);
        Ok(())
    }

    fn intrinsic_print(&mut self, th: &mut Thread, call: IntrinsicCall) -> Result<(), VmError> {
        let items: Vec<Value> = (0..call.argc).map(|i| call.arg(th, i)).collect();
        self.print_job = Some(PrintJob {
            items,
            idx: 0,
            out: Vec::new(),
            ret_to: call.ret_to,
            nres: call.nres,
            shape: call.shape,
            result_slot: 0,
            awaiting: false,
        });
        th.frames
            .last_mut()
            .unwrap()
            .pending_mut()
            .push(Pending::PrintStep);
        Ok(())
    }

    fn intrinsic_coroutine_close(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let co = call.arg_opt(th, 0);
        let Value::Thread(co) = co.unwrap_or(Value::Nil) else {
            let got = match co {
                Some(v) => v.type_name().to_string(),
                None => "no value".to_string(),
            };
            return Err(Self::rt_err(
                th,
                format!("bad argument #1 to 'coroutine.close' (thread expected, got {got})"),
            ));
        };
        self.begin_close(th, fuel, co, call.ret_to, call.nres, call.shape)
    }

    fn intrinsic_debug_getinfo(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let a0 = call.arg_opt(th, 0);
        let (target, base) = match a0 {
            Some(Value::Thread(t)) => (Some(t), 1usize),
            _ => (None, 0usize),
        };
        let f = call.arg_opt(th, base);
        let what = call.arg(th, base + 1);
        let r = self
            .debug_getinfo(th, target, f, what, base + 1)
            .map_err(|m| Self::rt_err(th, m))?;
        place_shaped(th, call.ret_to, call.nres, call.shape, &[r]);
        Ok(())
    }

    fn intrinsic_debug_traceback(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let a0 = call.arg_opt(th, 0);
        let (target, base) = match a0 {
            Some(Value::Thread(t)) => (Some(t), 1usize),
            _ => (None, 0usize),
        };
        let message = call.arg(th, base);
        let level = call.arg_opt(th, base + 1);
        let r = self
            .debug_traceback(th, target, message, level, base + 2)
            .map_err(|m| Self::rt_err(th, m))?;
        place_shaped(th, call.ret_to, call.nres, call.shape, &[r]);
        Ok(())
    }

    fn intrinsic_debug_getupvalue(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        // PUC checks the index (arg #2) before the function (arg #1).
        let n = self
            .debug_check_int(call.arg_opt(th, 1), 2, "debug.getupvalue")
            .map_err(|m| Self::rt_err(th, m))?;
        let f = call.arg(th, 0);
        match f {
            Value::Closure(cid) => match self.debug_getupvalue(th, cid, n) {
                Some((name, val)) => {
                    let nv = self.new_string(name.as_bytes());
                    place_shaped(th, call.ret_to, call.nres, call.shape, &[nv, val]);
                }
                // Out of range: PUC returns no values.
                None => place_shaped(th, call.ret_to, call.nres, call.shape, &[]),
            },
            // A native is a C function: it is a valid function with no
            // upvalues, so `lua_getupvalue` returns NULL -> zero values.
            Value::Native(_) => place_shaped(th, call.ret_to, call.nres, call.shape, &[]),
            other => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #1 to 'debug.getupvalue' (function expected, got {})",
                        other.type_name()
                    ),
                ));
            }
        }
        Ok(())
    }

    fn intrinsic_debug_setupvalue(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        // PUC checks the value (arg #3), then index (#2), then
        // function (#1).
        if call.arg_opt(th, 2).is_none() {
            return Err(Self::rt_err(
                th,
                "bad argument #3 to 'debug.setupvalue' (value expected)".into(),
            ));
        }
        let n = self
            .debug_check_int(call.arg_opt(th, 1), 2, "debug.setupvalue")
            .map_err(|m| Self::rt_err(th, m))?;
        let v = call.arg(th, 2);
        let f = call.arg(th, 0);
        match f {
            Value::Closure(cid) => match self.debug_setupvalue(th, cid, n, v) {
                Some(name) => {
                    let nv = self.new_string(name.as_bytes());
                    place_shaped(th, call.ret_to, call.nres, call.shape, &[nv]);
                }
                None => place_shaped(th, call.ret_to, call.nres, call.shape, &[]),
            },
            // Native (C) functions have no upvalues: `lua_setupvalue`
            // returns NULL, and the API reports zero values.
            Value::Native(_) => place_shaped(th, call.ret_to, call.nres, call.shape, &[]),
            other => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #1 to 'debug.setupvalue' (function expected, got {})",
                        other.type_name()
                    ),
                ));
            }
        }
        Ok(())
    }

    fn intrinsic_debug_upvalueid(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let n = self
            .debug_check_int(call.arg_opt(th, 1), 2, "debug.upvalueid")
            .map_err(|m| Self::rt_err(th, m))?;
        let f = call.arg(th, 0);
        let r = match f {
            Value::Closure(cid) => self.debug_upvalueid(cid, n),
            // `lua_upvalueid` returns NULL for a C function (no
            // upvalues); PUC pushes fail (nil) as a single value.
            Value::Native(_) => Value::Nil,
            other => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #1 to 'debug.upvalueid' (function expected, got {})",
                        other.type_name()
                    ),
                ));
            }
        };
        place_shaped(th, call.ret_to, call.nres, call.shape, &[r]);
        Ok(())
    }

    fn intrinsic_debug_upvaluejoin(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        // PUC's `checkupval` validates each (function, index) pair in
        // order: index (#2/#4) then function (#1/#3) then upvalue
        // existence. A native has no upvalues, so it fails the index
        // check; a non-function fails the type check.
        let n1 = self
            .debug_check_int(call.arg_opt(th, 1), 2, "debug.upvaluejoin")
            .map_err(|m| Self::rt_err(th, m))?;
        let c1 = self
            .debug_check_upval(call.arg(th, 0), n1, 1, 2, "debug.upvaluejoin")
            .map_err(|m| Self::rt_err(th, m))?;
        let n2 = self
            .debug_check_int(call.arg_opt(th, 3), 4, "debug.upvaluejoin")
            .map_err(|m| Self::rt_err(th, m))?;
        let c2 = self
            .debug_check_upval(call.arg(th, 2), n2, 3, 4, "debug.upvaluejoin")
            .map_err(|m| Self::rt_err(th, m))?;
        self.debug_upvaluejoin(c1, n1, c2, n2)
            .map_err(|m| Self::rt_err(th, m))?;
        place_shaped(th, call.ret_to, call.nres, call.shape, &[]);
        Ok(())
    }

    fn intrinsic_debug_getmetatable(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let v = call.arg(th, 0);
        let r = match self.get_metatable(v) {
            Some(mt) => Value::Table(mt),
            None => Value::Nil,
        };
        place_shaped(th, call.ret_to, call.nres, call.shape, &[r]);
        Ok(())
    }

    fn intrinsic_debug_setmetatable(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let v = call.arg(th, 0);
        let mt = match call.arg(th, 1) {
            Value::Nil => None,
            Value::Table(t) => Some(t),
            other => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #2 to 'setmetatable' (nil or table expected, got {})",
                        other.type_name()
                    ),
                ));
            }
        };
        self.set_raw_metatable(v, mt);
        place_shaped(th, call.ret_to, call.nres, call.shape, &[v]);
        Ok(())
    }

    fn intrinsic_debug_gethook(
        &mut self,
        th: &mut Thread,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        let target = match call.arg_opt(th, 0) {
            None | Some(Value::Nil) => None,
            Some(Value::Thread(t)) => Some(t),
            Some(v) => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #1 to 'gethook' (thread expected, got {})",
                        v.type_name()
                    ),
                ));
            }
        };
        let (hook, mask, count) = match target {
            Some(t) if t != self.current_thread => {
                let o = &self.threads[t.0 as usize];
                (o.hook.unwrap_or(Value::Nil), o.hook_mask, o.hook_count)
            }
            _ => (th.hook.unwrap_or(Value::Nil), th.hook_mask, th.hook_count),
        };
        // PUC returns a single `nil` (fail) when no hook is set,
        // otherwise the hook, its mask string, and the count.
        if hook == Value::Nil {
            place_shaped(th, call.ret_to, call.nres, call.shape, &[Value::Nil]);
            return Ok(());
        }
        let mut s = String::new();
        if mask & HOOK_CALL != 0 {
            s.push('c');
        }
        if mask & HOOK_RETURN != 0 {
            s.push('r');
        }
        if mask & HOOK_LINE != 0 {
            s.push('l');
        }
        let sv = self.new_string(s.as_bytes());
        place_shaped(
            th,
            call.ret_to,
            call.nres,
            call.shape,
            &[hook, sv, Value::Int(count)],
        );
        Ok(())
    }

    fn intrinsic_debug_sethook(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        call: IntrinsicCall,
    ) -> Result<(), VmError> {
        // `debug.sethook([thread,] hook, mask [, count])`
        let (target, base_index) = match call.arg_opt(th, 0) {
            Some(Value::Thread(t)) => (Some(t), 1usize),
            _ => (None, 0usize),
        };
        let hook_opt = match call.arg(th, base_index) {
            Value::Nil => None,
            v @ (Value::Closure(_) | Value::Native(_)) => Some(v),
            other => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #{} to 'sethook' (function expected, got {})",
                        base_index + 1,
                        other.type_name()
                    ),
                ));
            }
        };
        let mut bits = 0u8;
        match call.arg_opt(th, base_index + 1) {
            None | Some(Value::Nil) => {}
            Some(Value::Str(sid)) => {
                for &c in self.strings.get(sid) {
                    match c {
                        b'c' => bits |= HOOK_CALL,
                        b'r' => bits |= HOOK_RETURN,
                        b'l' => bits |= HOOK_LINE,
                        _ => {}
                    }
                }
            }
            Some(v) => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #{} to 'sethook' (string expected, got {})",
                        base_index + 2,
                        v.type_name()
                    ),
                ));
            }
        }
        let count = match call.arg_opt(th, base_index + 2) {
            None | Some(Value::Nil) => 0,
            Some(Value::Int(n)) => n,
            Some(Value::Float(f)) if f.fract() == 0.0 => f as i64,
            Some(v) => {
                return Err(Self::rt_err(
                    th,
                    format!(
                        "bad argument #{} to 'sethook' (number expected, got {})",
                        base_index + 3,
                        v.type_name()
                    ),
                ));
            }
        };
        let apply = |t: &mut Thread| {
            t.hook = hook_opt;
            t.hook_mask = bits;
            t.hook_count = count.max(0);
            t.hook_counter = count.max(0);
        };
        match target {
            Some(t) if t != self.current_thread => {
                apply(&mut self.threads[t.0 as usize]);
            }
            _ => apply(th),
        }
        // `debug.sethook` is itself a C function: if a return hook is
        // now active on this thread, its return fires the hook (and the
        // hook it just set observes it). A clearing call has already
        // removed the hook, so nothing fires.
        place_shaped(th, call.ret_to, call.nres, call.shape, &[]);
        if self.hook_suppress == 0 && Self::hook_on(th, HOOK_RETURN) {
            self.fire_hook(th, fuel, "return", -1)?;
        }
        Ok(())
    }

    /// Number of resumers above `parent` in the live coroutine chain.
    fn coroutine_depth(&self, parent: Option<ThreadId>) -> usize {
        let mut depth = 0;
        let mut cur = parent;
        while let Some(p) = cur {
            depth += 1;
            if depth > 1000 {
                break;
            }
            cur = self.threads[p.0 as usize].parent;
        }
        depth
    }

    /// Shared by `coroutine.resume` and wrapped coroutines.
    #[expect(clippy::too_many_arguments)]
    fn resume_thread(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        co: ThreadId,
        args: &[Value],
        ret_to: usize,
        nres: u8,
        shape: RetShape,
        wrap: bool,
    ) -> Result<(), VmError> {
        let fail = |me: &mut Self, th: &mut Thread, msg: &str| -> Result<(), VmError> {
            if wrap {
                Err(Self::rt_err(th, msg.into()))
            } else {
                let m = me.new_string(msg.as_bytes());
                place_shaped(th, ret_to, nres, shape, &[Value::Bool(false), m]);
                Ok(())
            }
        };
        if co == self.current_thread {
            return fail(self, th, "cannot resume non-suspended coroutine");
        }
        // PUC bounds the chain of nested resumes with its C-stack limit; the
        // classic `function(a) coroutine.wrap(a)(a) end` loop must fail with
        // "C stack overflow" rather than allocate coroutines without bound.
        if self.coroutine_depth(th.parent) >= 190 {
            return fail(self, th, "C stack overflow");
        }
        let status = self.threads[co.0 as usize].status;
        match status {
            CoStatus::Dead => fail(self, th, "cannot resume dead coroutine"),
            CoStatus::Running | CoStatus::Normal => {
                fail(self, th, "cannot resume non-suspended coroutine")
            }
            CoStatus::Start | CoStatus::Suspended => {
                *fuel -= 3;
                let rr = ResumeRet {
                    ret_to,
                    nres,
                    shape,
                    status_bool: !wrap,
                    wrap,
                };
                {
                    let co_th = &mut self.threads[co.0 as usize];
                    co_th.resume_ret = Some(rr);
                    co_th.parent = Some(self.current_thread);
                }
                if status == CoStatus::Start {
                    // body function was stashed at stack[0] by create()
                    let f = self.threads[co.0 as usize].stack[0];
                    match f {
                        Value::Closure(cid) => {
                            let proto = self.closures[cid.0 as usize].proto.clone();
                            let co_th = &mut self.threads[co.0 as usize];
                            let np = proto.nparams as usize;
                            ensure_len(
                                &mut co_th.stack,
                                1 + (proto.max_regs as usize).max(args.len()),
                            );
                            co_th.stack[1..=args.len()].copy_from_slice(args);
                            for i in args.len()..np {
                                co_th.stack[1 + i] = Value::Nil;
                            }
                            let varargs = if proto.is_vararg && args.len() > np {
                                args[np..].to_vec()
                            } else {
                                Vec::new()
                            };
                            co_th.status = CoStatus::Running;
                            co_th.frames.push(Frame::Lua(LuaFrame {
                                closure: cid,
                                proto,
                                pc: 0,
                                base: 1,
                                ret_to: 0,
                                nres: 0,
                                shape: RetShape::Normal,
                                protected: false,
                                handler: None,
                                handler_guard: false,
                                pending: Vec::new(),
                                tbc: Vec::new(),
                                varargs,
                                tailcall: false,
                                call_meta: None,
                                last_line: -1,
                                is_hook: false,
                            }));
                            // A hook set on this coroutine before it started
                            // must see the body's "call" event.
                            co_th.pending_call_hook = true;
                        }
                        Value::Native(nid) => {
                            let is_pcall = matches!(
                                self.natives[nid.0 as usize].kind,
                                NativeKind::Intrinsic(Intrinsic::Pcall)
                            );
                            let is_xpcall = matches!(
                                self.natives[nid.0 as usize].kind,
                                NativeKind::Intrinsic(Intrinsic::Xpcall)
                            );
                            if is_pcall || is_xpcall {
                                // `coroutine.create(pcall)` / `(xpcall)`: run the
                                // protected call inside the coroutine thread. The
                                // protected frame stays on the coroutine stack, so
                                // a yield from the callee suspends and resumes it
                                // as usual.
                                let mut co_th = std::mem::take(&mut self.threads[co.0 as usize]);
                                let extra = if is_xpcall {
                                    if args.len() < 2 {
                                        self.threads[co.0 as usize] = co_th;
                                        return fail(
                                            self,
                                            th,
                                            "bad argument #2 to 'xpcall' (value expected)",
                                        );
                                    }
                                    args.len() - 2
                                } else {
                                    if args.is_empty() {
                                        self.threads[co.0 as usize] = co_th;
                                        return fail(
                                            self,
                                            th,
                                            "bad argument #1 to 'pcall' (value expected)",
                                        );
                                    }
                                    args.len() - 1
                                };
                                ensure_len(&mut co_th.stack, 3 + extra);
                                co_th.stack[0] = Value::Native(nid);
                                co_th.stack[1] = args[0];
                                if is_xpcall {
                                    co_th.stack[2..2 + extra].copy_from_slice(&args[2..]);
                                } else {
                                    co_th.stack[2..2 + extra].copy_from_slice(&args[1..]);
                                }
                                co_th.top = 2 + extra;
                                co_th.status = CoStatus::Running;
                                let handler = if is_xpcall { Some(args[1]) } else { None };
                                let r = self.protected_call(
                                    &mut co_th, fuel, 1, extra, 0, 0, handler, true,
                                );
                                self.threads[co.0 as usize] = co_th;
                                r?;
                            } else {
                                // native coroutine body: cannot yield; run it to
                                // completion right here
                                let co_th = &mut self.threads[co.0 as usize];
                                co_th.status = CoStatus::Dead;
                                co_th.parent = None;
                                co_th.resume_ret = None;
                                let native_is_error = matches!(
                                    self.natives[nid.0 as usize].kind,
                                    NativeKind::Intrinsic(Intrinsic::Error)
                                );
                                if native_is_error {
                                    // `coroutine.create(error)`; error() with a
                                    // non-string argument raises that value.
                                    let errv = args.first().copied().unwrap_or(Value::Nil);
                                    self.threads[co.0 as usize].close_error = Some(errv);
                                    if wrap {
                                        return Err(VmError {
                                            val: ErrVal::Val(errv),
                                            line: 0,
                                            root_line: 0,
                                            source: None,
                                        });
                                    }
                                    place_shaped(
                                        th,
                                        ret_to,
                                        nres,
                                        shape,
                                        &[Value::Bool(false), errv],
                                    );
                                    return Ok(());
                                }
                                let native_is_print = matches!(
                                    self.natives[nid.0 as usize].kind,
                                    NativeKind::Intrinsic(Intrinsic::Print)
                                );
                                let kind_result = if native_is_print {
                                    // `coroutine.create(print)`: render directly
                                    // (no frame is available to drive
                                    // `__tostring` here).
                                    let line = args
                                        .iter()
                                        .map(|v| self.tostring_default(*v))
                                        .collect::<Vec<_>>()
                                        .join("\t");
                                    println!("{line}");
                                    Ok(Vec::new())
                                } else {
                                    match &self.natives[nid.0 as usize].kind {
                                        NativeKind::Plain(f) => f(self, args),
                                        NativeKind::Suspendable(_) => Err(
                                            "cannot use a suspendable native as a coroutine body"
                                                .into(),
                                        ),
                                        NativeKind::Intrinsic(_) => {
                                            Err("cannot use this builtin as a coroutine body"
                                                .into())
                                        }
                                    }
                                };
                                return match kind_result {
                                    Ok(res) => {
                                        let mut all = Vec::with_capacity(res.len() + 1);
                                        if wrap {
                                            place_shaped(th, ret_to, nres, shape, &res);
                                        } else {
                                            all.push(Value::Bool(true));
                                            all.extend_from_slice(&res);
                                            place_shaped(th, ret_to, nres, shape, &all);
                                        }
                                        Ok(())
                                    }
                                    Err(msg) => fail(self, th, &msg),
                                };
                            }
                        }
                        _ => return fail(self, th, "cannot resume dead coroutine"),
                    }
                } else {
                    let co_th = &mut self.threads[co.0 as usize];
                    co_th.status = CoStatus::Running;
                    if let Some((yret, ynres, yshape)) = co_th.yield_ret.take() {
                        // deliver resume args as the pending yield's results
                        place_shaped(co_th, yret, ynres, yshape, args);
                    }
                    // else: parked on a native wait, which has no yield site.
                    // Resume arguments are discarded; the wait's completion
                    // (if the host has supplied one) drives the call to
                    // completion in dispatch.
                }
                th.status = CoStatus::Normal;
                self.switch_to = Some(co);
                Ok(())
            }
        }
    }

    /// Starts `coroutine.close(co)`. Dead-and-clean and to-be-closed-free
    /// coroutines resolve immediately; otherwise a [`CloseJob`] is installed
    /// and driven by [`Pending::CloseStep`].
    fn begin_close(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        co: ThreadId,
        ret_to: usize,
        nres: u8,
        shape: RetShape,
    ) -> Result<(), VmError> {
        // Closing the running/normal coroutine is an error (catchable with
        // pcall), not a `false, msg` result.
        if co == self.current_thread {
            return Err(Self::rt_err(th, "cannot close a running coroutine".into()));
        }
        match self.threads[co.0 as usize].status {
            CoStatus::Running => Err(Self::rt_err(th, "cannot close a running coroutine".into())),
            CoStatus::Normal => Err(Self::rt_err(th, "cannot close a normal coroutine".into())),
            CoStatus::Dead => {
                let err = self.threads[co.0 as usize].close_error.take();
                match err {
                    Some(e) => place_shaped(th, ret_to, nres, shape, &[Value::Bool(false), e]),
                    None => place_shaped(th, ret_to, nres, shape, &[Value::Bool(true)]),
                }
                Ok(())
            }
            CoStatus::Start | CoStatus::Suspended => {
                let mut items = Vec::new();
                {
                    let ct = &self.threads[co.0 as usize];
                    for f in ct.frames.iter().rev() {
                        let Some(f) = f.lua() else { continue };
                        for &r in f.tbc.iter().rev() {
                            items.push(ct.stack[f.base + r as usize]);
                        }
                    }
                }
                if items.is_empty() {
                    self.finish_close(co);
                    place_shaped(th, ret_to, nres, shape, &[Value::Bool(true)]);
                    return Ok(());
                }
                // Mark running so a reentrant close from inside a __close
                // handler reports "cannot close a running coroutine".
                self.threads[co.0 as usize].status = CoStatus::Running;
                self.close_job = Some(CloseJob {
                    target: co,
                    items,
                    idx: 0,
                    err: Value::Nil,
                    failed: false,
                    ret_to,
                    nres,
                    shape,
                    result_slot: 0,
                    awaiting: false,
                });
                th.frames
                    .last_mut()
                    .unwrap()
                    .pending_mut()
                    .push(Pending::CloseStep);
                *fuel -= 1;
                Ok(())
            }
        }
    }

    /// One step of the [`CloseJob`] state machine: consume a pending result,
    /// then either call the next `__close` (protected, staged through
    /// `Pending::CloseStep`) or deliver the final `true`/`false, err`.
    fn close_step(&mut self, th: &mut Thread, fuel: &mut i64) -> Result<(), VmError> {
        let Some(mut job) = self.close_job.take() else {
            return Ok(());
        };
        if job.awaiting {
            job.awaiting = false;
            if th.stack[job.result_slot] == Value::Bool(false) {
                job.err = th.stack[job.result_slot + 1];
                job.failed = true;
            }
        }
        if job.idx < job.items.len() {
            let item = job.items[job.idx];
            job.idx += 1;
            let mm = self.metamethod_pub(item, "__close");
            if mm == Value::Nil {
                th.frames
                    .last_mut()
                    .unwrap()
                    .pending_mut()
                    .push(Pending::CloseStep);
                self.close_job = Some(job);
                return Ok(());
            }
            let scratch = scratch_base(th);
            ensure_len(&mut th.stack, scratch + 3);
            th.stack[scratch] = mm;
            th.stack[scratch + 1] = item;
            th.stack[scratch + 2] = job.err;
            let result_slot = scratch + 3;
            ensure_len(&mut th.stack, result_slot + 2);
            job.result_slot = result_slot;
            job.awaiting = true;
            th.frames
                .last_mut()
                .unwrap()
                .pending_mut()
                .push(Pending::CloseStep);
            self.close_job = Some(job);
            *fuel -= 1;
            // nres = 0 (multret) so an error's `false, err` pair both land.
            return self.protected_call(th, fuel, scratch, 2, result_slot, 0, None, true);
        }
        // all handlers ran: kill the target and report
        self.finish_close(job.target);
        let results: [Value; 2] = [Value::Bool(!job.failed), job.err];
        let slice: &[Value] = if job.failed { &results } else { &results[..1] };
        place_shaped(th, job.ret_to, job.nres, job.shape, slice);
        self.close_job = None;
        Ok(())
    }

    /// One step of the [`PrintJob`] state machine: consume a `__tostring`
    /// result, then either call the next one or emit the line.
    fn print_step(&mut self, th: &mut Thread, fuel: &mut i64) -> Result<(), VmError> {
        let Some(mut job) = self.print_job.take() else {
            return Ok(());
        };
        if job.awaiting {
            job.awaiting = false;
            let v = th.stack[job.result_slot];
            if let Value::Str(s) = v {
                job.out.push(self.strings.get(s).to_vec());
            } else {
                self.print_job = Some(job);
                return Err(Self::rt_err(th, "'__tostring' must return a string".into()));
            }
        }
        while job.idx < job.items.len() {
            let v = job.items[job.idx];
            job.idx += 1;
            let mm = self.metamethod(v, Mm::ToString);
            if mm != Value::Nil {
                let result_slot = scratch_base(th);
                ensure_len(&mut th.stack, result_slot + 1);
                job.result_slot = result_slot;
                job.awaiting = true;
                th.frames
                    .last_mut()
                    .unwrap()
                    .pending_mut()
                    .push(Pending::PrintStep);
                self.print_job = Some(job);
                // nres = 2 -> exactly one result
                return self.call_value(th, mm, &[v], result_slot, 2, RetShape::Normal, fuel);
            }
            job.out.push(self.tostring_default(v).into_bytes());
        }
        let line = job
            .out
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
            .join("\t");
        // Deviation: `print` writes to the process stdout directly, not through
        // the installed host's `stdout_write` sink. This matches PUC (whose
        // `print` targets C `stdout`) and keeps tests that install a capturing
        // host — or run without one — working unchanged; hosts that want to
        // capture `print` output should use `io.write`/`io.stdout` instead.
        println!("{line}");
        place_shaped(th, job.ret_to, job.nres, job.shape, &[]);
        self.print_job = None;
        Ok(())
    }

    /// One step of the [`FormatJob`] state machine. Consumes a resolved
    /// `__tostring` result when one is pending, then renders format items
    /// left-to-right until the whole string is built or another `%s` needs
    /// `__tostring`.
    fn format_step(&mut self, th: &mut Thread, fuel: &mut i64) -> Result<(), VmError> {
        let Some(mut job) = self.format_job.take() else {
            return Ok(());
        };
        if job.awaiting {
            job.awaiting = false;
            let v = th.stack[job.result_slot];
            let bytes = match v {
                Value::Str(id) => self.strings.get(id).to_vec(),
                Value::Int(_) | Value::Float(_) => crate::value::fmt_number(v).into_bytes(),
                _ => {
                    self.format_job = Some(job);
                    return Err(Self::rt_err(th, "'__tostring' must return a string".into()));
                }
            };
            let (spec, arg, argi) = job.saved.take().expect("saved spec while awaiting");
            let piece = crate::stdlib::string::render_spec(self, &spec, arg, argi, Some(bytes))
                .map_err(|m| Self::rt_err(th, m))?;
            job.out.extend_from_slice(&piece);
        }
        while job.pos < job.fmt.len() {
            let c = job.fmt[job.pos];
            if c != b'%' {
                job.out.push(c);
                job.pos += 1;
                continue;
            }
            if job.fmt.get(job.pos + 1) == Some(&b'%') {
                job.out.push(b'%');
                job.pos += 2;
                continue;
            }
            job.arg += 1;
            if job.arg > job.args.len() {
                let arg = job.arg;
                self.format_job = Some(job);
                return Err(Self::rt_err(
                    th,
                    format!("bad argument #{arg} to 'format' (no value)"),
                ));
            }
            let Some(spec) = crate::stdlib::string::parse_spec(&job.fmt, job.pos)
                .map_err(|m| Self::rt_err(th, m))?
            else {
                break;
            };
            let v = job.args[job.arg - 1];
            job.pos = spec.next;
            if spec.conv == b's' {
                let mm = self.metamethod(v, Mm::ToString);
                if mm != Value::Nil {
                    let result_slot = scratch_base(th);
                    ensure_len(&mut th.stack, result_slot + 1);
                    job.result_slot = result_slot;
                    job.awaiting = true;
                    let argi = job.arg;
                    job.saved = Some((spec, v, argi));
                    th.frames
                        .last_mut()
                        .unwrap()
                        .pending_mut()
                        .push(Pending::FormatStep);
                    self.format_job = Some(job);
                    return self.call_value(th, mm, &[v], result_slot, 2, RetShape::Normal, fuel);
                }
            }
            let piece = crate::stdlib::string::render_spec(self, &spec, v, job.arg, None)
                .map_err(|m| Self::rt_err(th, m))?;
            job.out.extend_from_slice(&piece);
        }
        let sv = self.new_string(&job.out);
        place_shaped(th, job.ret_to, job.nres, job.shape, &[sv]);
        self.format_job = None;
        Ok(())
    }

    /// Puts a coroutine into the dead state, dropping its frames and stack.
    fn finish_close(&mut self, co: ThreadId) {
        // Closures that escaped the coroutine can still reference its open
        // upvalues; close them before the stack disappears, or a later read or
        // write would index a dead stack.
        let mut ct = std::mem::take(&mut self.threads[co.0 as usize]);
        self.close_upvals(&mut ct, 0);
        ct.status = CoStatus::Dead;
        ct.frames.clear();
        ct.stack.clear();
        ct.open_upvals.clear();
        ct.parent = None;
        ct.resume_ret = None;
        ct.yield_ret = None;
        ct.pending_native = None;
        ct.close_error = None;
        self.threads[co.0 as usize] = ct;
    }

    /// Calls the value at `f_abs` under error protection: results arrive as
    /// `true, ...` on success and `false, err` on failure (via the handler
    /// for xpcall).
    #[expect(clippy::too_many_arguments)]
    fn protected_call(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        f_abs: usize,
        argc: usize,
        ret_to: usize,
        nres: u8,
        handler: Option<Value>,
        with_boundary: bool,
    ) -> Result<(), VmError> {
        // Push the boundary frame *below* the protected callee: it is where the
        // `__close`/error-delivery continuations are staged once the callee is
        // unwound or returns.
        if with_boundary {
            let base = th.frames.last().map_or(th.top, |_| scratch_base(th));
            // A native callee has no Lua frame to carry the protection flag,
            // so this boundary frame is itself the protection boundary for any
            // continuation it later stages (e.g. `pcall(tostring, v)` calling
            // `__tostring`).
            let native_boundary = !matches!(th.stack[f_abs], Value::Closure(_));
            th.frames.push(Frame::Boundary(BoundaryFrame {
                base,
                pending: Vec::new(),
                boundary: native_boundary.then_some(CBoundary {
                    ret_to,
                    nres,
                    handler,
                }),
            }));
        }
        let r = self.do_call(
            th,
            fuel,
            &CallSpec {
                func_abs: f_abs,
                argc,
                ret_to,
                nres,
                shape: RetShape::PrependTrue,
                protected: true,
                handler,
                native_caller: true,
            },
        );
        match r {
            Ok(()) => Ok(()),
            // The callee failed before a protected frame existed (native
            // error, not callable). Catch here.
            Err(e) => {
                let errv = self.err_value(&e);
                match handler {
                    None => {
                        place_results(th, ret_to, nres, &[Value::Bool(false), errv]);
                        Ok(())
                    }
                    Some(h) => {
                        match self.call_value(
                            th,
                            h,
                            &[errv],
                            ret_to,
                            nres,
                            RetShape::PrependFalse,
                            fuel,
                        ) {
                            Ok(()) => Ok(()),
                            Err(e2) => {
                                let v2 = self.err_value(&e2);
                                place_results(th, ret_to, nres, &[Value::Bool(false), v2]);
                                Ok(())
                            }
                        }
                    }
                }
            }
        }
    }

    /// `o[k]` honoring `__index` chains. `Some(v)` for an immediate result;
    /// `None` when a metamethod call was started (its result will land in
    /// `dst_abs` when the frame returns).
    fn index_chain(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        o: Value,
        k: Value,
        dst_abs: usize,
    ) -> Result<Option<Value>, VmError> {
        let mut cur = o;
        for _ in 0..MAX_META_CHAIN {
            let mm = if let Value::Table(t) = cur {
                let raw = self.tables[t.0 as usize].get(k);
                if raw != Value::Nil {
                    return Ok(Some(raw));
                }
                let mm = self.metamethod(cur, Mm::Index);
                if mm == Value::Nil {
                    return Ok(Some(Value::Nil));
                }
                mm
            } else {
                let mm = self.metamethod(cur, Mm::Index);
                if mm == Value::Nil {
                    return Err(Self::rt_err(
                        th,
                        format!("attempt to index a {} value", cur.type_name()),
                    ));
                }
                mm
            };
            match mm {
                Value::Closure(_) | Value::Native(_) => {
                    self.call_value(th, mm, &[cur, k], dst_abs, 2, RetShape::Normal, fuel)?;
                    return Ok(None);
                }
                _ => cur = mm,
            }
        }
        Err(Self::rt_err(
            th,
            "'__index' chain too long; possible loop".into(),
        ))
    }

    /// `o[k] = v` honoring `__newindex` chains.
    fn newindex_chain(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        o: Value,
        k: Value,
        v: Value,
    ) -> Result<(), VmError> {
        let mut cur = o;
        for _ in 0..MAX_META_CHAIN {
            let mm = if let Value::Table(t) = cur {
                if self.tables[t.0 as usize].get(k) != Value::Nil {
                    self.tables[t.0 as usize]
                        .set(k, v)
                        .map_err(|m| Self::rt_err(th, m.to_string()))?;
                    return Ok(());
                }
                let mm = self.metamethod(cur, Mm::NewIndex);
                if mm == Value::Nil {
                    self.tables[t.0 as usize]
                        .set(k, v)
                        .map_err(|m| Self::rt_err(th, m.to_string()))?;
                    return Ok(());
                }
                mm
            } else {
                let mm = self.metamethod(cur, Mm::NewIndex);
                if mm == Value::Nil {
                    return Err(Self::rt_err(
                        th,
                        format!("attempt to index a {} value", cur.type_name()),
                    ));
                }
                mm
            };
            match mm {
                Value::Closure(_) | Value::Native(_) => {
                    // discard results
                    let scratch = scratch_base(th);
                    self.call_value(th, mm, &[cur, k, v], scratch, 1, RetShape::Normal, fuel)?;
                    return Ok(());
                }
                _ => cur = mm,
            }
        }
        Err(Self::rt_err(
            th,
            "'__newindex' chain too long; possible loop".into(),
        ))
    }

    fn compare(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        op: CmpOp,
        a: Value,
        b: Value,
        dst_abs: usize,
    ) -> Result<(), VmError> {
        match op {
            CmpOp::Eq | CmpOp::Ne => {
                let shape = if op == CmpOp::Eq {
                    RetShape::ToBool
                } else {
                    RetShape::ToNotBool
                };
                if values_equal(a, b) {
                    th.stack[dst_abs] = Value::Bool(op == CmpOp::Eq);
                    return Ok(());
                }
                // __eq fires only for table/table or userdata/userdata
                // raw-unequal pairs.
                if matches!(
                    (a, b),
                    (Value::Table(_), Value::Table(_)) | (Value::Userdata(_), Value::Userdata(_))
                ) {
                    let mm = self.binary_mm(a, b, Mm::Eq);
                    if mm != Value::Nil {
                        return self.call_value(th, mm, &[a, b], dst_abs, 2, shape, fuel);
                    }
                }
                th.stack[dst_abs] = Value::Bool(op == CmpOp::Ne);
                Ok(())
            }
            CmpOp::Lt | CmpOp::Le => {
                let or_equal = op == CmpOp::Le;
                if let Some(r) = self.less_than(a, b, or_equal) {
                    th.stack[dst_abs] = Value::Bool(r);
                    Ok(())
                } else {
                    let mm = self.binary_mm(a, b, if or_equal { Mm::Le } else { Mm::Lt });
                    if mm == Value::Nil {
                        return Err(Self::rt_err(
                            th,
                            format!(
                                "attempt to compare {} with {}",
                                a.type_name(),
                                b.type_name()
                            ),
                        ));
                    }
                    self.call_value(th, mm, &[a, b], dst_abs, 2, RetShape::ToBool, fuel)
                }
            }
        }
    }

    /// Runs `__close` handlers for to-be-closed variables at or above
    /// register `from`, one call per dispatch step (re-arms itself as a
    /// pending so suspension and nested calls work).
    fn run_close_tbc(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        from: u8,
        err: Value,
    ) -> Result<(), VmError> {
        let f = th.frames.last_mut().unwrap().as_lua_mut();
        match f.tbc.last() {
            Some(&r) if r >= from => {
                f.tbc.pop();
                let v = th.stack[f.base + r as usize];
                let mm = self.metamethod(v, Mm::Close);
                if mm == Value::Nil {
                    // PUC reports the missing metamethod at close time.
                    return Err(Self::rt_err(
                        th,
                        "attempt to call a nil value (metamethod 'close')".into(),
                    ));
                }
                th.frames
                    .last_mut()
                    .unwrap()
                    .pending_mut()
                    .push(Pending::CloseTbc { from, err });
                let scratch = scratch_base(th);
                let before = th.frames.len();
                let callable = match mm {
                    Value::Closure(_) | Value::Native(_) => true,
                    other => self.metamethod(other, Mm::Call) != Value::Nil,
                };
                let r = self.call_value(th, mm, &[v, err], scratch, 1, RetShape::Normal, fuel);
                if r.is_ok()
                    && th.frames.len() > before
                    && let Some(Frame::Lua(lf)) = th.frames.last_mut()
                {
                    // PUC resolves this call site (an `OP_CLOSE`) to the
                    // `close` metamethod, shown by tracebacks.
                    lf.call_meta = Some(("metamethod", "close"));
                }
                if callable {
                    r
                } else {
                    r.map_err(|mut e| {
                        annotate_metamethod_error(&mut e, "close");
                        e
                    })
                }
            }
            _ => Ok(()),
        }
    }

    /// Concatenation over registers, folding string/number runs directly
    /// and dispatching `__concat` pairs as metamethod calls with a pending
    /// continuation to resume the fold.
    fn concat_run(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        dst: u8,
        b: u8,
        n: u8,
    ) -> Result<(), VmError> {
        let base = th.frames.last().unwrap().as_lua().base;
        let first = base + b as usize;
        let mut n = n as usize;
        loop {
            if n == 1 {
                th.stack[base + dst as usize] = th.stack[first];
                return Ok(());
            }
            // longest all-concatable suffix [k, n)
            let mut k = n;
            while k > 0 && is_concatable(th.stack[first + k - 1]) {
                k -= 1;
            }
            if k == 0 {
                *fuel -= n as i64;
                let out = self.flat_concat(th, first, n);
                th.stack[base + dst as usize] = self.new_string(&out);
                return Ok(());
            }
            if n - k >= 2 {
                // fold the suffix into one string at position k
                *fuel -= (n - k) as i64;
                let out = self.flat_concat(th, first + k, n - k);
                th.stack[first + k] = self.new_string(&out);
                n = k + 1;
                continue;
            }
            // metamethod pair (v[n-2], v[n-1])
            let x = th.stack[first + n - 2];
            let y = th.stack[first + n - 1];
            let mm = self.binary_mm(x, y, Mm::Concat);
            if mm == Value::Nil {
                let bad = if is_concatable(x) { y } else { x };
                return Err(Self::rt_err(
                    th,
                    format!("attempt to concatenate a {} value", bad.type_name()),
                ));
            }
            let (ret_abs, pending) = if n - 1 == 1 {
                (base + dst as usize, None)
            } else {
                (
                    first + n - 2,
                    Some(Pending::Concat {
                        dst,
                        base: b,
                        n: (n - 1) as u8,
                    }),
                )
            };
            if let Some(p) = pending {
                th.frames.last_mut().unwrap().pending_mut().push(p);
            }
            return self.call_value(th, mm, &[x, y], ret_abs, 2, RetShape::Normal, fuel);
        }
    }

    fn flat_concat(&self, th: &Thread, first: usize, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..n {
            match th.stack[first + i] {
                Value::Str(s) => out.extend_from_slice(self.strings.get(s)),
                v @ (Value::Int(_) | Value::Float(_)) => {
                    out.extend_from_slice(fmt_number(v).as_bytes());
                }
                _ => unreachable!("flat_concat on non-concatable"),
            }
        }
        out
    }

    fn for_prep(&mut self, th: &mut Thread, a: usize, off: i32) -> Result<(), VmError> {
        // PUC `forprep`: the integer path is taken only when the *control*
        // and *step* are actual integers; any numeric string (even when
        // integral) forces the float path.
        if let (Value::Int(i0), Value::Int(s)) = (th.stack[a], th.stack[a + 2]) {
            if s == 0 {
                return Err(Self::rt_err(th, "'for' step is zero".into()));
            }
            match self.for_limit(th, a + 1, s > 0)? {
                Some(l) if (s > 0 && i0 <= l) || (s < 0 && i0 >= l) => {
                    th.stack[a + 1] = Value::Int(l);
                    th.stack[a + 3] = Value::Int(i0);
                }
                _ => jump(th, off),
            }
        } else {
            // PUC checks limit, then step, then initial value.
            let limit = self.for_number(th, a + 1, "limit")?;
            let step = self.for_number(th, a + 2, "step")?;
            let init = self.for_number(th, a, "initial value")?;
            if step == 0.0 {
                return Err(Self::rt_err(th, "'for' step is zero".into()));
            }
            if (step > 0.0 && init <= limit) || (step < 0.0 && init >= limit) {
                th.stack[a] = Value::Float(init);
                th.stack[a + 1] = Value::Float(limit);
                th.stack[a + 2] = Value::Float(step);
                th.stack[a + 3] = Value::Float(init);
            } else {
                jump(th, off);
            }
        }
        Ok(())
    }

    /// PUC `forprep`'s `tonumber`: numbers pass through and numeric strings
    /// are parsed, all converted to `lua_Number` (f64). Anything else raises
    /// `'for' <what> must be a number`.
    fn for_number(&self, th: &Thread, idx: usize, what: &str) -> Result<f64, VmError> {
        let n = match th.stack[idx] {
            Value::Int(i) => Some(i as f64),
            Value::Float(f) => Some(f),
            Value::Str(s) => match crate::stdlib::parse_number(self.strings.get(s)) {
                Some(Value::Int(i)) => Some(i as f64),
                Some(Value::Float(f)) => Some(f),
                _ => None,
            },
            _ => None,
        };
        n.ok_or_else(|| Self::rt_err(th, format!("'for' {what} must be a number")))
    }

    /// PUC `forlimit`: coerce an integer loop's limit. Numeric strings are
    /// parsed; a float limit is rounded toward the loop interior (or clamped)
    /// by `for_int_limit`. `None` means the loop must not run.
    fn for_limit(
        &self,
        th: &Thread,
        idx: usize,
        step_positive: bool,
    ) -> Result<Option<i64>, VmError> {
        match th.stack[idx] {
            Value::Int(l) => Ok(Some(l)),
            Value::Float(f) => Ok(for_int_limit(f, step_positive)),
            Value::Str(s) => match crate::stdlib::parse_number(self.strings.get(s)) {
                Some(Value::Int(l)) => Ok(Some(l)),
                Some(Value::Float(f)) => Ok(for_int_limit(f, step_positive)),
                _ => Err(Self::rt_err(th, "'for' limit must be a number".into())),
            },
            _ => Err(Self::rt_err(th, "'for' limit must be a number".into())),
        }
    }

    fn unary(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        op: UnaryOp,
        v: Value,
        dst_abs: usize,
    ) -> Result<(), VmError> {
        let (result, mm) = match op {
            UnaryOp::Not => (Some(Value::Bool(!v.truthy())), None),
            UnaryOp::Neg => match v {
                Value::Int(i) => (Some(Value::Int(i.wrapping_neg())), None),
                Value::Float(f) => (Some(Value::Float(-f)), None),
                Value::Str(s) => match crate::stdlib::parse_number(self.strings.get(s)) {
                    Some(Value::Int(i)) => (Some(Value::Int(i.wrapping_neg())), None),
                    Some(Value::Float(f)) => (Some(Value::Float(-f)), None),
                    _ => (
                        None,
                        Some((
                            Mm::Unm,
                            format!("attempt to perform arithmetic on a {} value", v.type_name()),
                        )),
                    ),
                },
                _ => (
                    None,
                    Some((
                        Mm::Unm,
                        format!("attempt to perform arithmetic on a {} value", v.type_name()),
                    )),
                ),
            },
            UnaryOp::BNot => match to_int(v) {
                Some(i) => (Some(Value::Int(!i)), None),
                None => (
                    None,
                    Some((
                        Mm::BNot,
                        format!(
                            "attempt to perform bitwise operation on a {} value",
                            v.type_name()
                        ),
                    )),
                ),
            },
            UnaryOp::Len => match v {
                Value::Str(s) => (Some(Value::Int(self.strings.get(s).len() as i64)), None),
                Value::Table(t) => {
                    let mm = self.metamethod(v, Mm::Len);
                    if mm == Value::Nil {
                        (Some(Value::Int(self.tables[t.0 as usize].length())), None)
                    } else {
                        // PUC calls unary metamethods with the operand twice
                        // (`luaT_callTMres`), and `events.lua` observes the
                        // argument list __len is invoked with.
                        self.call_value(th, mm, &[v, v], dst_abs, 2, RetShape::Normal, fuel)?;
                        return Ok(());
                    }
                }
                _ => (
                    None,
                    Some((
                        Mm::Len,
                        format!("attempt to get length of a {} value", v.type_name()),
                    )),
                ),
            },
        };
        if let Some(r) = result {
            th.stack[dst_abs] = r;
            return Ok(());
        }
        let (mm_name, msg) = mm.unwrap();
        let m = self.metamethod(v, mm_name);
        if m == Value::Nil {
            return Err(Self::rt_err(th, msg));
        }
        // unary metamethods receive the operand twice (Lua convention)
        self.call_value(th, m, &[v, v], dst_abs, 2, RetShape::Normal, fuel)
    }

    /// `Some(result)` for primitively comparable values, `None` when a
    /// metamethod is required.
    fn less_than(&self, a: Value, b: Value, or_equal: bool) -> Option<bool> {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Some(if or_equal { x <= y } else { x < y }),
            (Value::Float(x), Value::Float(y)) => Some(if or_equal { x <= y } else { x < y }),
            (Value::Int(x), Value::Float(y)) => Some(if or_equal {
                int_le_float(x, y)
            } else {
                int_lt_float(x, y)
            }),
            (Value::Float(x), Value::Int(y)) => {
                if x.is_nan() {
                    return Some(false);
                }
                // x < y  <=>  not (y <= x);  x <= y  <=>  not (y < x)
                Some(if or_equal {
                    !int_lt_float(y, x)
                } else {
                    !int_le_float(y, x)
                })
            }
            (Value::Str(x), Value::Str(y)) => {
                let (xs, ys) = (self.strings.get(x), self.strings.get(y));
                Some(if or_equal { xs <= ys } else { xs < ys })
            }
            _ => None,
        }
    }

    fn frame_upval(&self, th: &Thread, up: u8) -> UpvalId {
        let cur = th.frames.last().unwrap().as_lua().closure;
        self.closures[cur.0 as usize].upvals[up as usize]
    }

    fn read_upval(&self, tid: ThreadId, th: &Thread, id: UpvalId) -> Value {
        match self.upvals[id.0 as usize] {
            Upval::Closed(v) => v,
            Upval::Open(t, idx) => {
                if t == tid {
                    th.stack[idx]
                } else {
                    self.threads[t.0 as usize].stack[idx]
                }
            }
        }
    }

    fn write_upval(&mut self, tid: ThreadId, th: &mut Thread, id: UpvalId, v: Value) {
        match self.upvals[id.0 as usize] {
            Upval::Closed(_) => self.upvals[id.0 as usize] = Upval::Closed(v),
            Upval::Open(t, idx) => {
                if t == tid {
                    th.stack[idx] = v;
                } else {
                    self.threads[t.0 as usize].stack[idx] = v;
                }
            }
        }
    }

    fn find_or_create_open(&mut self, tid: ThreadId, th: &mut Thread, abs: usize) -> UpvalId {
        match th.open_upvals.binary_search_by_key(&abs, |&(i, _)| i) {
            Ok(i) => th.open_upvals[i].1,
            Err(pos) => {
                let id = self.new_upval(Upval::Open(tid, abs));
                th.open_upvals.insert(pos, (abs, id));
                id
            }
        }
    }

    fn close_upvals(&mut self, th: &mut Thread, from_abs: usize) {
        while let Some(&(idx, id)) = th.open_upvals.last() {
            if idx < from_abs {
                break;
            }
            self.upvals[id.0 as usize] = Upval::Closed(th.stack[idx]);
            th.open_upvals.pop();
        }
    }

    fn rt_err(th: &Thread, message: String) -> VmError {
        let source = th
            .frames
            .last()
            .and_then(|f| f.lua())
            .map(|f| f.proto.source.clone());
        VmError {
            val: ErrVal::Msg(message),
            line: line_of(th),
            root_line: 0,
            source,
        }
    }

    // ---- garbage collection ----

    /// Pins a value so the host can hold it across collections. Without an
    /// anchor (or reachability from Lua), a `Value` held only in Rust can
    /// be collected and its handle becomes stale.
    pub fn anchor(&mut self, v: Value) {
        self.anchors.push(v);
    }

    /// Removes one anchor pin of `v`.
    pub fn unanchor(&mut self, v: Value) {
        if let Some(i) = self.anchors.iter().position(|&a| a == v) {
            self.anchors.swap_remove(i);
        }
    }

    /// Runs a full mark-sweep collection. Returns the (approximate) bytes
    /// in use afterwards.
    pub fn gc(&mut self) -> usize {
        self.collect(None);
        self.memory_used()
    }

    /// Runs queued `__gc` handlers, one per call, as protected calls so that
    /// an erroring finalizer cannot escape into the running program. Returns
    /// after setting up one handler call (the dispatch loop invokes us again
    /// once it returns). `__gc` handlers execute on the running thread, which
    /// matches PUC's "called from a C function" context.
    fn service_finalizers(
        &mut self,
        tid: ThreadId,
        th: &mut Thread,
        fuel: &mut i64,
    ) -> Result<(), VmError> {
        if let Some(depth) = self.finalizer_depth {
            if th.frames.len() < depth || self.current_thread != tid {
                self.finalizer_depth = None;
            } else {
                return Ok(());
            }
        }
        if th.frames.is_empty() {
            return Ok(());
        }
        while let Some(v) = self.pending_finalizers.pop() {
            let mm = self.raw_metafield(v, "__gc");
            if mm == Value::Nil {
                continue;
            }
            let scratch = scratch_base(th);
            ensure_len(&mut th.stack, scratch + 2);
            th.stack[scratch] = mm;
            th.stack[scratch + 1] = v;
            let discard = scratch + 2;
            ensure_len(&mut th.stack, discard + 2);
            let frames_before = th.frames.len();
            self.finalizer_running = true;
            let result = self.protected_call(th, fuel, scratch, 1, discard, 0, None, false);
            self.finalizer_running = false;
            result?;
            // A Lua-closure handler pushes a frame that is still running when
            // `protected_call` returns, so hold the depth guard until it
            // unwinds. A native handler (e.g. the file `__gc`) runs to
            // completion inside `protected_call` and leaves no frame; keeping
            // the guard set in that case would stall the driver forever and
            // leave `pending_finalizers` permanently rooted.
            self.finalizer_depth = if th.frames.len() > frames_before {
                Some(th.frames.len())
            } else {
                None
            };
            return Ok(());
        }
        Ok(())
    }

    /// Approximate live heap footprint in bytes.
    #[must_use]
    pub fn memory_used(&self) -> usize {
        let mut total = self.strings.bytes() + self.strings.live_count() * 40;
        for (i, t) in self.tables.iter().enumerate() {
            if self.tables_live.get(i).copied().unwrap_or(true) {
                total += t.mem_estimate();
            }
        }
        for (i, th) in self.threads.iter().enumerate() {
            if self.threads_live.get(i).copied().unwrap_or(true) {
                total += 128
                    + th.stack.capacity() * 16
                    + th.frames.len() * 192
                    + th.frames
                        .iter()
                        .map(|f| f.lua().map_or(0, |l| l.varargs.len() * 16))
                        .sum::<usize>();
            }
        }
        for (i, c) in self.closures.iter().enumerate() {
            if self.closures_live.get(i).copied().unwrap_or(true) {
                total += 48 + c.upvals.len() * 8;
            }
        }
        total += self.upvals.len() * 24;
        total += self.natives.len() * 56;
        for (i, u) in self.userdata.iter().enumerate() {
            if self.userdata_live.get(i).copied().unwrap_or(true) {
                total += 64 + u.read_buf.capacity();
            }
        }
        total
    }

    /// Auto-GC trigger from allocation sites inside the dispatch loop.
    /// `th` is the running thread (moved out of the arena), which must be
    /// traced as an extra root. Enforces the memory ceiling.
    fn maybe_gc(&mut self, tid: ThreadId, th: &Thread) -> Result<(), VmError> {
        let due = self.gc_running
            && self.gc_alloc_threshold != 0
            && (self.allocs_since_gc >= self.gc_alloc_threshold
                || self.strings.bytes() > self.str_bytes_at_gc + (8 << 20));
        if due {
            self.collect(Some((tid, th, 0)));
        }
        if let Some(limit) = self.memory_limit {
            // only re-measured at collection points; cheap proxy otherwise
            if due && self.memory_used() > limit {
                return Err(Self::rt_err(th, "not enough memory".into()));
            }
        }
        Ok(())
    }

    /// Implements `collectgarbage([opt [, arg]])`. See `n_collectgarbage`'s
    /// PUC-compatible option set; because the collector is a stop-the-world
    /// mark-sweep, "collect"/"step" run a full collection (with the running
    /// thread as an extra root).
    fn gc_command(
        &mut self,
        th: &Thread,
        opt: Value,
        arg1: Option<Value>,
        call_top: usize,
    ) -> Result<Vec<Value>, String> {
        let opt = match opt {
            Value::Str(s) => self.strings.get(s).to_vec(),
            Value::Nil => b"collect".to_vec(),
            // PUC's `luaL_checkoption` coerces numbers to strings before
            // matching, so `collectgarbage(5)` reports an invalid option
            // rather than a type error.
            Value::Int(_) | Value::Float(_) => self.display_value(opt).into_bytes(),
            v => {
                return Err(format!(
                    "bad argument #1 to 'collectgarbage' (string expected, got {})",
                    v.type_name()
                ));
            }
        };
        // PUC marks its GC state "running a collection" for the whole cycle,
        // including `__gc` handlers, and `lua_gc` reports every option as
        // invalid (-1) there; `luaB_collectgarbage` then returns a single nil
        // instead of the option's result. Option #1 was already validated
        // above; option #2 for the numeric options is coerced before `lua_gc`
        // sees the reentrancy, so mirror that too.
        let opt = String::from_utf8_lossy(&opt).into_owned();
        let int_arg = |v: Option<Value>| -> Result<i64, String> {
            match v {
                None | Some(Value::Nil) => Ok(0),
                Some(Value::Int(i)) => Ok(i),
                Some(Value::Float(f)) => Ok(f as i64),
                Some(v) => Err(format!(
                    "bad argument #2 to 'collectgarbage' (number expected, got {})",
                    v.type_name()
                )),
            }
        };
        if self.finalizer_depth.is_some() {
            if matches!(opt.as_str(), "step" | "setpause" | "setstepmul") {
                let _ = int_arg(arg1)?;
            }
            return Ok(vec![Value::Nil]);
        }
        let collect_now = |me: &mut Self| me.collect(Some((me.current_thread, th, call_top)));
        match opt.as_str() {
            "collect" => {
                collect_now(self);
                Ok(vec![Value::Int(0)])
            }
            "step" => {
                // The collector is a stop-the-world mark-sweep with no
                // resumable phases, so one "step" is a bounded full
                // collection: a cycle always completes, and PUC's contract
                // ("true if the step finished a collection cycle") is
                // therefore always satisfied. `n` is validated like PUC but
                // cannot select a partial amount of work; see DESIGN.md.
                let _ = int_arg(arg1)?;
                collect_now(self);
                Ok(vec![Value::Bool(true)])
            }
            "stop" => {
                self.gc_running = false;
                Ok(vec![Value::Int(0)])
            }
            "restart" => {
                self.gc_running = true;
                Ok(vec![Value::Int(0)])
            }
            "count" => Ok(vec![Value::Float(self.memory_used() as f64 / 1024.0)]),
            "isrunning" => Ok(vec![Value::Bool(self.gc_running)]),
            "incremental" => {
                let prev = self.gc_mode == 0;
                self.gc_mode = 0;
                Ok(vec![self.new_string(if prev {
                    b"incremental"
                } else {
                    b"generational"
                })])
            }
            "generational" => {
                let prev = self.gc_mode == 1;
                self.gc_mode = 1;
                Ok(vec![self.new_string(if prev {
                    b"generational"
                } else {
                    b"incremental"
                })])
            }
            "setpause" => {
                let v = int_arg(arg1)?;
                let prev = self.gc_pause;
                self.gc_pause = v;
                Ok(vec![Value::Int(prev)])
            }
            "setstepmul" => {
                let v = int_arg(arg1)?;
                let prev = self.gc_stepmul;
                self.gc_stepmul = v;
                Ok(vec![Value::Int(prev)])
            }
            other => Err(format!(
                "bad argument #1 to 'collectgarbage' (invalid option '{other}')"
            )),
        }
    }

    fn collect(&mut self, extra: Option<(ThreadId, &Thread, usize)>) {
        let table_count = self.tables.len();
        let mut m = Marks {
            strings: vec![false; self.strings.len()],
            tables: vec![false; table_count],
            closures: vec![false; self.closures.len()],
            natives: vec![false; self.natives.len()],
            upvals: vec![false; self.upvals.len()],
            threads: vec![false; self.threads.len()],
            userdata: vec![false; self.userdata.len()],
        };
        let weak = self.weak_kinds(table_count);
        let gc_name = self.strings.lookup(b"__gc");
        let mut work: Vec<Value> = Vec::with_capacity(64);
        let mut ephemerons: Vec<usize> = Vec::new();
        // roots
        work.push(Value::Table(self.globals));
        if let Some(sm) = self.string_meta {
            work.push(Value::Table(sm));
        }
        for mt in self.type_metas.iter().flatten() {
            work.push(Value::Table(*mt));
        }
        if let Some(mt) = self.file_meta {
            work.push(Value::Table(mt));
        }
        if let Some(u) = self.io_input {
            work.push(Value::Userdata(u));
        }
        if let Some(u) = self.io_output {
            work.push(Value::Userdata(u));
        }
        work.extend_from_slice(&self.anchors);
        work.extend_from_slice(&self.pending_finalizers);
        if let Some(job) = &self.close_job {
            work.push(Value::Thread(job.target));
            work.extend_from_slice(&job.items);
            work.push(job.err);
        }
        if let Some(job) = &self.print_job {
            work.extend_from_slice(&job.items);
        }
        if let Some(job) = &self.format_job {
            work.extend_from_slice(&job.args);
            if let Some((_, v, _)) = &job.saved {
                work.push(*v);
            }
        }
        for (&root, &cur) in &self.exec_roots {
            work.push(Value::Thread(ThreadId(root)));
            work.push(Value::Thread(ThreadId(cur)));
        }
        if let Some((etid, eth, extra_top)) = extra {
            m.threads[etid.0 as usize] = true;
            Self::trace_thread(eth, extra_top, &mut m, &mut work, &self.upvals);
        }
        self.mark_loop(&mut m, &mut work, &weak, &mut ephemerons);
        self.ephemeron_fixpoint(&mut m, &mut work, &weak, &mut ephemerons);
        // Finalization: resurrect unreachable objects carrying a `__gc`,
        // keeping them (and everything they reach) alive for one more cycle
        // while the handler is queued. Repeat until nothing new surfaces.
        if let Some(gc_name) = gc_name {
            loop {
                let mut found = false;
                for i in 0..table_count {
                    if !self.tables_live[i] || m.tables[i] || self.tables[i].finalized {
                        continue;
                    }
                    let has_gc = match self.tables[i].metatable {
                        Some(mt) => {
                            self.tables[mt.0 as usize].get(Value::Str(gc_name)) != Value::Nil
                        }
                        None => false,
                    };
                    if has_gc {
                        // Do NOT pre-mark here: `mark_loop` must see the
                        // table unmarked so it also traces its metatable
                        // (which holds `__gc`) and its whole reachable graph.
                        self.tables[i].finalized = true;
                        work.push(Value::Table(TableId(i as u32)));
                        self.pending_finalizers
                            .push(Value::Table(TableId(i as u32)));
                        found = true;
                    }
                }
                // Userdata carrying `__gc` finalize the same way.
                for i in 0..self.userdata.len() {
                    if !self.userdata_live[i] || m.userdata[i] || self.userdata[i].finalized {
                        continue;
                    }
                    let has_gc = match self.userdata[i].metatable {
                        Some(mt) => {
                            self.tables[mt.0 as usize].get(Value::Str(gc_name)) != Value::Nil
                        }
                        None => false,
                    };
                    if has_gc {
                        self.userdata[i].finalized = true;
                        let v = Value::Userdata(UserdataId(i as u32));
                        work.push(v);
                        self.pending_finalizers.push(v);
                        found = true;
                    }
                }
                if !found {
                    break;
                }
                self.mark_loop(&mut m, &mut work, &weak, &mut ephemerons);
                self.ephemeron_fixpoint(&mut m, &mut work, &weak, &mut ephemerons);
            }
        }
        self.clear_weak(&mut m, &weak);
        self.sweep(&m);
        self.allocs_since_gc = 0;
        self.str_bytes_at_gc = self.strings.bytes();
    }

    /// Weakness of every table index, derived from each metatable's `__mode`.
    fn weak_kinds(&self, table_count: usize) -> Vec<WeakKind> {
        let mut weak = vec![WeakKind::Strong; table_count];
        let Some(mode_key) = self.strings.lookup(b"__mode") else {
            return weak;
        };
        for (i, w) in weak.iter_mut().enumerate() {
            if !self.tables_live.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(mt) = self.tables[i].metatable else {
                continue;
            };
            let Value::Str(mode) = self.tables[mt.0 as usize].get(Value::Str(mode_key)) else {
                continue;
            };
            let bytes = self.strings.get(mode);
            let k = bytes.contains(&b'k');
            let v = bytes.contains(&b'v');
            *w = match (k, v) {
                (true, true) => WeakKind::Both,
                (true, false) => WeakKind::Keys,
                (false, true) => WeakKind::Values,
                (false, false) => WeakKind::Strong,
            };
        }
        weak
    }

    /// Drains the mark worklist, honouring weak tables: strong tables are
    /// traced fully, weak-value tables trace only their keys, weak-key tables
    /// become ephemerons, weak-both trace nothing here.
    fn mark_loop(
        &self,
        m: &mut Marks,
        work: &mut Vec<Value>,
        weak: &[WeakKind],
        ephemerons: &mut Vec<usize>,
    ) {
        while let Some(v) = work.pop() {
            match v {
                Value::Str(s) => {
                    if let Some(slot) = m.strings.get_mut(s.obj.0 as usize) {
                        *slot = true;
                    }
                    if let Some(slot) = m.strings.get_mut(s.content.0 as usize) {
                        *slot = true;
                    }
                }
                Value::Table(t) => {
                    let i = t.0 as usize;
                    if m.tables[i] {
                        continue;
                    }
                    m.tables[i] = true;
                    if let Some(mt) = self.tables[i].metatable {
                        work.push(Value::Table(mt));
                    }
                    match weak[i] {
                        WeakKind::Strong => {
                            self.tables[i].trace(|v| work.push(v));
                        }
                        WeakKind::Values => {
                            for (k, _) in self.tables[i].entries() {
                                work.push(k);
                            }
                        }
                        WeakKind::Keys => ephemerons.push(i),
                        WeakKind::Both => {}
                    }
                }
                Value::Closure(c) => {
                    let i = c.0 as usize;
                    if !m.closures[i] {
                        m.closures[i] = true;
                        for &uid in &self.closures[i].upvals {
                            mark_upval(uid, m, work, &self.upvals);
                        }
                    }
                }
                Value::Native(n) => {
                    let i = n.0 as usize;
                    if !m.natives[i] {
                        m.natives[i] = true;
                        if let NativeKind::Intrinsic(Intrinsic::WrapResume(t)) =
                            self.natives[i].kind
                        {
                            work.push(Value::Thread(t));
                        }
                    }
                }
                Value::Thread(t) => {
                    let i = t.0 as usize;
                    if !m.threads[i] {
                        m.threads[i] = true;
                        Self::trace_thread(&self.threads[i], 0, m, work, &self.upvals);
                    }
                }
                Value::Userdata(u) => {
                    let i = u.0 as usize;
                    if !m.userdata[i] {
                        m.userdata[i] = true;
                        if let Some(mt) = self.userdata[i].metatable {
                            work.push(Value::Table(mt));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Ephemeron fixpoint: for weak-key/strong-value tables a value is only
    /// reachable through its key, so iterate until no new value is marked.
    fn ephemeron_fixpoint(
        &self,
        m: &mut Marks,
        work: &mut Vec<Value>,
        weak: &[WeakKind],
        ephemerons: &mut Vec<usize>,
    ) {
        loop {
            let mut changed = false;
            let mut i = 0;
            while i < ephemerons.len() {
                let ti = ephemerons[i];
                for (k, v) in self.tables[ti].entries() {
                    if !Self::value_marked(m, k) {
                        continue;
                    }
                    if !Self::value_marked(m, v) {
                        work.push(v);
                        changed = true;
                    }
                }
                i += 1;
            }
            if !changed {
                break;
            }
            self.mark_loop(m, work, weak, ephemerons);
        }
    }

    /// True when `v` is alive for weak-reference purposes: non-collectable
    /// values and strings always are (PUC never collects a string through a
    /// weak table); everything else must be marked.
    fn value_marked(m: &Marks, v: Value) -> bool {
        match v {
            Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_) => true,
            Value::Table(t) => m.tables[t.0 as usize],
            Value::Closure(c) => m.closures[c.0 as usize],
            Value::Native(n) => m.natives[n.0 as usize],
            Value::Thread(t) => m.threads[t.0 as usize],
            Value::Userdata(u) => m.userdata[u.0 as usize],
        }
    }

    fn weak_dead(m: &Marks, v: Value) -> bool {
        matches!(
            v,
            Value::Table(_)
                | Value::Closure(_)
                | Value::Native(_)
                | Value::Thread(_)
                | Value::Userdata(_)
        ) && !Self::value_marked(m, v)
    }

    /// Removes entries whose weak component died, and marks string keys/values
    /// of surviving entries so they are not swept while still referenced.
    fn clear_weak(&mut self, m: &mut Marks, weak: &[WeakKind]) {
        for (i, &kind) in weak.iter().enumerate().take(self.tables.len()) {
            if kind == WeakKind::Strong || !m.tables[i] {
                continue;
            }
            let keys_weak = matches!(kind, WeakKind::Keys | WeakKind::Both);
            let values_weak = matches!(kind, WeakKind::Values | WeakKind::Both);
            for (k, v) in self.tables[i].entries() {
                let key_dead = keys_weak && Self::weak_dead(m, k);
                let val_dead = values_weak && Self::weak_dead(m, v);
                if key_dead || val_dead {
                    self.tables[i].remove(k);
                } else {
                    if let Value::Str(s) = k {
                        m.strings[s.obj.0 as usize] = true;
                        m.strings[s.content.0 as usize] = true;
                    }
                    if let Value::Str(s) = v {
                        m.strings[s.obj.0 as usize] = true;
                        m.strings[s.content.0 as usize] = true;
                    }
                }
            }
        }
    }

    fn trace_thread(
        th: &Thread,
        extra_top: usize,
        m: &mut Marks,
        work: &mut Vec<Value>,
        upvals: &[Upval],
    ) {
        let stack_len = th.stack.len();
        if th.frames.is_empty() {
            // e.g. a created-but-never-resumed coroutine: the body lives at
            // stack[0] with no frame to bound the window, and a finished
            // coroutine may leave results behind.
            let top = th.top.max(extra_top).min(stack_len);
            work.extend_from_slice(&th.stack[..top]);
        } else {
            // Mark each frame's live register window only. Slots above the
            // current instruction's extent are dead temporaries, and gaps left
            // by popped/tail-replaced frames stay unmarked.
            for f in &th.frames {
                match f {
                    Frame::Lua(lf) => {
                        let end = (lf.base + frame_reg_extent(lf)).min(stack_len);
                        if lf.base < end {
                            work.extend_from_slice(&th.stack[lf.base..end]);
                        }
                        work.push(Value::Closure(lf.closure));
                        if let Some(h) = lf.handler {
                            work.push(h);
                        }
                        for &v in &lf.varargs {
                            work.push(v);
                        }
                        mark_pending(&lf.pending, th, work, stack_len);
                    }
                    Frame::Boundary(cf) => {
                        if let Some(b) = &cf.boundary
                            && let Some(h) = b.handler
                        {
                            work.push(h);
                        }
                        mark_pending(&cf.pending, th, work, stack_len);
                    }
                }
            }
            // A native/intrinsic call that triggered the collection may hold
            // open multret arguments above the top frame's window.
            if extra_top > 0 {
                let start = th.frames.last().and_then(|f| f.lua()).map_or(0, |f| f.base);
                let end = extra_top.min(stack_len);
                if start < end {
                    work.extend_from_slice(&th.stack[start..end]);
                }
            }
        }
        for &(_, uid) in &th.open_upvals {
            mark_upval(uid, m, work, upvals);
        }
        if let Some(e) = th.close_error {
            work.push(e);
        }
        if let Some(h) = th.hook {
            work.push(h);
        }
        if let Some(job) = &th.yield_job {
            work.push(job.func);
            for &v in &job.args {
                work.push(v);
            }
        }
        if let Some(pending) = &th.pending_native {
            work.push(pending.func);
            if let Some(Ok(values)) = &pending.completion {
                work.extend_from_slice(values);
            }
        }
        if let Some(p) = th.parent {
            work.push(Value::Thread(p));
        }
    }

    fn sweep(&mut self, m: &Marks) {
        // dying threads may have open upvalues that survive through
        // closures: close them (copy the values out) first
        for i in 0..self.threads.len() {
            if m.threads[i] || !self.threads_live[i] {
                continue;
            }
            let ou = std::mem::take(&mut self.threads[i].open_upvals);
            for (idx, uid) in ou {
                if m.upvals[uid.0 as usize] {
                    let v = self.threads[i]
                        .stack
                        .get(idx)
                        .copied()
                        .unwrap_or(Value::Nil);
                    self.upvals[uid.0 as usize] = Upval::Closed(v);
                }
            }
        }
        for i in 0..self.tables.len() {
            if !m.tables[i] && self.tables_live[i] {
                self.tables_live[i] = false;
                self.tables_free.push(i as u32);
                self.tables[i] = Table::default();
            }
        }
        for i in 0..self.closures.len() {
            if !m.closures[i] && self.closures_live[i] {
                self.closures_live[i] = false;
                self.closures_free.push(i as u32);
                self.closures[i] = LuaClosure {
                    proto: self.empty_proto.clone(),
                    upvals: Vec::new(),
                };
            }
        }
        for i in 0..self.upvals.len() {
            if !m.upvals[i] && self.upvals_live[i] {
                self.upvals_live[i] = false;
                self.upvals_free.push(i as u32);
                self.upvals[i] = Upval::Closed(Value::Nil);
            }
        }
        for i in 0..self.threads.len() {
            if !m.threads[i] && self.threads_live[i] {
                self.threads_live[i] = false;
                self.threads_free.push(i as u32);
                self.threads[i] = Thread::default();
            }
        }
        for i in 0..self.natives.len() {
            if !m.natives[i] && self.natives_live[i] {
                self.natives_live[i] = false;
                self.natives_free.push(i as u32);
                self.natives[i] = Native {
                    name: String::new(),
                    kind: NativeKind::Plain(n_dead),
                };
            }
        }
        // Dead userdata: release any host resource it still owns.
        for i in 0..self.userdata.len() {
            if !m.userdata[i] && self.userdata_live[i] {
                let ud = std::mem::take(&mut self.userdata[i]);
                self.userdata_live[i] = false;
                self.userdata_free.push(i as u32);
                if !ud.closed
                    && let HostObject::File(h) = ud.object
                    && let Some(host) = self.host.as_mut()
                {
                    let _ = host.close(h);
                }
            }
        }
        self.strings.sweep(&m.strings);
    }
}

impl<C> Execution<C> {
    /// Runs the script for at most `fuel` units of work (roughly one unit
    /// per VM instruction, with surcharges for calls and allocations).
    /// Returns `Step::Pending` if the budget ran out — call again to resume.
    ///
    /// # Errors
    ///
    /// Returns an error if this execution already finished or a runtime error
    /// escapes the script.
    #[expect(clippy::missing_panics_doc)]
    pub fn step(&mut self, lua: &mut Lua<C>, fuel: u64) -> Result<Step, Error> {
        if self.finished {
            return Err(Error::Runtime(RuntimeError {
                message: "execution already finished".into(),
                value: Value::Nil,
                line: 0,
                root_line: 0,
            }));
        }
        let budget = fuel.min(i64::MAX as u64) as i64;
        let mut remaining = budget - self.debt;
        if remaining <= 0 {
            self.debt -= budget;
            return Ok(Step::Pending);
        }
        lua.current_execution = Some(self.id);
        lua.active_context = Some(self.context.take().expect("execution context missing"));
        let outcome = lua.run(self.current, &mut remaining);
        lua.current_execution = None;
        self.context = lua.active_context.take();
        match outcome {
            Ok(RunOutcome::Done(vals)) => {
                self.finished = true;
                lua.exec_roots.remove(&self.thread.0);
                Ok(Step::Done(vals))
            }
            Ok(RunOutcome::Pending(current)) => {
                self.current = current;
                lua.exec_roots.insert(self.thread.0, current.0);
                self.debt = (-remaining).max(0);
                Ok(Step::Pending)
            }
            Ok(RunOutcome::Waiting(current, wait)) => {
                self.current = current;
                lua.exec_roots.insert(self.thread.0, current.0);
                self.debt = (-remaining).max(0);
                Ok(Step::Waiting(wait))
            }
            Err(e) => {
                self.finished = true;
                lua.exec_roots.remove(&self.thread.0);
                Err(Error::Runtime(e))
            }
        }
    }

    /// Abandons a suspended execution, releasing its GC roots. Without
    /// this (or running to completion), the execution's threads stay
    /// rooted for the lifetime of the `Lua`.
    pub fn abort(mut self, lua: &mut Lua<C>) {
        self.finished = true;
        lua.exec_roots.remove(&self.thread.0);
    }

    #[must_use]
    pub fn id(&self) -> ExecutionId {
        self.id
    }

    #[expect(clippy::missing_panics_doc)]
    pub fn context(&self) -> &C {
        self.context.as_ref().expect("execution context missing")
    }

    #[expect(clippy::missing_panics_doc)]
    pub fn context_mut(&mut self) -> &mut C {
        self.context.as_mut().expect("execution context missing")
    }

    /// Supplies the result of the native calls awaiting `wait`.
    ///
    /// The wait may be parked on any thread of this execution — the root
    /// thread (whose wait blocked the execution and surfaced as
    /// `Step::Waiting`) or one or more suspended coroutines (which parked only
    /// themselves). Every wait parked in this execution under `wait` is
    /// completed; use [`Execution::pending_waits`] to discover outstanding
    /// tokens. The completion is delivered when each parked coroutine is next
    /// resumed.
    ///
    /// # Errors
    ///
    /// Returns an error for a finished execution, a token this execution is
    /// not waiting on, or a token whose calls were already completed.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "public API takes the result by value; it is cloned per parked wait"
    )]
    pub fn complete_native(
        &mut self,
        lua: &mut Lua<C>,
        wait: NativeWait,
        result: Result<Vec<Value>, String>,
    ) -> Result<(), String> {
        if self.finished {
            return Err("execution already finished".into());
        }
        let mut matching = false;
        let mut open = Vec::new();
        for (i, th) in lua.threads.iter().enumerate() {
            if let Some(pending) = &th.pending_native
                && pending.exec == self.id
                && pending.wait == wait
            {
                matching = true;
                if pending.completion.is_none() {
                    open.push(i);
                }
            }
        }
        if open.is_empty() {
            return Err(if matching {
                "native call was already completed".into()
            } else {
                "execution is not waiting for this native call".into()
            });
        }
        for &i in &open {
            if let Some(pending) = lua.threads[i].pending_native.as_mut() {
                pending.completion = Some(result.clone());
            }
        }
        Ok(())
    }

    /// Wait tokens currently awaiting completion in this execution, in thread
    /// order and without duplicates. A token belongs to the root thread
    /// (whole-execution block, also reported as `Step::Waiting`), one or more
    /// suspended coroutines, or both. Tokens whose completion the host has
    /// already supplied are omitted: they resume on the next `step`, they do
    /// not need completing again.
    ///
    /// A finished execution reports none, even for a coroutine parked under it
    /// that is still reachable from Lua: adopt such a wait by resuming the
    /// coroutine from a live execution first (its next park transfers the wait
    /// to that execution). A parked coroutine is only tracked while it is
    /// reachable (from Lua, or as the execution's current thread); an embedder
    /// that parks a coroutine and drops every reference to it cannot complete
    /// that wait.
    #[must_use]
    pub fn pending_waits(&self, lua: &Lua<C>) -> Vec<NativeWait> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        for th in &lua.threads {
            if let Some(pending) = &th.pending_native
                && pending.exec == self.id
                && pending.completion.is_none()
                && !out.contains(&pending.wait)
            {
                out.push(pending.wait);
            }
        }
        out
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// `(chunk name, source line)` of the instruction the execution would
    /// run next, for debuggers and tracers. `None` once it has finished.
    #[must_use]
    pub fn current_location(&self, lua: &Lua<C>) -> Option<(String, u32)> {
        let th = lua.threads.get(self.current.0 as usize)?;
        let f = th.frames.iter().rev().find_map(|f| f.lua())?;
        let line = f.proto.lines.get(f.pc).copied().unwrap_or(0);
        Some((f.proto.source.to_string(), line))
    }
}
