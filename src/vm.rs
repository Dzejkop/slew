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
use crate::parser::{ParseError, parse};
use crate::value::{
    ClosId, NativeId, StrId, Strings, Table, TableId, ThreadId, UpvalId, Value, float_to_exact_int,
    fmt_number,
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
    /// Line where the error was raised; for errors inside a called function
    /// or a `load`ed chunk this points into that chunk, not the root script.
    pub line: u32,
    /// Line of the outermost frame (the root script) when the error escaped.
    /// Handy for progress reporting: a script that calls a helper still
    /// reports its own call site here.
    pub root_line: u32,
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
    /// Line of the outermost frame at raise time (the script's own call
    /// site). Set by the dispatcher before unwinding pops frames, since
    /// `recover` discards them.
    pub root_line: u32,
    /// Chunk name for the position prefix; `None` for native errors,
    /// which carry no position (matching PUC).
    pub source: Option<Rc<str>>,
}

pub type NativeFn = fn(&mut Lua, &[Value]) -> Result<Vec<Value>, String>;

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
    IsYieldable,
    Running,
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
    DeliverError {
        ret_to: usize,
        nres: u8,
        err: Value,
        handler: Option<Value>,
    },
    /// Final step of a return that had to run `__close` handlers first.
    FinishReturn { start: usize, count: usize },
    /// Final step of a tail call into a native/intrinsic: the callee's
    /// results now sit at `start` (with `th.top` already updated); complete
    /// the frame's return as usual.
    TailReturn { start: usize },
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
    /// True if this frame was entered via a proper tail call.
    tailcall: bool,
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
    pub(crate) string_meta: Option<TableId>,
    mm_names: Vec<StrId>,
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
    natives_live: Vec<bool>,
    natives_free: Vec<u32>,
    /// Root + current thread per live (unfinished) execution.
    exec_roots: std::collections::HashMap<u32, u32>,
    /// Host-pinned values (see [`Lua::anchor`]).
    anchors: Vec<Value>,
    /// Placeholder proto for swept closure slots.
    empty_proto: Rc<Proto>,
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
}

impl Default for Lua {
    fn default() -> Self {
        Self::new()
    }
}

