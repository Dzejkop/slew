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
use crate::compiler::{compile, CompileError};
use crate::parser::{parse, ParseError};
use crate::value::{
    fmt_number, float_to_exact_int, ClosId, NativeId, StrId, Strings, Table, TableId, ThreadId,
    UpvalId, Value,
};
use std::fmt;
use std::rc::Rc;

/// Default cap on call-frame depth; a deliberately bounded execution profile
/// knob (recursion consumes heap, not the host stack).
const MAX_CALL_DEPTH: usize = 10_000;
/// Bound on `__index`/`__newindex`/`__call` metamethod chains.
const MAX_META_CHAIN: usize = 100;

#[derive(Debug)]
pub enum Error {
    Parse(ParseError),
    Compile(CompileError),
    Runtime(RuntimeError),
}

#[derive(Debug, Clone)]
pub struct RuntimeError {
    /// Rendered error message (position-prefixed for VM-raised errors).
    pub message: String,
    /// The Lua error value (`error()` can raise any value).
    pub value: Value,
    pub line: u32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "{e}"),
            Error::Compile(e) => write!(f, "{e}"),
            Error::Runtime(e) => write!(f, "runtime error: {}", e.message),
        }
    }
}

impl std::error::Error for Error {}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Self {
        Error::Parse(e)
    }
}

impl From<CompileError> for Error {
    fn from(e: CompileError) -> Self {
        Error::Compile(e)
    }
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
    /// Chunk name for the position prefix; `None` for native errors,
    /// which carry no position (matching PUC).
    pub source: Option<Rc<str>>,
}

pub type NativeFn = fn(&mut Lua, &[Value]) -> Result<Vec<Value>, String>;

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
    IsYieldable,
    Running,
}

pub(crate) enum NativeKind {
    Plain(NativeFn),
    Intrinsic(Intrinsic),
}

pub(crate) struct Native {
    pub name: String,
    pub kind: NativeKind,
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
    DeliverError { ret_to: usize, nres: u8, err: Value, handler: Option<Value> },
    /// Final step of a return that had to run `__close` handlers first.
    FinishReturn { start: usize, count: usize },
}

struct Frame {
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
    pending: Vec<Pending>,
    /// Registers holding active to-be-closed variables (ascending).
    tbc: Vec<u8>,
    varargs: Vec<Value>,
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
    /// The thread that resumed this one.
    parent: Option<ThreadId>,
    /// Result-delivery info in the parent (set at each resume).
    resume_ret: Option<ResumeRet>,
    /// Where the next resume's arguments land (set at each yield).
    yield_ret: Option<(usize, u8, RetShape)>,
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
}

/// A suspendable run of a chunk. Created by [`Lua::execute`]; all VM state
/// lives in the `Lua`, this is a handle plus fuel-debt bookkeeping.
pub struct Execution {
    /// Root thread of this execution (a GC root while the execution lives).
    #[allow(dead_code)]
    thread: ThreadId,
    /// Thread to resume on the next step (a coroutine may have been
    /// running when fuel ran out).
    current: ThreadId,
    /// Fuel overdrawn by the last step (surcharges can overshoot), repaid
    /// from the next budget.
    debt: i64,
    finished: bool,
}

/// Metamethod identifiers; indexes into `Lua::mm_names`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub(crate) enum Mm {
    Index,
    NewIndex,
    Call,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Unm,
    IDiv,
    BAnd,
    BOr,
    BXor,
    BNot,
    Shl,
    Shr,
    Concat,
    Len,
    Eq,
    Lt,
    Le,
    ToString,
    #[allow(dead_code)] // looked up by name in setmetatable/getmetatable
    Metatable,
    #[allow(dead_code)] // used by to-be-closed variables (M3)
    Close,
}

const MM_NAMES: [&str; 25] = [
    "__index",
    "__newindex",
    "__call",
    "__add",
    "__sub",
    "__mul",
    "__div",
    "__mod",
    "__pow",
    "__unm",
    "__idiv",
    "__band",
    "__bor",
    "__bxor",
    "__bnot",
    "__shl",
    "__shr",
    "__concat",
    "__len",
    "__eq",
    "__lt",
    "__le",
    "__tostring",
    "__metatable",
    "__close",
];

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

pub struct Lua {
    pub(crate) strings: Strings,
    pub(crate) tables: Vec<Table>,
    pub(crate) closures: Vec<LuaClosure>,
    pub(crate) natives: Vec<Native>,
    pub(crate) upvals: Vec<Upval>,
    pub(crate) threads: Vec<Thread>,
    pub(crate) globals: TableId,
    pub(crate) builtin_next: Value,
    pub(crate) builtin_ipairs_iter: Value,
    pub(crate) string_meta: Option<TableId>,
    mm_names: Vec<StrId>,
    /// Thread being dispatched right now (its `Thread` is temporarily
    /// moved out of the arena).
    pub(crate) current_thread: ThreadId,
    /// Set by resume/yield intrinsics; the dispatch loop performs the
    /// actual thread switch.
    switch_to: Option<ThreadId>,
    /// math.random state (xoshiro256**); deterministically seeded so
    /// scripts behave identically run-to-run unless reseeded.
    rng: [u64; 4],
}

impl Default for Lua {
    fn default() -> Self {
        Self::new()
    }
}

impl Lua {
    pub fn new() -> Self {
        let mut strings = Strings::default();
        let mm_names = MM_NAMES.iter().map(|n| strings.intern(n.as_bytes())).collect();
        let mut lua = Lua {
            strings,
            tables: vec![Table::default()],
            closures: Vec::new(),
            natives: Vec::new(),
            upvals: Vec::new(),
            threads: Vec::new(),
            globals: TableId(0),
            builtin_next: Value::Nil,
            builtin_ipairs_iter: Value::Nil,
            string_meta: None,
            mm_names,
            current_thread: ThreadId(u32::MAX),
            switch_to: None,
            rng: [0; 4],
        };
        lua.seed_random(0x5375734c75615f31); // "SusLua_1"
        crate::stdlib::install(&mut lua);
        lua
    }