impl Lua {
    pub fn new() -> Self {
        let mut strings = Strings::default();
        let mm_names = MM_NAMES
            .iter()
            .map(|n| strings.intern_fixed(n.as_bytes()))
            .collect();
        let mut lua = Lua {
            strings,
            tables: vec![Table::default()],
            closures: Vec::new(),
            natives: Vec::new(),
            upvals: Vec::new(),
            threads: Vec::new(),
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
            natives_live: Vec::new(),
            natives_free: Vec::new(),
            exec_roots: std::collections::HashMap::new(),
            anchors: Vec::new(),
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
        };
        lua.seed_random(0x536c65775f5f5f31); // "Slew____1"
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
        let cid = self.alloc_closure(LuaClosure {
            proto: chunk.proto.clone(),
            upvals: vec![env],
        });
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
            tailcall: false,
        });
        let tid = self.alloc_thread(th);
        // GC root for as long as the execution is live
        self.exec_roots.insert(tid.0, tid.0);
        Execution {
            thread: tid,
            current: tid,
            debt: 0,
            finished: false,
        }
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
        self.allocs_since_gc += 1;
        let n = Native {
            name: name.into(),
            kind,
        };
        let id = match self.natives_free.pop() {
            Some(i) => {
                self.natives[i as usize] = n;
                self.natives_live[i as usize] = true;
                NativeId(i)
            }
            None => {
                self.natives.push(n);
                self.natives_live.push(true);
                NativeId(self.natives.len() as u32 - 1)
            }
        };
        Value::Native(id)
    }

    pub(crate) fn register_intrinsic(&mut self, name: &str, i: Intrinsic) -> Value {
        let v = self.add_native_kind(name, NativeKind::Intrinsic(i));
        let k = self.new_string(name.as_bytes());
        self.tables[self.globals.0 as usize].set(k, v).unwrap();
        v
    }

    pub fn new_string(&mut self, s: &[u8]) -> Value {
        self.allocs_since_gc += 1;
        Value::Str(self.strings.intern(s))
    }

    pub fn new_table(&mut self) -> Value {
        self.allocs_since_gc += 1;
        let id = match self.tables_free.pop() {
            Some(i) => {
                self.tables[i as usize] = Table::default();
                self.tables_live[i as usize] = true;
                TableId(i)
            }
            None => {
                self.tables.push(Table::default());
                self.tables_live.push(true);
                TableId(self.tables.len() as u32 - 1)
            }
        };
        Value::Table(id)
    }

    pub(crate) fn alloc_closure(&mut self, c: LuaClosure) -> ClosId {
        self.allocs_since_gc += 1;
        match self.closures_free.pop() {
            Some(i) => {
                self.closures[i as usize] = c;
                self.closures_live[i as usize] = true;
                ClosId(i)
            }
            None => {
                self.closures.push(c);
                self.closures_live.push(true);
                ClosId(self.closures.len() as u32 - 1)
            }
        }
    }

    fn alloc_thread(&mut self, th: Thread) -> ThreadId {
        self.allocs_since_gc += 1;
        match self.threads_free.pop() {
            Some(i) => {
                self.threads[i as usize] = th;
                self.threads_live[i as usize] = true;
                ThreadId(i)
            }
            None => {
                self.threads.push(th);
                self.threads_live.push(true);
                ThreadId(self.threads.len() as u32 - 1)
            }
        }
    }

    /// Creates a coroutine from a function value (for `coroutine.create`).
    pub(crate) fn create_coroutine(&mut self, f: Value) -> Value {
        let mut th = Thread::default();
        th.stack.push(f); // consumed on first resume
        th.status = CoStatus::Start;
        Value::Thread(self.alloc_thread(th))
    }

    fn new_upval(&mut self, u: Upval) -> UpvalId {
        self.allocs_since_gc += 1;
        match self.upvals_free.pop() {
            Some(i) => {
                self.upvals[i as usize] = u;
                self.upvals_live[i as usize] = true;
                UpvalId(i)
            }
            None => {
                self.upvals.push(u);
                self.upvals_live.push(true);
                UpvalId(self.upvals.len() as u32 - 1)
            }
        }
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

    /// `luaL_tolstring`'s fallback path, used after `__tostring` has been
    /// ruled out: a string `__name` field replaces the type tag, then the
    /// default rendering (`name: 0x...`). Strings keep their raw rendering
    /// (PUC only consults `__tostring` for them), and `display_value`'s
    /// output is used when there is no `__name`.
    pub(crate) fn tostring_default(&mut self, v: Value) -> String {
        let custom = match v {
            Value::Table(_) | Value::Closure(_) | Value::Native(_) | Value::Thread(_) => {
                match self.metamethod_pub(v, "__name") {
                    Value::Str(id) => Some(self.strings.get_str_lossy(id).into_owned()),
                    _ => None,
                }
            }
            _ => None,
        };
        match custom {
            Some(name) => {
                let ptr = match v {
                    Value::Table(t) => t.0,
                    Value::Closure(c) => c.0,
                    Value::Thread(t) => t.0,
                    Value::Native(n) => n.0,
                    _ => unreachable!(),
                };
                format!("{name}: 0x{ptr:08x}")
            }
            None => self.display_value(v),
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
            Value::Nil => self.type_metas[0],
            Value::Bool(_) => self.type_metas[1],
            Value::Int(_) | Value::Float(_) => self.type_metas[2],
            Value::Closure(_) | Value::Native(_) => self.type_metas[3],
            Value::Thread(_) => self.type_metas[4],
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

    /// Frame at `level` in `target` (or the running thread), or the C
    /// function `getinfo` at current level 0. `None` when out of range.
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
        let idx = if current {
            if level == 0 {
                return Some(LevelFrame::Native);
            }
            if level > frames.len() {
                return None;
            }
            frames.len() - level
        } else {
            if level >= frames.len() {
                return None;
            }
            frames.len() - 1 - level
        };
        let fr = frames.get(idx)?;
        let name = if idx > 0 {
            let caller = &frames[idx - 1];
            caller
                .proto
                .call_names
                .get(caller.pc.wrapping_sub(1))
                .cloned()
                .flatten()
        } else {
            None
        };
        Some(LevelFrame::Lua {
            proto: fr.proto.clone(),
            pc: fr.pc,
            closure: fr.closure,
            tailcall: fr.tailcall,
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

    #[allow(clippy::too_many_arguments)]
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
            None | Some(Value::Nil) => {
                if target.is_none() {
                    1
                } else {
                    0
                }
            }
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
        // Level 0 is the running frame of `target`; for the current thread
        // (no C-frame model) it behaves like level 1. A level past the stack
        // shows no frames at all (PUC `luaL_traceback`).
        let first = if current {
            frames.len().checked_sub(level.max(1) as usize)
        } else {
            frames.len().checked_sub(1 + level.max(0) as usize)
        };
        let Some(first) = first else {
            return Ok(self.new_string(out.as_bytes()));
        };
        for idx in (0..=first).rev() {
            let fr = &frames[idx];
            let line = fr
                .proto
                .lines
                .get(fr.pc.wrapping_sub(1))
                .copied()
                .unwrap_or(0);
            let src = short_source(&fr.proto.source);
            out.push_str(&format!("\n\t{src}:{line}: in "));
            let name = if idx > 0 {
                let caller = &frames[idx - 1];
                caller
                    .proto
                    .call_names
                    .get(caller.pc.wrapping_sub(1))
                    .cloned()
                    .flatten()
            } else {
                None
            };
            match name {
                Some((nw, nm)) => out.push_str(&format!("function '{nm}' ({nw})")),
                None if fr.proto.linedefined == 0 => out.push_str("main chunk"),
                None => out.push_str(&format!("function <{src}:{}>", fr.proto.linedefined)),
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
            .map(|s| s.to_string())
            .unwrap_or_else(|| "(...)".to_string());
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
            .map(|s| s.to_string())
            .unwrap_or_else(|| "(...)".to_string());
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
                Err(mut e) => {
                    let mut root_line = None;
                    loop {
                        // capture the root script's own position before the
                        // frames are cleared below (on coroutine propagation
                        // this is the resume call site; for root-origin
                        // errors the frames are already gone and the
                        // dispatcher's `root_line` stands)
                        if root_line.is_none() && th.parent.is_none() {
                            root_line =
                                Some(th.frames.first().map(frame_line).unwrap_or(e.root_line));
                        }
                        // error escaped this thread entirely; close its open
                        // upvalues (closures may outlive the thread) and kill it
                        self.close_upvals(&mut th, 0);
                        th.frames.clear();
                        th.status = CoStatus::Dead;
                        let parent = th.parent.take();
                        let rr = th.resume_ret.take();
                        th.stack.clear();
                        self.threads[cur.0 as usize] = th;
                        match parent {
                            None => {
                                let root_line = root_line.unwrap_or(e.line);
                                return Err(self.materialize_error(e, root_line));
                            }
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
                    }
                }
            }
        }
    }

    fn materialize_error(&mut self, e: VmError, root_line: u32) -> RuntimeError {
        let value = self.err_value(&e);
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
            if *fuel <= 0 {
                return Ok(DispatchEnd::Pending);
            }
            match self.exec_one(tid, th, fuel) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Finished(vals)) => return Ok(DispatchEnd::Finished(vals)),
                Err(mut e) => {
                    // capture the script's own call site before `recover`
                    // pops the frames it unwinds
                    if e.root_line == 0 {
                        e.root_line = th.frames.first().map(frame_line).unwrap_or(0);
                    }
                    self.recover(th, e)?;
                }
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
                Pending::TailReturn { start } => {
                    let count = th.top.saturating_sub(start);
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
                self.maybe_gc(tid, th)?;
            }
            Instr::SetList {
                obj,
                base: b,
                n,
                start,
            } => {
                let t = match th.stack[base + obj as usize] {
                    Value::Table(t) => t,
                    _ => unreachable!("SetList on non-table"),
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
                        .map_err(|m| self.rt_err(th, m.to_string()))?;
                }
            }
            Instr::Arith { op, dst, lhs, rhs } => {
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
                                let f = th.frames.last().unwrap();
                                match reg.and_then(|r| {
                                    name_for_register(
                                        &self.strings,
                                        &f.proto,
                                        f.pc.wrapping_sub(1),
                                        r,
                                    )
                                }) {
                                    Some(v) => {
                                        format!("number ({v}) has no integer representation")
                                    }
                                    None => msg,
                                }
                            } else {
                                msg
                            };
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
            Instr::TailCall { base: b, nargs } => {
                let fb = th.frames.last().unwrap().base;
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
                            .pending
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
            }
            Instr::Return { base: b, n } => {
                if !th.frames.last().unwrap().tbc.is_empty() {
                    // run __close handlers before completing the return;
                    // snapshot the value window now (closes may clobber top)
                    let fb = th.frames.last().unwrap().base;
                    let start = fb + b as usize;
                    let count = if n == 0 {
                        th.top.saturating_sub(start)
                    } else {
                        (n - 1) as usize
                    };
                    let f = th.frames.last_mut().unwrap();
                    f.pending.push(Pending::FinishReturn { start, count });
                    f.pending.push(Pending::CloseTbc {
                        from: 0,
                        err: Value::Nil,
                    });
                    return Ok(Flow::Continue);
                }
                let frame = th.frames.pop().unwrap();
                self.close_upvals(th, frame.base);
                let start = frame.base + b as usize;
                let count = if n == 0 {
                    th.top.saturating_sub(start)
                } else {
                    (n - 1) as usize
                };
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
                let cid = self.alloc_closure(LuaClosure { proto, upvals: ups });
                th.stack[base + dst as usize] = Value::Closure(cid);
                self.maybe_gc(tid, th)?;
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
                        ));
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
        let CallSpec {
            mut func_abs,
            mut argc,
            ret_to,
            nres,
            shape,
            protected,
            handler,
            native_caller,
        } = spec;
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
                        tailcall: false,
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
                        return Err(self
                            .rt_err(th, format!("attempt to call a {} value", other.type_name())));
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
                        return Err(self
                            .rt_err(th, format!("attempt to call a {} value", other.type_name())));
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
        let old = th.frames.last().unwrap();
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
            tailcall: true,
        });
        Ok(())
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
                    root_line: 0,
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
        let arg_opt = |th: &Thread, i: usize| -> Option<Value> {
            if i < argc {
                Some(th.stack[func_abs + 1 + i])
            } else {
                None
            }
        };
        let arg = |th: &Thread, i: usize| -> Value { arg_opt(th, i).unwrap_or(Value::Nil) };
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
                                f.proto
                                    .lines
                                    .get(f.pc.wrapping_sub(1))
                                    .copied()
                                    .unwrap_or(0),
                                f.proto.source.clone(),
                            ),
                            None => (line_of(th), th.frames.last().unwrap().proto.source.clone()),
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
            Intrinsic::Assert => {
                if argc == 0 {
                    return Err(
                        self.rt_err(th, "bad argument #1 to 'assert' (value expected)".into())
                    );
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
            Intrinsic::ToString => {
                let v = arg(th, 0);
                let mm = self.metamethod(v, Mm::ToString);
                if mm == Value::Nil {
                    let s = self.tostring_default(v);
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
                    return Err(
                        self.rt_err(th, "bad argument #2 to 'xpcall' (value expected)".into())
                    );
                }
                let handler = arg(th, 1);
                // rebuild a contiguous window: [f, args...] (handler sits
                // between f and the args in the original window)
                let f = arg(th, 0);
                let wb = scratch_base(th).max(func_abs + 1 + argc);
                let n_args = argc - 2;
                ensure_len(&mut th.stack, wb + 1 + n_args);
                th.stack[wb] = f;
                th.stack
                    .copy_within(func_abs + 3..func_abs + 1 + argc, wb + 1);
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
                    return Err(self.rt_err(th, "attempt to yield from outside a coroutine".into()));
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
            Intrinsic::DebugGetinfo => {
                let a0 = arg_opt(th, 0);
                let (target, base) = match a0 {
                    Some(Value::Thread(t)) => (Some(t), 1usize),
                    _ => (None, 0usize),
                };
                let f = arg_opt(th, base);
                let what = arg(th, base + 1);
                let r = self
                    .debug_getinfo(th, target, f, what, base + 1)
                    .map_err(|m| self.rt_err(th, m))?;
                place_shaped(th, ret_to, nres, shape, &[r]);
                Ok(())
            }
            Intrinsic::DebugTraceback => {
                let a0 = arg_opt(th, 0);
                let (target, base) = match a0 {
                    Some(Value::Thread(t)) => (Some(t), 1usize),
                    _ => (None, 0usize),
                };
                let message = arg(th, base);
                let level = arg_opt(th, base + 1);
                let r = self
                    .debug_traceback(th, target, message, level, base + 2)
                    .map_err(|m| self.rt_err(th, m))?;
                place_shaped(th, ret_to, nres, shape, &[r]);
                Ok(())
            }
            Intrinsic::DebugGetupvalue => {
                // PUC checks the index (arg #2) before the function (arg #1).
                let n = self
                    .debug_check_int(arg_opt(th, 1), 2, "debug.getupvalue")
                    .map_err(|m| self.rt_err(th, m))?;
                let f = arg(th, 0);
                match f {
                    Value::Closure(cid) => match self.debug_getupvalue(th, cid, n) {
                        Some((name, val)) => {
                            let nv = self.new_string(name.as_bytes());
                            place_shaped(th, ret_to, nres, shape, &[nv, val]);
                        }
                        // Out of range: PUC returns no values.
                        None => place_shaped(th, ret_to, nres, shape, &[]),
                    },
                    // A native is a C function: it is a valid function with no
                    // upvalues, so `lua_getupvalue` returns NULL -> zero values.
                    Value::Native(_) => place_shaped(th, ret_to, nres, shape, &[]),
                    other => {
                        return Err(self.rt_err(
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
            Intrinsic::DebugSetupvalue => {
                // PUC checks the value (arg #3), then index (#2), then
                // function (#1).
                if arg_opt(th, 2).is_none() {
                    return Err(self.rt_err(
                        th,
                        "bad argument #3 to 'debug.setupvalue' (value expected)".into(),
                    ));
                }
                let n = self
                    .debug_check_int(arg_opt(th, 1), 2, "debug.setupvalue")
                    .map_err(|m| self.rt_err(th, m))?;
                let v = arg(th, 2);
                let f = arg(th, 0);
                match f {
                    Value::Closure(cid) => match self.debug_setupvalue(th, cid, n, v) {
                        Some(name) => {
                            let nv = self.new_string(name.as_bytes());
                            place_shaped(th, ret_to, nres, shape, &[nv]);
                        }
                        None => place_shaped(th, ret_to, nres, shape, &[]),
                    },
                    // Native (C) functions have no upvalues: `lua_setupvalue`
                    // returns NULL, and the API reports zero values.
                    Value::Native(_) => place_shaped(th, ret_to, nres, shape, &[]),
                    other => {
                        return Err(self.rt_err(
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
            Intrinsic::DebugUpvalueid => {
                let n = self
                    .debug_check_int(arg_opt(th, 1), 2, "debug.upvalueid")
                    .map_err(|m| self.rt_err(th, m))?;
                let f = arg(th, 0);
                let r = match f {
                    Value::Closure(cid) => self.debug_upvalueid(cid, n),
                    // `lua_upvalueid` returns NULL for a C function (no
                    // upvalues); PUC pushes fail (nil) as a single value.
                    Value::Native(_) => Value::Nil,
                    other => {
                        return Err(self.rt_err(
                            th,
                            format!(
                                "bad argument #1 to 'debug.upvalueid' (function expected, got {})",
                                other.type_name()
                            ),
                        ));
                    }
                };
                place_shaped(th, ret_to, nres, shape, &[r]);
                Ok(())
            }
            Intrinsic::DebugUpvaluejoin => {
                // PUC's `checkupval` validates each (function, index) pair in
                // order: index (#2/#4) then function (#1/#3) then upvalue
                // existence. A native has no upvalues, so it fails the index
                // check; a non-function fails the type check.
                let n1 = self
                    .debug_check_int(arg_opt(th, 1), 2, "debug.upvaluejoin")
                    .map_err(|m| self.rt_err(th, m))?;
                let c1 = self
                    .debug_check_upval(arg(th, 0), n1, 1, 2, "debug.upvaluejoin")
                    .map_err(|m| self.rt_err(th, m))?;
                let n2 = self
                    .debug_check_int(arg_opt(th, 3), 4, "debug.upvaluejoin")
                    .map_err(|m| self.rt_err(th, m))?;
                let c2 = self
                    .debug_check_upval(arg(th, 2), n2, 3, 4, "debug.upvaluejoin")
                    .map_err(|m| self.rt_err(th, m))?;
                self.debug_upvaluejoin(c1, n1, c2, n2)
                    .map_err(|m| self.rt_err(th, m))?;
                place_shaped(th, ret_to, nres, shape, &[]);
                Ok(())
            }
            Intrinsic::DebugGetmetatable => {
                let v = arg(th, 0);
                let r = match self.get_metatable(v) {
                    Some(mt) => Value::Table(mt),
                    None => Value::Nil,
                };
                place_shaped(th, ret_to, nres, shape, &[r]);
                Ok(())
            }
            Intrinsic::DebugSetmetatable => {
                let v = arg(th, 0);
                let mt = match arg(th, 1) {
                    Value::Nil => None,
                    Value::Table(t) => Some(t),
                    other => {
                        return Err(self.rt_err(
                            th,
                            format!(
                                "bad argument #2 to 'setmetatable' (nil or table expected, got {})",
                                other.type_name()
                            ),
                        ));
                    }
                };
                self.set_raw_metatable(v, mt);
                place_shaped(th, ret_to, nres, shape, &[v]);
                Ok(())
            }
            Intrinsic::DebugGetregistry => {
                place_shaped(th, ret_to, nres, shape, &[Value::Table(self.globals)]);
                Ok(())
            }
            Intrinsic::DebugGethook => {
                // No hooks are supported (tier c); report "no hook".
                place_shaped(th, ret_to, nres, shape, &[Value::Nil]);
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
                                tailcall: false,
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
                    let (yret, ynres, yshape) = co_th
                        .yield_ret
                        .take()
                        .expect("suspended thread has yield_ret");
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
                        return Err(self
                            .rt_err(th, format!("attempt to index a {} value", cur.type_name())));
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
                        return Err(self
                            .rt_err(th, format!("attempt to index a {} value", cur.type_name())));
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
                let shape = if op == CmpOp::Eq {
                    RetShape::ToBool
                } else {
                    RetShape::ToNotBool
                };
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
                        let mm = self.binary_mm(a, b, if or_equal { Mm::Le } else { Mm::Lt });
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
                    th.frames
                        .last_mut()
                        .unwrap()
                        .pending
                        .push(Pending::CloseTbc { from, err });
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

    /// Approximate live heap footprint in bytes.
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
                        .map(|f| f.varargs.len() * 16)
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
        total
    }

    /// Auto-GC trigger from allocation sites inside the dispatch loop.
    /// `th` is the running thread (moved out of the arena), which must be
    /// traced as an extra root. Enforces the memory ceiling.
    fn maybe_gc(&mut self, tid: ThreadId, th: &Thread) -> Result<(), VmError> {
        let due = self.gc_alloc_threshold != 0
            && (self.allocs_since_gc >= self.gc_alloc_threshold
                || self.strings.bytes() > self.str_bytes_at_gc + (8 << 20));
        if due {
            self.collect(Some((tid, th)));
        }
        if let Some(limit) = self.memory_limit {
            // only re-measured at collection points; cheap proxy otherwise
            if due && self.memory_used() > limit {
                return Err(self.rt_err(th, "not enough memory".into()));
            }
        }
        Ok(())
    }

    fn collect(&mut self, extra: Option<(ThreadId, &Thread)>) {
        let mut m = Marks {
            strings: vec![false; self.strings.len()],
            tables: vec![false; self.tables.len()],
            closures: vec![false; self.closures.len()],
            natives: vec![false; self.natives.len()],
            upvals: vec![false; self.upvals.len()],
            threads: vec![false; self.threads.len()],
        };
        let mut work: Vec<Value> = Vec::with_capacity(64);
        // roots
        work.push(Value::Table(self.globals));
        if let Some(sm) = self.string_meta {
            work.push(Value::Table(sm));
        }
        for mt in self.type_metas.iter().flatten() {
            work.push(Value::Table(*mt));
        }
        work.extend_from_slice(&self.anchors);
        for (&root, &cur) in &self.exec_roots {
            work.push(Value::Thread(ThreadId(root)));
            work.push(Value::Thread(ThreadId(cur)));
        }
        if let Some((etid, eth)) = extra {
            m.threads[etid.0 as usize] = true;
            Self::trace_thread(eth, &mut m, &mut work, &self.upvals);
        }
        while let Some(v) = work.pop() {
            match v {
                Value::Str(s) => m.strings[s.0 as usize] = true,
                Value::Table(t) => {
                    let i = t.0 as usize;
                    if !m.tables[i] {
                        m.tables[i] = true;
                        self.tables[i].trace(|v| work.push(v));
                        if let Some(mt) = self.tables[i].metatable {
                            work.push(Value::Table(mt));
                        }
                    }
                }
                Value::Closure(c) => {
                    let i = c.0 as usize;
                    if !m.closures[i] {
                        m.closures[i] = true;
                        for &uid in &self.closures[i].upvals {
                            mark_upval(uid, &mut m, &mut work, &self.upvals);
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
                        Self::trace_thread(&self.threads[i], &mut m, &mut work, &self.upvals);
                    }
                }
                _ => {}
            }
        }
        self.sweep(&m);
        self.allocs_since_gc = 0;
        self.str_bytes_at_gc = self.strings.bytes();
    }

    fn trace_thread(th: &Thread, m: &mut Marks, work: &mut Vec<Value>, upvals: &[Upval]) {
        for &v in &th.stack {
            work.push(v);
        }
        for f in &th.frames {
            work.push(Value::Closure(f.closure));
            if let Some(h) = f.handler {
                work.push(h);
            }
            for &v in &f.varargs {
                work.push(v);
            }
            for p in &f.pending {
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
                    Pending::CloseTbc { err, .. } => work.push(err),
                    Pending::Concat { .. }
                    | Pending::FinishReturn { .. }
                    | Pending::TailReturn { .. } => {}
                }
            }
        }
        for &(_, uid) in &th.open_upvals {
            mark_upval(uid, m, work, upvals);
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
        self.strings.sweep(&m.strings);
    }
}

fn n_dead(_: &mut Lua, _: &[Value]) -> Result<Vec<Value>, String> {
    Err("attempt to call a collected function".into())
}

struct Marks {
    strings: Vec<bool>,
    tables: Vec<bool>,
    closures: Vec<bool>,
    natives: Vec<bool>,
    upvals: Vec<bool>,
    threads: Vec<bool>,
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
                root_line: 0,
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
                lua.exec_roots.remove(&self.thread.0);
                Ok(Step::Done(vals))
            }
            Ok(RunOutcome::Pending(current)) => {
                self.current = current;
                lua.exec_roots.insert(self.thread.0, current.0);
                self.debt = (-remaining).max(0);
                Ok(Step::Pending)
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
    pub fn abort(mut self, lua: &mut Lua) {
        self.finished = true;
        lua.exec_roots.remove(&self.thread.0);
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// `(chunk name, source line)` of the instruction the execution would
    /// run next, for debuggers and tracers. `None` once it has finished.
    pub fn current_location(&self, lua: &Lua) -> Option<(String, u32)> {
        let th = lua.threads.get(self.current.0 as usize)?;
        let f = th.frames.last()?;
        let line = f.proto.lines.get(f.pc).copied().unwrap_or(0);
        Some((f.proto.source.to_string(), line))
    }
}

// ---- free helpers ----

/// A resolved `debug.getinfo` level: either the running C function (current
/// thread level 0) or a Lua frame's snapshot.
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
fn instr_dst(i: &Instr) -> Option<u8> {
    match *i {
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
        Instr::ForLoop { base, .. } | Instr::TForLoop { base, .. } => Some(base + 3),
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
                if instr_dst(&instr) == Some(reg) {
                    return None;
                }
            }
        }
    }
    None
}

fn kval(th: &Thread, k: u16) -> Value {
    th.frames.last().unwrap().proto.consts[k as usize]
}

fn jump(th: &mut Thread, off: i32) {
    let f = th.frames.last_mut().unwrap();
    f.pc = (f.pc as i64 + off as i64) as usize;
}

fn line_of(th: &Thread) -> u32 {
    let Some(f) = th.frames.last() else { return 0 };
    frame_line(f)
}

/// Source line the frame's next instruction belongs to.
fn frame_line(f: &Frame) -> u32 {
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
    use ArithOp::*;
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
        Add | Sub | Mul => match (na, nb) {
            (Some(Value::Int(x)), Some(Value::Int(y))) => Ok(Value::Int(match op {
                Add => x.wrapping_add(y),
                Sub => x.wrapping_sub(y),
                Mul => x.wrapping_mul(y),
                _ => unreachable!(),
            })),
            _ => {
                let x = as_float(na, a)?;
                let y = as_float(nb, b)?;
                Ok(Value::Float(match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    _ => unreachable!(),
                }))
            }
        },
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
        IDiv => match (na, nb) {
            (Some(Value::Int(x)), Some(Value::Int(y))) => {
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
            }
            _ => {
                let x = as_float(na, a)?;
                let y = as_float(nb, b)?;
                Ok(Value::Float((x / y).floor()))
            }
        },
        Mod => match (na, nb) {
            (Some(Value::Int(x)), Some(Value::Int(y))) => {
                if y == 0 {
                    return Err("attempt to perform 'n%0'".into());
                }
                let r = x.wrapping_rem(y);
                Ok(Value::Int(if r != 0 && (r < 0) != (y < 0) {
                    r + y
                } else {
                    r
                }))
            }
            _ => {
                let x = as_float(na, a)?;
                let y = as_float(nb, b)?;
                let r = x % y;
                Ok(Value::Float(if r != 0.0 && (r < 0.0) != (y < 0.0) {
                    r + y
                } else {
                    r
                }))
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