    pub fn seed_random(&mut self, seed: u64) {
        // splitmix64 to expand the seed into the xoshiro state
        let mut x = seed;
        for s in &mut self.rng {
            x = x.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            *s = z ^ (z >> 31);
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
    pub fn load(&mut self, src: impl AsRef<[u8]>) -> Result<Chunk, Error> {
        self.load_named("chunk", src)
    }

    /// Like [`Lua::load`] with an explicit chunk name for error messages.
    pub fn load_named(&mut self, name: &str, src: impl AsRef<[u8]>) -> Result<Chunk, Error> {
        let block = parse(src.as_ref())?;
        let proto = compile(&block, &mut self.strings, name)?;
        Ok(Chunk { proto })
    }

    /// Instantiates a chunk as a suspended execution. Nothing runs until
    /// [`Execution::step`] is called.
    pub fn execute(&mut self, chunk: &Chunk) -> Execution {
        let env = self.new_upval(Upval::Closed(Value::Table(self.globals)));
        let cid = ClosId(self.closures.len() as u32);
        self.closures.push(LuaClosure { proto: chunk.proto.clone(), upvals: vec![env] });
        let mut th = Thread::default();
        th.stack.resize(chunk.proto.max_regs as usize, Value::Nil);
        th.frames.push(Frame {
            closure: cid,
            proto: chunk.proto.clone(),
            pc: 0,
            base: 0,
            ret_to: 0,
            nres: 0,
            shape: RetShape::Normal,
            protected: false,
            handler: None,
            pending: Vec::new(),
            tbc: Vec::new(),
            varargs: Vec::new(),
        });
        let tid = ThreadId(self.threads.len() as u32);
        self.threads.push(th);
        Execution { thread: tid, current: tid, debt: 0, finished: false }
    }

    /// Registers a native function as a global.
    pub fn register_native(&mut self, name: &str, f: NativeFn) -> Value {
        let v = self.add_native(name, f);
        let k = self.new_string(name.as_bytes());
        self.tables[self.globals.0 as usize].set(k, v).unwrap();
        v
    }

    /// Adds a native function without binding it to a global.
    pub fn add_native(&mut self, name: &str, f: NativeFn) -> Value {
        self.add_native_kind(name, NativeKind::Plain(f))
    }

    pub(crate) fn add_native_kind(&mut self, name: &str, kind: NativeKind) -> Value {
        let id = NativeId(self.natives.len() as u32);
        self.natives.push(Native { name: name.into(), kind });
        Value::Native(id)
    }

    pub(crate) fn register_intrinsic(&mut self, name: &str, i: Intrinsic) -> Value {
        let v = self.add_native_kind(name, NativeKind::Intrinsic(i));
        let k = self.new_string(name.as_bytes());
        self.tables[self.globals.0 as usize].set(k, v).unwrap();
        v
    }

    pub fn new_string(&mut self, s: &[u8]) -> Value {
        Value::Str(self.strings.intern(s))
    }

    pub fn new_table(&mut self) -> Value {
        let id = TableId(self.tables.len() as u32);
        self.tables.push(Table::default());
        Value::Table(id)
    }

    /// Creates a coroutine from a function value (for `coroutine.create`).
    pub(crate) fn create_coroutine(&mut self, f: Value) -> Value {
        let mut th = Thread::default();
        th.stack.push(f); // consumed on first resume
        th.status = CoStatus::Start;
        let tid = ThreadId(self.threads.len() as u32);
        self.threads.push(th);
        Value::Thread(tid)
    }

    fn new_upval(&mut self, u: Upval) -> UpvalId {
        let id = UpvalId(self.upvals.len() as u32);
        self.upvals.push(u);
        id
    }

    pub fn get_global(&self, name: &str) -> Value {
        match self.strings.lookup(name.as_bytes()) {
            Some(id) => self.tables[self.globals.0 as usize].get(Value::Str(id)),
            None => Value::Nil, // a name never interned can't be a set global
        }
    }

    pub fn set_global(&mut self, name: &str, v: Value) {
        let k = self.new_string(name.as_bytes());
        self.tables[self.globals.0 as usize].set(k, v).unwrap();
    }

    /// Raw table read (no metamethods).
    pub fn table_get(&self, t: Value, k: Value) -> Value {
        match t {
            Value::Table(id) => self.tables[id.0 as usize].get(k),
            _ => Value::Nil,
        }
    }

    pub fn str_bytes(&self, v: Value) -> Option<&[u8]> {
        match v {
            Value::Str(id) => Some(self.strings.get(id)),
            _ => None,
        }
    }

    /// Human-readable rendering of a value (like raw `tostring`, lossy for
    /// non-UTF-8 strings; does not invoke `__tostring`).
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
        }
    }

    // ---- metatables ----

    pub fn get_metatable(&self, v: Value) -> Option<TableId> {
        match v {
            Value::Table(t) => self.tables[t.0 as usize].metatable,
            Value::Str(_) => self.string_meta,
            _ => None,
        }
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
            Some(mt) => {
                self.tables[mt.0 as usize].get(Value::Str(self.mm_names[mm as usize]))
            }
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
                Err(mut e) => loop {
                    // error escaped this thread entirely
                    th.frames.clear();
                    th.open_upvals.clear();
                    th.status = CoStatus::Dead;
                    let parent = th.parent.take();
                    let rr = th.resume_ret.take();
                    th.stack.clear();
                    self.threads[cur.0 as usize] = th;
                    match parent {
                        None => return Err(self.materialize_error(e)),
                        Some(p) => {
                            cur = p;
                            self.current_thread = cur;
                            th = std::mem::take(&mut self.threads[cur.0 as usize]);
                            th.status = CoStatus::Running;
                            let rr = rr.unwrap();
                            if rr.wrap {
                                // wrap propagates the error into the resumer
                                match self.recover(&mut th, e) {
                                    Ok(()) => break,
                                    Err(e2) => {
                                        e = e2;
                                        continue;
                                    }
                                }
                            } else {
                                let errv = self.err_value(&e);
                                deliver_resume(&mut th, rr, false, &[errv]);
                                break;
                            }
                        }
                    }
                },
            }
        }
    }

    fn materialize_error(&mut self, e: VmError) -> RuntimeError {
        let value = self.err_value(&e);
        RuntimeError { message: self.display_value(value), value, line: e.line }
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
            if *fuel <= 0 {
                return Ok(DispatchEnd::Pending);
            }
            match self.exec_one(tid, th, fuel) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Finished(vals)) => return Ok(DispatchEnd::Finished(vals)),
                Err(e) => self.recover(th, e)?,
            }
            if let Some(next) = self.switch_to.take() {
                return Ok(DispatchEnd::Switch(next));
            }
        }
    }

    /// Unwinds to the nearest protected frame; re-raises if none exists.
    /// Recovery is staged as pendings on the frame below the protection
    /// boundary: first any `__close` handlers of unwound to-be-closed
    /// variables (with the error object), then error delivery (directly or
    /// via the xpcall handler).
    fn recover(&mut self, th: &mut Thread, e: VmError) -> Result<(), VmError> {
        // values of to-be-closed variables in unwound frames, innermost first
        let mut to_close: Vec<Value> = Vec::new();
        loop {
            match th.frames.last() {
                None => return Err(e),
                Some(f) if f.protected => break,
                Some(_) => {
                    let f = th.frames.pop().unwrap();
                    for &r in f.tbc.iter().rev() {
                        to_close.push(th.stack[f.base + r as usize]);
                    }
                    self.close_upvals(th, f.base);
                }
            }
        }
        let pf = th.frames.pop().unwrap();
        for &r in pf.tbc.iter().rev() {
            to_close.push(th.stack[pf.base + r as usize]);
        }
        self.close_upvals(th, pf.base);
        let errv = self.err_value(&e);
        let Some(below) = th.frames.last_mut() else {
            // a protected root frame shouldn't exist (pcall always pushes
            // below an existing frame), but fail safe
            return Err(e);
        };
        below.pending.push(Pending::DeliverError {
            ret_to: pf.ret_to,
            nres: pf.nres,
            err: errv,
            handler: pf.handler,
        });
        // outermost closes are pushed first so the innermost pops first
        for v in to_close.into_iter().rev() {
            below.pending.push(Pending::CallClose { v, err: errv });
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
        // run continuations (e.g. a concat interrupted by a metamethod call,
        // staged __close handlers) before fetching the next instruction
        if !th.frames.last().unwrap().pending.is_empty() {
            let p = th.frames.last_mut().unwrap().pending.pop().unwrap();
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
                Pending::DeliverError { ret_to, nres, err, handler } => match handler {
                    None => {
                        place_results(th, ret_to, nres, &[Value::Bool(false), err]);
                    }
                    Some(h) => {
                        self.call_value(th, h, &[err], ret_to, nres, RetShape::PrependFalse, fuel)?;
                    }
                },
                Pending::FinishReturn { start, count } => {
                    let frame = th.frames.pop().unwrap();
                    self.close_upvals(th, frame.base);
                    if th.frames.is_empty() {
                        let vals = th.stack[start..start + count].to_vec();
                        th.stack.clear();
                        th.top = 0;
                        return Ok(Flow::Finished(vals));
                    }
                    deliver_return(th, &frame, start, count);
                }
            }
            return Ok(Flow::Continue);
        }
        let (instr, base) = {
            let f = th.frames.last_mut().unwrap();
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
            }
            Instr::SetList { obj, base: b, n, start } => {
                let t = match th.stack[base + obj as usize] {
                    Value::Table(t) => t,
                    _ => unreachable!("SetList on non-table"),
                };
                let first = base + b as usize;
                let count = if n == 0 { th.top.saturating_sub(first) } else { n as usize };
                *fuel -= count as i64;
                for i in 0..count {
                    let v = th.stack[first + i];
                    self.tables[t.0 as usize]
                        .set(Value::Int(start as i64 + i as i64), v)
                        .map_err(|m| self.rt_err(th, m.to_string()))?;
                }
            }
            Instr::Arith { op, dst, lhs, rhs } => {
                let a = th.stack[base + lhs as usize];
                let b = th.stack[base + rhs as usize];
                match arith(op, a, b) {
                    Ok(v) => th.stack[base + dst as usize] = v,
                    Err(msg) => {
                        let mm = self.binary_mm(a, b, mm_of_arith(op));
                        if mm == Value::Nil {
                            return Err(self.rt_err(th, msg));
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
            }
            Instr::Jump { off } => {
                jump(th, off);
            }
            Instr::Test { src, if_true, off } => {
                if th.stack[base + src as usize].truthy() == if_true {
                    jump(th, off);
                }
            }
            Instr::Call { base: b, nargs, nres } => {
                let func_abs = base + b as usize;
                let argc = if nargs == 0 {
                    th.top.saturating_sub(func_abs + 1)
                } else {
                    (nargs - 1) as usize
                };
                self.do_call(
                    th,
                    fuel,
                    CallSpec {
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
            }
            Instr::Return { base: b, n } => {
                if !th.frames.last().unwrap().tbc.is_empty() {
                    // run __close handlers before completing the return;
                    // snapshot the value window now (closes may clobber top)
                    let fb = th.frames.last().unwrap().base;
                    let start = fb + b as usize;
                    let count =
                        if n == 0 { th.top.saturating_sub(start) } else { (n - 1) as usize };
                    let f = th.frames.last_mut().unwrap();
                    f.pending.push(Pending::FinishReturn { start, count });
                    f.pending.push(Pending::CloseTbc { from: 0, err: Value::Nil });
                    return Ok(Flow::Continue);
                }
                let frame = th.frames.pop().unwrap();
                self.close_upvals(th, frame.base);
                let start = frame.base + b as usize;
                let count =
                    if n == 0 { th.top.saturating_sub(start) } else { (n - 1) as usize };
                if th.frames.is_empty() {
                    let vals = th.stack[start..start + count].to_vec();
                    th.stack.clear();
                    th.top = 0;
                    return Ok(Flow::Finished(vals));
                }
                deliver_return(th, &frame, start, count);
            }
            Instr::Vararg { dst, n } => {
                let varargs = std::mem::take(&mut th.frames.last_mut().unwrap().varargs);
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
                th.frames.last_mut().unwrap().varargs = varargs;
            }
            Instr::Closure { dst, p } => {
                *fuel -= 2;
                let proto = th.frames.last().unwrap().proto.protos[p as usize].clone();
                let mut ups = Vec::with_capacity(proto.upvals.len());
                for d in &proto.upvals {
                    match *d {
                        UpvalDesc::Local(r) => {
                            let abs = base + r as usize;
                            ups.push(self.find_or_create_open(tid, th, abs));
                        }
                        UpvalDesc::Upval(i) => {
                            let cur = th.frames.last().unwrap().closure;
                            ups.push(self.closures[cur.0 as usize].upvals[i as usize]);
                        }
                    }
                }
                let cid = ClosId(self.closures.len() as u32);
                self.closures.push(LuaClosure { proto, upvals: ups });
                th.stack[base + dst as usize] = Value::Closure(cid);
            }
            Instr::Close { from } => {
                self.close_upvals(th, base + from as usize);
                self.run_close_tbc(th, fuel, from, Value::Nil)?;
            }
            Instr::Tbc { reg } => {
                let v = th.stack[base + reg as usize];
                match v {
                    Value::Nil | Value::Bool(false) => {}
                    _ if self.metamethod(v, Mm::Close) != Value::Nil => {
                        th.frames.last_mut().unwrap().tbc.push(reg);
                    }
                    _ => {
                        return Err(self.rt_err(
                            th,
                            format!(
                                "variable of a <close> declaration got a non-closable {} value",
                                v.type_name()
                            ),
                        ))
                    }
                }
            }
            Instr::ForPrep { base: b, off } => {
                self.for_prep(th, base + b as usize, off)?;
            }
            Instr::ForLoop { base: b, off } => {
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
                    _ => unreachable!("ForLoop after ForPrep normalization"),
                }
            }
            Instr::TForLoop { base: b, off } => {
                let a = base + b as usize;
                let v = th.stack[a + 3];
                if v != Value::Nil {
                    th.stack[a + 2] = v;
                    jump(th, off);
                }
            }
        }
        Ok(Flow::Continue)
    }

    /// Calls `f` with `args` copied to a scratch window above the current
    /// frame. Results are delivered to `ret_to` (immediately for natives,
    /// after the pushed frame returns for Lua closures).
    #[allow(clippy::too_many_arguments)]
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
            CallSpec {
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

    fn do_call(&mut self, th: &mut Thread, fuel: &mut i64, spec: CallSpec) -> Result<(), VmError> {
        *fuel -= 2;
        let CallSpec { mut func_abs, mut argc, ret_to, nres, shape, protected, handler, native_caller } =
            spec;
        for _ in 0..MAX_META_CHAIN {
            match th.stack[func_abs] {
                Value::Closure(cid) => {
                    if th.frames.len() >= MAX_CALL_DEPTH {
                        return Err(
                            self.rt_err(th, "stack overflow (too many nested calls)".into())
                        );
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
                    th.frames.push(Frame {
                        closure: cid,
                        proto,
                        pc: 0,
                        base: new_base,
                        ret_to,
                        nres,
                        shape,
                        protected,
                        handler,
                        pending: Vec::new(),
                        tbc: Vec::new(),
                        varargs,
                    });
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
                        return Err(self.rt_err(
                            th,
                            format!("attempt to call a {} value", other.type_name()),
                        ));
                    }
                    let wb = scratch_base(th).max(func_abs + 1 + argc);
                    ensure_len(&mut th.stack, wb + 2 + argc);
                    th.stack[wb] = mm;
                    th.stack.copy_within(func_abs..func_abs + 1 + argc, wb + 1);
                    func_abs = wb;
                    argc += 1;
                }
            }
        }
        Err(self.rt_err(th, "'__call' chain too long".into()))
    }

    #[allow(clippy::too_many_arguments)]
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
                    source: None,
                })?;
                place_shaped(th, ret_to, nres, shape, &res);
                Ok(())
            }
            NativeKind::Intrinsic(i) => self.call_intrinsic(
                th,
                fuel,
                i,
                func_abs,
                argc,
                ret_to,
                nres,
                shape,
                native_caller,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call_intrinsic(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        i: Intrinsic,
        func_abs: usize,
        argc: usize,
        ret_to: usize,
        nres: u8,
        shape: RetShape,
        native_caller: bool,
    ) -> Result<(), VmError> {
        let arg = |th: &Thread, i: usize| -> Value {
            if i < argc { th.stack[func_abs + 1 + i] } else { Value::Nil }
        };
        match i {
            Intrinsic::Error => {
                let v = arg(th, 0);
                let level = match arg(th, 1) {
                    Value::Nil => 1,
                    Value::Int(l) => l,
                    Value::Float(f) => f as i64,
                    _ => 1,
                };
                // level 1 names the caller of error(); when that caller is a
                // native (pcall(error, ...)), there is no Lua position
                let val = match v {
                    Value::Str(s) if level > 0 && !native_caller => {
                        let fidx = th.frames.len().saturating_sub(level as usize);
                        let (line, src) = match th.frames.get(fidx) {
                            Some(f) => (
                                f.proto.lines.get(f.pc.wrapping_sub(1)).copied().unwrap_or(0),
                                f.proto.source.clone(),
                            ),
                            None => (line_of(th), th.frames.last().unwrap().proto.source.clone()),
                        };
                        let msg = format!("{src}:{line}: {}", self.strings.get_str_lossy(s));
                        self.new_string(msg.as_bytes())
                    }
                    _ => v,
                };
                Err(VmError { val: ErrVal::Val(val), line: line_of(th), source: None })
            }
            Intrinsic::Assert => {
                if argc == 0 {
                    return Err(self.rt_err(th, "bad argument #1 to 'assert' (value expected)".into()));
                }
                if arg(th, 0).truthy() {
                    let res = th.stack[func_abs + 1..func_abs + 1 + argc].to_vec();
                    place_shaped(th, ret_to, nres, shape, &res);
                    Ok(())
                } else {
                    match arg(th, 1) {
                        Value::Nil => {
                            let source = if native_caller {
                                None
                            } else {
                                th.frames.last().map(|f| f.proto.source.clone())
                            };
                            Err(VmError {
                                val: ErrVal::Msg("assertion failed!".into()),
                                line: line_of(th),
                                source,
                            })
                        }
                        v => Err(VmError {
                            val: ErrVal::Val(v),
                            line: line_of(th),
                            source: None,
                        }),
                    }
                }
            }
            Intrinsic::ToString => {
                let v = arg(th, 0);
                let mm = self.metamethod(v, Mm::ToString);
                if mm == Value::Nil {
                    let s = self.display_value(v);
                    let sv = self.new_string(s.as_bytes());
                    place_shaped(th, ret_to, nres, shape, &[sv]);
                    Ok(())
                } else {
                    // shape composition: tostring under pcall keeps PrependTrue
                    self.call_value(th, mm, &[v], ret_to, nres, shape, fuel)
                }
            }
            Intrinsic::Pcall => {
                if argc == 0 {
                    return Err(
                        self.rt_err(th, "bad argument #1 to 'pcall' (value expected)".into())
                    );
                }
                self.protected_call(th, fuel, func_abs + 1, argc - 1, ret_to, nres, None)
            }
            Intrinsic::Xpcall => {
                if argc < 2 {
                    return Err(self.rt_err(
                        th,
                        "bad argument #2 to 'xpcall' (value expected)".into(),
                    ));
                }
                let handler = arg(th, 1);
                // rebuild a contiguous window: [f, args...] (handler sits
                // between f and the args in the original window)
                let f = arg(th, 0);
                let wb = scratch_base(th).max(func_abs + 1 + argc);
                let n_args = argc - 2;
                ensure_len(&mut th.stack, wb + 1 + n_args);
                th.stack[wb] = f;
                th.stack.copy_within(func_abs + 3..func_abs + 1 + argc, wb + 1);
                self.protected_call(th, fuel, wb, n_args, ret_to, nres, Some(handler))
            }
            Intrinsic::Resume => {
                let co = arg(th, 0);
                let Value::Thread(co) = co else {
                    return Err(self.rt_err(
                        th,
                        format!(
                            "bad argument #1 to 'resume' (coroutine expected, got {})",
                            co.type_name()
                        ),
                    ));
                };
                let args: Vec<Value> = th.stack[func_abs + 2..func_abs + 1 + argc].to_vec();
                self.resume_thread(th, fuel, co, &args, ret_to, nres, shape, false)
            }
            Intrinsic::WrapResume(co) => {
                let args: Vec<Value> = th.stack[func_abs + 1..func_abs + 1 + argc].to_vec();
                self.resume_thread(th, fuel, co, &args, ret_to, nres, shape, true)
            }
            Intrinsic::Yield => {
                let Some(parent) = th.parent else {
                    return Err(
                        self.rt_err(th, "attempt to yield from outside a coroutine".into())
                    );
                };
                *fuel -= 3;
                let args: Vec<Value> = th.stack[func_abs + 1..func_abs + 1 + argc].to_vec();
                let rr = th.resume_ret.expect("resumed thread has resume_ret");
                th.status = CoStatus::Suspended;
                // the next resume's arguments become this yield call's results
                th.yield_ret = Some((ret_to, nres, shape));
                let parent_th = &mut self.threads[parent.0 as usize];
                parent_th.status = CoStatus::Running;
                deliver_resume(parent_th, rr, true, &args);
                self.switch_to = Some(parent);
                Ok(())
            }
            Intrinsic::IsYieldable => {
                let r = Value::Bool(th.parent.is_some());
                place_shaped(th, ret_to, nres, shape, &[r]);
                Ok(())
            }
            Intrinsic::Running => {
                let cur = Value::Thread(self.current_thread);
                let is_main = Value::Bool(th.parent.is_none());
                place_shaped(th, ret_to, nres, shape, &[cur, is_main]);
                Ok(())
            }
        }
    }

    /// Shared by `coroutine.resume` and wrapped coroutines.
    #[allow(clippy::too_many_arguments)]
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
                Err(me.rt_err(th, msg.into()))
            } else {
                let m = me.new_string(msg.as_bytes());
                place_shaped(th, ret_to, nres, shape, &[Value::Bool(false), m]);
                Ok(())
            }
        };
        if co == self.current_thread {
            return fail(self, th, "cannot resume non-suspended coroutine");
        }
        let status = self.threads[co.0 as usize].status;
        match status {
            CoStatus::Dead => fail(self, th, "cannot resume dead coroutine"),
            CoStatus::Running | CoStatus::Normal => {
                fail(self, th, "cannot resume non-suspended coroutine")
            }
            CoStatus::Start | CoStatus::Suspended => {
                *fuel -= 3;
                let rr = ResumeRet { ret_to, nres, shape, status_bool: !wrap, wrap };
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
                            co_th.stack[1..1 + args.len()].copy_from_slice(args);
                            for i in args.len()..np {
                                co_th.stack[1 + i] = Value::Nil;
                            }
                            let varargs = if proto.is_vararg && args.len() > np {
                                args[np..].to_vec()
                            } else {
                                Vec::new()
                            };
                            co_th.status = CoStatus::Running;
                            co_th.frames.push(Frame {
                                closure: cid,
                                proto,
                                pc: 0,
                                base: 1,
                                ret_to: 0,
                                nres: 0,
                                shape: RetShape::Normal,
                                protected: false,
                                handler: None,
                                pending: Vec::new(),
                                tbc: Vec::new(),
                                varargs,
                            });
                        }
                        Value::Native(nid) => {
                            // native coroutine body: cannot yield; run it to
                            // completion right here
                            let co_th = &mut self.threads[co.0 as usize];
                            co_th.status = CoStatus::Dead;
                            co_th.parent = None;
                            co_th.resume_ret = None;
                            let kind_result = match &self.natives[nid.0 as usize].kind {
                                NativeKind::Plain(f) => f(self, args),
                                NativeKind::Intrinsic(_) => {
                                    Err("cannot use this builtin as a coroutine body".into())
                                }
                            };
                            return match kind_result {
                                Ok(res) => {
                                    let mut all = Vec::with_capacity(res.len() + 1);
                                    if !wrap {
                                        all.push(Value::Bool(true));
                                        all.extend_from_slice(&res);
                                        place_shaped(th, ret_to, nres, shape, &all);
                                    } else {
                                        place_shaped(th, ret_to, nres, shape, &res);
                                    }
                                    Ok(())
                                }
                                Err(msg) => fail(self, th, &msg),
                            };
                        }
                        _ => return fail(self, th, "cannot resume dead coroutine"),
                    }
                } else {
                    // deliver resume args as the pending yield's results
                    let co_th = &mut self.threads[co.0 as usize];
                    co_th.status = CoStatus::Running;
                    let (yret, ynres, yshape) =
                        co_th.yield_ret.take().expect("suspended thread has yield_ret");
                    place_shaped(co_th, yret, ynres, yshape, args);
                }
                th.status = CoStatus::Normal;
                self.switch_to = Some(co);
                Ok(())
            }
        }
    }

    /// Calls the value at `f_abs` under error protection: results arrive as
    /// `true, ...` on success and `false, err` on failure (via the handler
    /// for xpcall).
    #[allow(clippy::too_many_arguments)]
    fn protected_call(
        &mut self,
        th: &mut Thread,
        fuel: &mut i64,
        f_abs: usize,
        argc: usize,
        ret_to: usize,
        nres: u8,
        handler: Option<Value>,
    ) -> Result<(), VmError> {
        let r = self.do_call(
            th,
            fuel,
            CallSpec {
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
            let mm = match cur {
                Value::Table(t) => {
                    let raw = self.tables[t.0 as usize].get(k);
                    if raw != Value::Nil {
                        return Ok(Some(raw));
                    }
                    let mm = self.metamethod(cur, Mm::Index);
                    if mm == Value::Nil {
                        return Ok(Some(Value::Nil));
                    }
                    mm
                }
                _ => {
                    let mm = self.metamethod(cur, Mm::Index);
                    if mm == Value::Nil {
                        return Err(self.rt_err(
                            th,
                            format!("attempt to index a {} value", cur.type_name()),
                        ));
                    }
                    mm
                }
            };
            match mm {
                Value::Closure(_) | Value::Native(_) => {
                    self.call_value(th, mm, &[cur, k], dst_abs, 2, RetShape::Normal, fuel)?;
                    return Ok(None);
                }
                _ => cur = mm,
            }
        }
        Err(self.rt_err(th, "'__index' chain too long; possible loop".into()))
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
            let mm = match cur {
                Value::Table(t) => {
                    if self.tables[t.0 as usize].get(k) != Value::Nil {
                        self.tables[t.0 as usize]
                            .set(k, v)
                            .map_err(|m| self.rt_err(th, m.to_string()))?;
                        return Ok(());
                    }
                    let mm = self.metamethod(cur, Mm::NewIndex);
                    if mm == Value::Nil {
                        self.tables[t.0 as usize]
                            .set(k, v)
                            .map_err(|m| self.rt_err(th, m.to_string()))?;
                        return Ok(());
                    }
                    mm
                }
                _ => {
                    let mm = self.metamethod(cur, Mm::NewIndex);
                    if mm == Value::Nil {
                        return Err(self.rt_err(
                            th,
                            format!("attempt to index a {} value", cur.type_name()),
                        ));
                    }
                    mm
                }
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
        Err(self.rt_err(th, "'__newindex' chain too long; possible loop".into()))
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
                let shape = if op == CmpOp::Eq { RetShape::ToBool } else { RetShape::ToNotBool };
                if values_equal(a, b) {
                    th.stack[dst_abs] = Value::Bool(op == CmpOp::Eq);
                    return Ok(());
                }
                // __eq fires only for table/table (or userdata) raw-unequal pairs
                if let (Value::Table(_), Value::Table(_)) = (a, b) {
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
                match self.less_than(a, b, or_equal) {
                    Some(r) => {
                        th.stack[dst_abs] = Value::Bool(r);
                        Ok(())
                    }
                    None => {
                        let mm =
                            self.binary_mm(a, b, if or_equal { Mm::Le } else { Mm::Lt });
                        if mm == Value::Nil {
                            return Err(self.rt_err(
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
        loop {
            let f = th.frames.last_mut().unwrap();
            match f.tbc.last() {
                Some(&r) if r >= from => {
                    f.tbc.pop();
                    let v = th.stack[f.base + r as usize];
                    let mm = self.metamethod(v, Mm::Close);
                    if mm == Value::Nil {
                        continue; // metatable changed since Tbc; skip
                    }
                    th.frames.last_mut().unwrap().pending.push(Pending::CloseTbc { from, err });
                    let scratch = scratch_base(th);
                    return self.call_value(th, mm, &[v, err], scratch, 1, RetShape::Normal, fuel);
                }
                _ => return Ok(()),
            }
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
        let base = th.frames.last().unwrap().base;
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
                return Err(self.rt_err(
                    th,
                    format!("attempt to concatenate a {} value", bad.type_name()),
                ));
            }
            let (ret_abs, pending) = if n - 1 == 1 {
                (base + dst as usize, None)
            } else {
                (first + n - 2, Some(Pending::Concat { dst, base: b, n: (n - 1) as u8 }))
            };
            if let Some(p) = pending {
                th.frames.last_mut().unwrap().pending.push(p);
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
                    out.extend_from_slice(fmt_number(v).as_bytes())
                }
                _ => unreachable!("flat_concat on non-concatable"),
            }
        }
        out
    }

    fn for_prep(&mut self, th: &mut Thread, a: usize, off: i32) -> Result<(), VmError> {
        let init = th.stack[a];
        let limit = th.stack[a + 1];
        let step = th.stack[a + 2];
        let num = |me: &Self, v: Value, what: &str| -> Result<Value, VmError> {
            match v {
                Value::Int(_) | Value::Float(_) => Ok(v),
                _ => Err(me.rt_err(th, format!("'for' {what} must be a number"))),
            }
        };
        let init = num(self, init, "initial value")?;
        let limit = num(self, limit, "limit")?;
        let step = num(self, step, "step")?;
        match (init, step) {
            (Value::Int(i0), Value::Int(s)) => {
                if s == 0 {
                    return Err(self.rt_err(th, "'for' step is zero".into()));
                }
                let l = match limit {
                    Value::Int(l) => Some(l),
                    Value::Float(f) => for_int_limit(f, s > 0),
                    _ => unreachable!(),
                };
                match l {
                    Some(l) if (s > 0 && i0 <= l) || (s < 0 && i0 >= l) => {
                        th.stack[a + 1] = Value::Int(l);
                        th.stack[a + 3] = Value::Int(i0);
                    }
                    _ => jump(th, off),
                }
            }
            _ => {
                let to_f = |v: Value| match v {
                    Value::Int(i) => i as f64,
                    Value::Float(f) => f,
                    _ => unreachable!(),
                };
                let (i0, l, s) = (to_f(init), to_f(limit), to_f(step));
                if s == 0.0 {
                    return Err(self.rt_err(th, "'for' step is zero".into()));
                }
                if (s > 0.0 && i0 <= l) || (s < 0.0 && i0 >= l) {
                    th.stack[a] = Value::Float(i0);
                    th.stack[a + 1] = Value::Float(l);
                    th.stack[a + 2] = Value::Float(s);
                    th.stack[a + 3] = Value::Float(i0);
                } else {
                    jump(th, off);
                }
            }
        }
        Ok(())
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
                _ => (None, Some((Mm::Unm, format!(
                    "attempt to perform arithmetic on a {} value",
                    v.type_name()
                )))),
            },
            UnaryOp::BNot => match to_int(v) {
                Some(i) => (Some(Value::Int(!i)), None),
                None => (None, Some((Mm::BNot, format!(
                    "attempt to perform bitwise operation on a {} value",
                    v.type_name()
                )))),
            },
            UnaryOp::Len => match v {
                Value::Str(s) => (Some(Value::Int(self.strings.get(s).len() as i64)), None),
                Value::Table(t) => {
                    let mm = self.metamethod(v, Mm::Len);
                    if mm == Value::Nil {
                        (Some(Value::Int(self.tables[t.0 as usize].length())), None)
                    } else {
                        self.call_value(th, mm, &[v], dst_abs, 2, RetShape::Normal, fuel)?;
                        return Ok(());
                    }
                }
                _ => (None, Some((Mm::Len, format!(
                    "attempt to get length of a {} value",
                    v.type_name()
                )))),
            },
        };
        if let Some(r) = result {
            th.stack[dst_abs] = r;
            return Ok(());
        }
        let (mm_name, msg) = mm.unwrap();
        let m = self.metamethod(v, mm_name);
        if m == Value::Nil {
            return Err(self.rt_err(th, msg));
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
            (Value::Int(x), Value::Float(y)) => {
                Some(if or_equal { int_le_float(x, y) } else { int_lt_float(x, y) })
            }
            (Value::Float(x), Value::Int(y)) => {
                if x.is_nan() {
                    return Some(false);
                }
                // x < y  <=>  not (y <= x);  x <= y  <=>  not (y < x)
                Some(if or_equal { !int_lt_float(y, x) } else { !int_le_float(y, x) })
            }
            (Value::Str(x), Value::Str(y)) => {
                let (xs, ys) = (self.strings.get(x), self.strings.get(y));
                Some(if or_equal { xs <= ys } else { xs < ys })
            }
            _ => None,
        }
    }

    fn frame_upval(&self, th: &Thread, up: u8) -> UpvalId {
        let cur = th.frames.last().unwrap().closure;
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

    fn rt_err(&self, th: &Thread, message: String) -> VmError {
        let source = th.frames.last().map(|f| f.proto.source.clone());
        VmError { val: ErrVal::Msg(message), line: line_of(th), source }
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
    Finished(Vec<Value>),
    Switch(ThreadId),
}

pub(crate) enum RunOutcome {
    Done(Vec<Value>),
    Pending(ThreadId),
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

impl Execution {
    /// Runs the script for at most `fuel` units of work (roughly one unit
    /// per VM instruction, with surcharges for calls and allocations).
    /// Returns `Step::Pending` if the budget ran out — call again to resume.
    pub fn step(&mut self, lua: &mut Lua, fuel: u64) -> Result<Step, Error> {
        if self.finished {
            return Err(Error::Runtime(RuntimeError {
                message: "execution already finished".into(),
                value: Value::Nil,
                line: 0,
            }));
        }
        let budget = fuel.min(i64::MAX as u64) as i64;
        let mut remaining = budget - self.debt;
        if remaining <= 0 {
            self.debt -= budget;
            return Ok(Step::Pending);
        }
        match lua.run(self.current, &mut remaining) {
            Ok(RunOutcome::Done(vals)) => {
                self.finished = true;
                Ok(Step::Done(vals))
            }
            Ok(RunOutcome::Pending(current)) => {
                self.current = current;
                self.debt = (-remaining).max(0);
                Ok(Step::Pending)
            }
            Err(e) => {
                self.finished = true;
                Err(Error::Runtime(e))
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

// ---- free helpers ----

fn kval(th: &Thread, k: u16) -> Value {
    th.frames.last().unwrap().proto.consts[k as usize]
}

fn jump(th: &mut Thread, off: i32) {
    let f = th.frames.last_mut().unwrap();
    f.pc = (f.pc as i64 + off as i64) as usize;
}

fn line_of(th: &Thread) -> u32 {
    let Some(f) = th.frames.last() else { return 0 };
    f.proto.lines.get(f.pc.wrapping_sub(1)).copied().unwrap_or(0)
}

fn ensure_len(stack: &mut Vec<Value>, len: usize) {
    if stack.len() < len {
        stack.resize(len, Value::Nil);
    }
}

/// First stack slot safely above all live data of the current frame.
/// Bounded by the frame's register window (plus any active multret run),
/// so repeated metamethod calls reuse the same scratch space instead of
/// growing the stack.
fn scratch_base(th: &Thread) -> usize {
    let f = th.frames.last().unwrap();
    let frame_top = f.base + f.proto.max_regs as usize;
    frame_top.max(th.top)
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
fn deliver_return(th: &mut Thread, frame: &Frame, start: usize, count: usize) {
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
                    th.stack[ret_to + i] =
                        if i < count { th.stack[start + i] } else { Value::Nil };
                }
            }
        }
        RetShape::ToBool | RetShape::ToNotBool => {
            let v = if count > 0 { th.stack[start] } else { Value::Nil };
            let b = if frame.shape == RetShape::ToBool { v.truthy() } else { !v.truthy() };
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

/// `i < f` with exact semantics across the full i64/f64 ranges.
fn int_lt_float(i: i64, f: f64) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= 9.223372036854776e18 {
        return true; // f >= 2^63 > any i64
    }
    if f < -9.223372036854776e18 {
        return false;
    }
    let ff = f.floor();
    let fi = ff as i64;
    i < fi || (i == fi && f > ff)
}

fn int_le_float(i: i64, f: f64) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= 9.223372036854776e18 {
        return true;
    }
    if f < -9.223372036854776e18 {
        return false;
    }
    let ff = f.floor();
    let fi = ff as i64;
    i < fi || (i == fi && f >= ff)
}

/// Converts a float limit of an integer `for` loop per Lua 5.4 (floor/ceil
/// toward the loop interior, clamped). `None` means the loop is empty.
fn for_int_limit(f: f64, step_positive: bool) -> Option<i64> {
    if f.is_nan() {
        return None;
    }
    if step_positive {
        if f < -9.223372036854776e18 {
            None
        } else if f >= 9.223372036854776e18 {
            Some(i64::MAX)
        } else {
            Some(f.floor() as i64)
        }
    } else if f >= 9.223372036854776e18 {
        None
    } else if f < -9.223372036854776e18 {
        Some(i64::MIN)
    } else {
        Some(f.ceil() as i64)
    }
}

fn arith(op: ArithOp, a: Value, b: Value) -> Result<Value, String> {
    use ArithOp::*;
    let num_err = |v: Value| {
        format!("attempt to perform arithmetic on a {} value", v.type_name())
    };
    let int_err = |v: Value| match v {
        Value::Float(_) => "number has no integer representation".to_string(),
        _ => format!("attempt to perform bitwise operation on a {} value", v.type_name()),
    };
    match op {
        Add | Sub | Mul => match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(match op {
                Add => x.wrapping_add(y),
                Sub => x.wrapping_sub(y),
                Mul => x.wrapping_mul(y),
                _ => unreachable!(),
            })),
            _ => {
                let x = to_float(a).ok_or_else(|| num_err(a))?;
                let y = to_float(b).ok_or_else(|| num_err(b))?;
                Ok(Value::Float(match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    _ => unreachable!(),
                }))
            }
        },
        Div => {
            let x = to_float(a).ok_or_else(|| num_err(a))?;
            let y = to_float(b).ok_or_else(|| num_err(b))?;
            Ok(Value::Float(x / y))
        }
        Pow => {
            let x = to_float(a).ok_or_else(|| num_err(a))?;
            let y = to_float(b).ok_or_else(|| num_err(b))?;
            Ok(Value::Float(x.powf(y)))
        }
        IDiv => match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if y == 0 {
                    return Err("attempt to perform 'n//0'".into());
                }
                let q = x.wrapping_div(y);
                let q = if x.wrapping_rem(y) != 0 && (x < 0) != (y < 0) { q - 1 } else { q };
                Ok(Value::Int(q))
            }
            _ => {
                let x = to_float(a).ok_or_else(|| num_err(a))?;
                let y = to_float(b).ok_or_else(|| num_err(b))?;
                Ok(Value::Float((x / y).floor()))
            }
        },
        Mod => match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if y == 0 {
                    return Err("attempt to perform 'n%0'".into());
                }
                let r = x.wrapping_rem(y);
                Ok(Value::Int(if r != 0 && (r < 0) != (y < 0) { r + y } else { r }))
            }
            _ => {
                let x = to_float(a).ok_or_else(|| num_err(a))?;
                let y = to_float(b).ok_or_else(|| num_err(b))?;
                let r = x % y;
                Ok(Value::Float(if r != 0.0 && (r < 0.0) != (y < 0.0) { r + y } else { r }))
            }
        },
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
            let n = if op == Shr { y.checked_neg().unwrap_or(i64::MAX) } else { y };
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
