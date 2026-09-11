//! AST → bytecode compiler.
//!
//! Register discipline follows PUC Lua: locals occupy fixed registers from
//! the frame base upward; expression temporaries are allocated above the
//! live locals and released after each statement. Call windows (callee +
//! args) are built from consecutive temporaries so the VM can splice frames.

use crate::ast::*;
use crate::bytecode::*;
use crate::value::{Strings, Value};
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct CompileError {
    pub message: String,
    pub line: u32,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "compile error at line {}: {}", self.line, self.message)
    }
}

pub fn compile(
    block: &Block,
    strings: &mut Strings,
    chunk_name: &str,
) -> Result<Rc<Proto>, CompileError> {
    let source: Rc<str> = chunk_name.into();
    let mut c = Compiler { strings, funcs: Vec::new(), source };
    let mut main = FuncState::new(format!("main chunk ({chunk_name})"), 0, true);
    // The main chunk has _ENV as its sole upvalue (Lua 5.4); the host fills
    // it with the globals table when instantiating the chunk.
    main.upvals.push(UpvalDesc::Upval(0));
    main.upval_names.push("_ENV".into());
    c.funcs.push(main);
    c.block_scope(block)?;
    c.check_pending_gotos()?;
    c.emit(Instr::Return { base: 0, n: 1 });
    let fs = c.funcs.pop().unwrap();
    let source = c.source.clone();
    Ok(Rc::new(fs.into_proto(source)))
}

/// Hashable identity of a constant for deduplication.
#[derive(PartialEq, Eq, Hash)]
enum CKey {
    Int(i64),
    Float(u64),
    Str(crate::value::StrId),
}

struct LocalVar {
    name: Box<str>,
    reg: u8,
    captured: bool,
    attrib: Attrib,
}

struct LoopCtx {
    breaks: Vec<usize>,
    /// Register floor at loop entry; `break` closes upvalues from here.
    reg_floor: u8,
}

/// A `goto` whose label hasn't been seen yet. `close_pc` points at a
/// placeholder `Close` (from=255, a no-op) patched when the target's
/// register floor is known.
struct PendingGoto {
    name: Box<str>,
    close_pc: usize,
    jump_pc: usize,
    /// Active-local count at the goto, capped at each enclosing block exit.
    nact: usize,
    line: u32,
}

/// A label visible at the current point.
struct LabelDef {
    name: Box<str>,
    pc: usize,
    reg: u8,
}

struct FuncState {
    code: Vec<Instr>,
    lines: Vec<u32>,
    consts: Vec<Value>,
    const_map: HashMap<CKey, u16>,
    protos: Vec<Rc<Proto>>,
    upvals: Vec<UpvalDesc>,
    upval_names: Vec<Box<str>>,
    locals: Vec<LocalVar>,
    loops: Vec<LoopCtx>,
    gotos: Vec<PendingGoto>,
    labels: Vec<LabelDef>,
    /// Gotos below this index belong to enclosing blocks and cannot see a
    /// label defined in the current statement sequence.
    goto_floor: usize,
    nparams: u8,
    is_vararg: bool,
    free_reg: u8,
    max_regs: u8,
    cur_line: u32,
    name: String,
    /// True once a to-be-closed local is declared in this function; proper
    /// tail calls are disabled while a `__close` handler may be pending
    /// (PUC's `insidetbc` rule).
    has_tbc: bool,
}

impl FuncState {
    fn new(name: String, nparams: u8, is_vararg: bool) -> Self {
        FuncState {
            code: Vec::new(),
            lines: Vec::new(),
            consts: Vec::new(),
            const_map: HashMap::new(),
            protos: Vec::new(),
            upvals: Vec::new(),
            upval_names: Vec::new(),
            locals: Vec::new(),
            loops: Vec::new(),
            gotos: Vec::new(),
            labels: Vec::new(),
            goto_floor: 0,
            nparams,
            is_vararg,
            free_reg: 0,
            max_regs: nparams.max(2),
            cur_line: 0,
            name,
            has_tbc: false,
        }
    }

    fn into_proto(self, source: Rc<str>) -> Proto {
        Proto {
            code: self.code,
            source,
            lines: self.lines,
            consts: self.consts,
            protos: self.protos,
            upvals: self.upvals,
            nparams: self.nparams,
            is_vararg: self.is_vararg,
            max_regs: self.max_regs,
            name: self.name,
        }
    }
}

/// Resolved location of a name.
enum NameLoc {
    Local(u8),
    Upval(u8),
    Global,
}

/// Where the current `_ENV` table can be found.
enum EnvLoc {
    /// A `local _ENV` in scope (use its register directly).
    Local(u8),
    /// The implicit upvalue (load it into a temporary first).
    Upval(u8),
}

/// An assignment target with its prefix expressions already evaluated.
enum Target {
    Local(u8),
    Upval(u8),
    Global(u16),
    Index { obj: u8, key: u8 },
    Field { obj: u8, k: u16 },
}

struct Compiler<'h> {
    strings: &'h mut Strings,
    funcs: Vec<FuncState>,
    source: Rc<str>,
}

fn enc(n: Option<usize>) -> u8 {
    n.map_or(0, |c| c as u8 + 1)
}

impl<'h> Compiler<'h> {
    fn fs(&mut self) -> &mut FuncState {
        self.funcs.last_mut().unwrap()
    }

    fn err<T>(&mut self, message: impl Into<String>) -> Result<T, CompileError> {
        Err(CompileError { message: message.into(), line: self.fs().cur_line })
    }

    fn at_line(&mut self, line: u32) {
        self.fs().cur_line = line;
    }

    fn emit(&mut self, i: Instr) -> usize {
        let fs = self.fs();
        fs.code.push(i);
        fs.lines.push(fs.cur_line);
        fs.code.len() - 1
    }

    /// Emits a placeholder jump-like instruction, to be patched later.
    fn emit_jump(&mut self, i: Instr) -> usize {
        self.emit(i)
    }

    fn here(&mut self) -> usize {
        self.fs().code.len()
    }

    fn patch_to_here(&mut self, idx: usize) {
        let target = self.here();
        let off = target as i32 - (idx as i32 + 1);
        match &mut self.fs().code[idx] {
            Instr::Jump { off: o }
            | Instr::Test { off: o, .. }
            | Instr::ForPrep { off: o, .. } => *o = off,
            other => unreachable!("patching non-jump {other:?}"),
        }
    }

    /// Offset for a backward jump from the *next* emitted instruction to `target`.
    fn back_off(&mut self, target: usize) -> i32 {
        target as i32 - (self.here() as i32 + 1)
    }

    fn alloc_reg(&mut self) -> Result<u8, CompileError> {
        let fs = self.fs();
        if fs.free_reg >= 250 {
            return self.err("function or expression too complex (out of registers)");
        }
        let r = fs.free_reg;
        fs.free_reg += 1;
        if fs.free_reg > fs.max_regs {
            fs.max_regs = fs.free_reg;
        }
        Ok(r)
    }

    /// Allocates `n` consecutive registers, returning the first.
    fn alloc_regs(&mut self, n: usize) -> Result<u8, CompileError> {
        let base = self.fs().free_reg;
        for _ in 0..n {
            self.alloc_reg()?;
        }
        Ok(base)
    }

    fn const_idx(&mut self, key: CKey, v: Value) -> Result<u16, CompileError> {
        if let Some(&i) = self.fs().const_map.get(&key) {
            return Ok(i);
        }
        let fs = self.fs();
        if fs.consts.len() >= u16::MAX as usize {
            return self.err("too many constants in function");
        }
        let i = fs.consts.len() as u16;
        fs.consts.push(v);
        fs.const_map.insert(key, i);
        Ok(i)
    }

    fn str_const(&mut self, s: &[u8]) -> Result<u16, CompileError> {
        // proto constants live outside the GC heap: intern as fixed
        let id = self.strings.intern_fixed(s);
        self.const_idx(CKey::Str(id), Value::Str(id))
    }

    // ---- scopes & locals ----

    fn local_top(&self) -> u8 {
        self.funcs
            .last()
            .unwrap()
            .locals
            .iter()
            .map(|l| l.reg + 1)
            .max()
            .unwrap_or(0)
    }

    fn declare_local(&mut self, name: Box<str>, reg: u8, attrib: Attrib) {
        self.fs().locals.push(LocalVar { name, reg, captured: false, attrib });
    }

    fn enter_scope(&mut self) -> usize {
        self.funcs.last().unwrap().locals.len()
    }

    /// Pops locals down to `floor`, emitting `Close` if any was captured,
    /// and releases their registers.
    fn exit_scope(&mut self, floor: usize) {
        let fs = self.funcs.last_mut().unwrap();
        let mut close_from: Option<u8> = None;
        while fs.locals.len() > floor {
            let l = fs.locals.pop().unwrap();
            if l.captured || l.attrib == Attrib::Close {
                close_from = Some(close_from.map_or(l.reg, |c| c.min(l.reg)));
            }
        }
        if let Some(from) = close_from {
            self.emit(Instr::Close { from });
        }
        let top = self.local_top();
        self.fs().free_reg = top;
    }

    fn block_scope(&mut self, b: &Block) -> Result<(), CompileError> {
        let floor = self.enter_scope();
        self.stmt_seq(&b.stmts)?;
        self.exit_scope(floor);
        Ok(())
    }

    /// Compiles a statement sequence, handling label visibility: a label is
    /// "at the end of the block" (and may be jumped to over the block's
    /// locals) when only other labels follow it.
    fn stmt_seq(&mut self, stmts: &[Stmt]) -> Result<(), CompileError> {
        let nact_entry = self.funcs.last().unwrap().locals.len();
        let labels_floor = self.funcs.last().unwrap().labels.len();
        let gotos_floor = self.funcs.last().unwrap().gotos.len();
        let outer_goto_floor = self.funcs.last().unwrap().goto_floor;
        self.funcs.last_mut().unwrap().goto_floor = gotos_floor;
        for (i, s) in stmts.iter().enumerate() {
            if let Stmt::Label(name) = s {
                let last = stmts[i + 1..].iter().all(|s| matches!(s, Stmt::Label(_)));
                self.define_label(name, last, nact_entry)?;
            } else {
                self.stmt(s)?;
            }
        }
        self.funcs.last_mut().unwrap().goto_floor = outer_goto_floor;
        // leaving the block: labels go out of scope; unmatched gotos float
        // up with their local count capped at this block's entry level
        let fs = self.funcs.last_mut().unwrap();
        fs.labels.truncate(labels_floor);
        let start = gotos_floor.min(fs.gotos.len());
        for g in &mut fs.gotos[start..] {
            g.nact = g.nact.min(nact_entry);
        }
        Ok(())
    }

    fn define_label(
        &mut self,
        name: &str,
        last_in_block: bool,
        block_nact: usize,
    ) -> Result<(), CompileError> {
        if self.funcs.last().unwrap().labels.iter().any(|l| &*l.name == name) {
            return self.err(format!("label '{name}' already defined"));
        }
        let fs = self.funcs.last().unwrap();
        let nact = if last_in_block { block_nact } else { fs.locals.len() };
        let reg = fs.locals[..nact].iter().map(|l| l.reg + 1).max().unwrap_or(0);
        let floor = fs.goto_floor;
        let pc = self.here();
        // resolve pending gotos targeting this label; only gotos opened in
        // this block can see it (labels are not visible to enclosing blocks)
        let mut i = floor;
        while i < self.funcs.last().unwrap().gotos.len() {
            let g = &self.funcs.last().unwrap().gotos[i];
            if &*g.name != name {
                i += 1;
                continue;
            }
            if g.nact < nact {
                let line = g.line;
                self.fs().cur_line = line;
                return self.err(format!(
                    "<goto {name}> jumps into the scope of a local"
                ));
            }
            let g = self.funcs.last_mut().unwrap().gotos.remove(i);
            if nact < g.nact {
                // jumping out of local scopes: close their upvalues
                if let Instr::Close { from } =
                    &mut self.funcs.last_mut().unwrap().code[g.close_pc]
                {
                    *from = reg;
                }
            }
            let off = pc as i32 - (g.jump_pc as i32 + 1);
            if let Instr::Jump { off: o } = &mut self.funcs.last_mut().unwrap().code[g.jump_pc] {
                *o = off;
            }
        }
        self.funcs.last_mut().unwrap().labels.push(LabelDef { name: name.into(), pc, reg });
        Ok(())
    }

    fn check_pending_gotos(&mut self) -> Result<(), CompileError> {
        if let Some(g) = self.funcs.last().unwrap().gotos.first() {
            let (name, line) = (g.name.clone(), g.line);
            self.fs().cur_line = line;
            return self.err(format!("no visible label '{name}' for goto"));
        }
        Ok(())
    }

    // ---- name resolution ----

    fn resolve(&mut self, name: &str) -> NameLoc {
        self.resolve_at(self.funcs.len() - 1, name)
    }

    fn resolve_at(&mut self, level: usize, name: &str) -> NameLoc {
        if let Some(i) = self.funcs[level]
            .locals
            .iter()
            .rposition(|l| &*l.name == name)
        {
            return NameLoc::Local(self.funcs[level].locals[i].reg);
        }
        if let Some(i) = self.funcs[level]
            .upval_names
            .iter()
            .position(|n| &**n == name)
        {
            return NameLoc::Upval(i as u8);
        }
        if level == 0 {
            return NameLoc::Global;
        }
        // resolve in the enclosing function, then capture
        let desc = if let Some(i) = self.funcs[level - 1]
            .locals
            .iter()
            .rposition(|l| &*l.name == name)
        {
            self.funcs[level - 1].locals[i].captured = true;
            UpvalDesc::Local(self.funcs[level - 1].locals[i].reg)
        } else {
            match self.resolve_at(level - 1, name) {
                NameLoc::Local(_) => unreachable!("handled above"),
                NameLoc::Upval(i) => UpvalDesc::Upval(i),
                NameLoc::Global => return NameLoc::Global,
            }
        };
        let fs = &mut self.funcs[level];
        fs.upvals.push(desc);
        fs.upval_names.push(name.into());
        NameLoc::Upval(fs.upvals.len() as u8 - 1)
    }

    // ---- statements ----

    fn stmt(&mut self, s: &Stmt) -> Result<(), CompileError> {
        let watermark = self.local_top();
        match s {
            Stmt::Empty => {}
            Stmt::Label(name) => {
                // labels are normally handled by stmt_seq (which knows
                // block-end position); a stray one is not last-in-block
                let nact = self.funcs.last().unwrap().locals.len();
                self.define_label(name, false, nact)?;
            }
            Stmt::Goto { label, line } => {
                self.at_line(*line);
                let fs = self.funcs.last().unwrap();
                // backward goto: label already visible (innermost match)
                if let Some(l) = fs.labels.iter().rev().find(|l| l.name == *label) {
                    let (pc, reg) = (l.pc, l.reg);
                    self.emit(Instr::Close { from: reg });
                    let off = self.back_off(pc);
                    self.emit(Instr::Jump { off });
                } else {
                    // forward goto: placeholder Close (no-op until patched)
                    let close_pc = self.emit(Instr::Close { from: 255 });
                    let jump_pc = self.emit(Instr::Jump { off: 0 });
                    let nact = self.funcs.last().unwrap().locals.len();
                    self.funcs.last_mut().unwrap().gotos.push(PendingGoto {
                        name: label.clone(),
                        close_pc,
                        jump_pc,
                        nact,
                        line: *line,
                    });
                }
            }
            Stmt::Local { names, values, line } => {
                self.at_line(*line);
                if names.iter().filter(|(_, a)| *a == Attrib::Close).count() > 1 {
                    return self.err("multiple to-be-closed variables in local list");
                }
                let base = self.fs().free_reg;
                self.explist_to(values, names.len())?;
                for (i, (name, attrib)) in names.iter().enumerate() {
                    self.declare_local(name.clone(), base + i as u8, *attrib);
                    if *attrib == Attrib::Close {
                        self.fs().has_tbc = true;
                        self.emit(Instr::Tbc { reg: base + i as u8 });
                    }
                }
                // locals stay allocated
                self.fs().free_reg = base + names.len() as u8;
                return Ok(());
            }
            Stmt::LocalFunction { name, body } => {
                self.at_line(body.line);
                let reg = self.alloc_reg()?;
                self.declare_local(name.clone(), reg, Attrib::None);
                self.function_to_reg(body, format!("function '{name}'"), reg)?;
                return Ok(());
            }
            Stmt::Function { target, body } => {
                self.at_line(body.line);
                let tmp = self.alloc_reg()?;
                self.function_to_reg(body, "function".to_string(), tmp)?;
                let t = self.target_of(target)?;
                self.store(t, tmp)?;
            }
            Stmt::Assign { targets, values, line } => {
                self.at_line(*line);
                let resolved: Vec<Target> = targets
                    .iter()
                    .map(|t| self.target_of(t))
                    .collect::<Result<_, _>>()?;
                let base = self.fs().free_reg;
                self.explist_to(values, targets.len())?;
                for (i, t) in resolved.into_iter().enumerate() {
                    self.store(t, base + i as u8)?;
                }
            }
            Stmt::ExprStat(e) => {
                self.call_like(e, Some(0))?;
            }
            Stmt::Do(b) => self.block_scope(b)?,
            Stmt::While { cond, body } => {
                let top = self.here();
                let r = self.expr_to_any(cond)?;
                let exit = self.emit_jump(Instr::Test { src: r, if_true: false, off: 0 });
                self.fs().free_reg = self.local_top();
                let floor = self.fs().free_reg;
                self.funcs
                    .last_mut()
                    .unwrap()
                    .loops
                    .push(LoopCtx { breaks: Vec::new(), reg_floor: floor });
                self.block_scope(body)?;
                let off = self.back_off(top);
                self.emit(Instr::Jump { off });
                self.patch_to_here(exit);
                self.finish_loop();
            }
            Stmt::Repeat { body, cond } => {
                let top = self.here();
                let floor = self.enter_scope();
                let reg_floor = self.fs().free_reg;
                self.funcs
                    .last_mut()
                    .unwrap()
                    .loops
                    .push(LoopCtx { breaks: Vec::new(), reg_floor });
                self.stmt_seq(&body.stmts)?;
                // condition sees the body's locals (Lua scoping rule)
                let r = self.expr_to_any(cond)?;
                let exit = self.emit_jump(Instr::Test { src: r, if_true: true, off: 0 });
                self.emit(Instr::Close { from: reg_floor });
                let off = self.back_off(top);
                self.emit(Instr::Jump { off });
                self.patch_to_here(exit);
                self.exit_scope(floor);
                self.finish_loop();
            }
            Stmt::If { arms, else_block } => {
                let mut end_jumps = Vec::new();
                for (i, (cond, body)) in arms.iter().enumerate() {
                    let r = self.expr_to_any(cond)?;
                    let skip = self.emit_jump(Instr::Test { src: r, if_true: false, off: 0 });
                    self.fs().free_reg = self.local_top();
                    self.block_scope(body)?;
                    let is_last_arm = i + 1 == arms.len() && else_block.is_none();
                    if !is_last_arm {
                        end_jumps.push(self.emit_jump(Instr::Jump { off: 0 }));
                    }
                    self.patch_to_here(skip);
                }
                if let Some(b) = else_block {
                    self.block_scope(b)?;
                }
                for j in end_jumps {
                    self.patch_to_here(j);
                }
            }
            Stmt::NumericFor { var, start, end, step, body, line } => {
                self.at_line(*line);
                let floor = self.enter_scope();
                let base = self.fs().free_reg;
                let r0 = self.alloc_reg()?;
                self.expr_to_reg(start, r0)?;
                let r1 = self.alloc_reg()?;
                self.expr_to_reg(end, r1)?;
                let r2 = self.alloc_reg()?;
                match step {
                    Some(e) => self.expr_to_reg(e, r2)?,
                    None => {
                        let k = self.const_idx(CKey::Int(1), Value::Int(1))?;
                        self.emit(Instr::LoadK { dst: r2, k });
                    }
                }
                // hidden control registers are pseudo-locals (names can't collide)
                self.declare_local("(for state)".into(), r0, Attrib::None);
                self.declare_local("(for state)".into(), r1, Attrib::None);
                self.declare_local("(for state)".into(), r2, Attrib::None);
                let var_reg = self.alloc_reg()?;
                self.declare_local(var.clone(), var_reg, Attrib::None);
                let prep = self.emit_jump(Instr::ForPrep { base, off: 0 });
                let body_top = self.here();
                self.funcs
                    .last_mut()
                    .unwrap()
                    .loops
                    .push(LoopCtx { breaks: Vec::new(), reg_floor: base });
                self.block_scope(body)?;
                if self.var_captured(var_reg) {
                    self.emit(Instr::Close { from: var_reg });
                }
                let off = self.back_off(body_top);
                self.emit(Instr::ForLoop { base, off });
                self.patch_to_here(prep);
                self.finish_loop();
                self.exit_scope(floor);
            }
            Stmt::GenericFor { vars, exprs, body, line } => {
                self.at_line(*line);
                let floor = self.enter_scope();
                let base = self.fs().free_reg;
                self.explist_to(exprs, 3)?;
                self.declare_local("(for state)".into(), base, Attrib::None);
                self.declare_local("(for state)".into(), base + 1, Attrib::None);
                self.declare_local("(for state)".into(), base + 2, Attrib::None);
                let vars_base = self.fs().free_reg;
                debug_assert_eq!(vars_base, base + 3);
                for v in vars {
                    let r = self.alloc_reg()?;
                    self.declare_local(v.clone(), r, Attrib::None);
                }
                let to_call = self.emit_jump(Instr::Jump { off: 0 });
                let body_top = self.here();
                self.funcs
                    .last_mut()
                    .unwrap()
                    .loops
                    .push(LoopCtx { breaks: Vec::new(), reg_floor: base });
                self.block_scope(body)?;
                let captured = (0..vars.len()).any(|i| self.var_captured(vars_base + i as u8));
                if captured {
                    self.emit(Instr::Close { from: vars_base });
                }
                self.patch_to_here(to_call);
                // call site: iterator(state, control) -> vars
                let nvars = vars.len();
                let save = self.fs().free_reg;
                let tmp = self.alloc_regs(3)?;
                self.emit(Instr::Move { dst: tmp, src: base });
                self.emit(Instr::Move { dst: tmp + 1, src: base + 1 });
                self.emit(Instr::Move { dst: tmp + 2, src: base + 2 });
                self.emit(Instr::Call { base: tmp, nargs: 3, nres: nvars as u8 + 1 });
                for i in 0..nvars {
                    self.emit(Instr::Move { dst: vars_base + i as u8, src: tmp + i as u8 });
                }
                let off = self.back_off(body_top);
                self.emit(Instr::TForLoop { base, off });
                self.fs().free_reg = save;
                self.finish_loop();
                self.exit_scope(floor);
            }
            Stmt::Return { exprs, line } => {
                self.at_line(*line);
                // `return f(...)` in a function with no pending to-be-closed
                // variables is a proper tail call: no frame is left behind.
                if exprs.len() == 1
                    && !self.fs().has_tbc
                    && matches!(exprs[0], Expr::Call { .. } | Expr::MethodCall { .. })
                {
                    self.tail_call_like(&exprs[0])?;
                    return Ok(());
                }
                let (base, count) = self.explist_open(exprs)?;
                self.emit(Instr::Return { base, n: enc(count) });
            }
            Stmt::Break(line) => {
                self.at_line(*line);
                if self.funcs.last().unwrap().loops.is_empty() {
                    return self.err("break outside a loop");
                }
                let floor = self.funcs.last().unwrap().loops.last().unwrap().reg_floor;
                self.emit(Instr::Close { from: floor });
                let j = self.emit_jump(Instr::Jump { off: 0 });
                self.funcs
                    .last_mut()
                    .unwrap()
                    .loops
                    .last_mut()
                    .unwrap()
                    .breaks
                    .push(j);
            }
        }
        self.fs().free_reg = watermark.max(self.local_top());
        Ok(())
    }

    fn var_captured(&self, reg: u8) -> bool {
        self.funcs
            .last()
            .unwrap()
            .locals
            .iter()
            .any(|l| l.reg == reg && l.captured)
    }

    fn finish_loop(&mut self) {
        let ctx = self.funcs.last_mut().unwrap().loops.pop().unwrap();
        for j in ctx.breaks {
            self.patch_to_here(j);
        }
    }

    /// Resolves an assignment target, evaluating prefix expressions to temps.
    fn target_of(&mut self, e: &Expr) -> Result<Target, CompileError> {
        match e {
            Expr::Name(n, line) => {
                self.at_line(*line);
                match self.resolve(n) {
                    NameLoc::Local(reg) => {
                        let is_const = self
                            .funcs
                            .last()
                            .unwrap()
                            .locals
                            .iter()
                            .rev()
                            .find(|l| l.reg == reg)
                            .is_some_and(|l| l.attrib == Attrib::Const);
                        if is_const {
                            return self
                                .err(format!("attempt to assign to const variable '{n}'"));
                        }
                        Ok(Target::Local(reg))
                    }
                    NameLoc::Upval(i) => Ok(Target::Upval(i)),
                    NameLoc::Global => Ok(Target::Global(self.str_const(n.as_bytes())?)),
                }
            }
            Expr::Index { obj, key, line } => {
                self.at_line(*line);
                let o = self.expr_to_any(obj)?;
                if let Expr::Str(s) = &**key {
                    let k = self.str_const(s)?;
                    Ok(Target::Field { obj: o, k })
                } else {
                    let k = self.expr_to_any(key)?;
                    Ok(Target::Index { obj: o, key: k })
                }
            }
            _ => self.err("cannot assign to this expression"),
        }
    }

    fn store(&mut self, t: Target, src: u8) -> Result<(), CompileError> {
        match t {
            Target::Local(reg) => {
                if reg != src {
                    self.emit(Instr::Move { dst: reg, src });
                }
            }
            Target::Upval(up) => {
                self.emit(Instr::SetUpval { up, src });
            }
            Target::Global(k) => {
                match self.env_loc() {
                    EnvLoc::Local(r) => {
                        self.emit(Instr::SetField { obj: r, k, src });
                    }
                    EnvLoc::Upval(up) => {
                        let tmp = self.alloc_reg()?;
                        self.emit(Instr::GetUpval { dst: tmp, up });
                        self.emit(Instr::SetField { obj: tmp, k, src });
                        self.fs().free_reg -= 1;
                    }
                }
            }
            Target::Index { obj, key } => {
                self.emit(Instr::SetIndex { obj, key, src });
            }
            Target::Field { obj, k } => {
                self.emit(Instr::SetField { obj, k, src });
            }
        }
        Ok(())
    }

    /// Where the current environment table lives: a local `_ENV` (Lua 5.4
    /// allows `local _ENV = ...`) or the implicit `_ENV` upvalue.
    fn env_loc(&mut self) -> EnvLoc {
        match self.resolve("_ENV") {
            NameLoc::Local(r) => EnvLoc::Local(r),
            NameLoc::Upval(i) => EnvLoc::Upval(i),
            NameLoc::Global => unreachable!("_ENV always resolves"),
        }
    }

    // ---- expression lists ----

    /// Compiles `exprs` adjusted to exactly `want` values in `want` freshly
    /// allocated consecutive registers starting at the current free register.
    fn explist_to(&mut self, exprs: &[Expr], want: usize) -> Result<(), CompileError> {
        let base = self.fs().free_reg;
        if exprs.is_empty() {
            if want > 0 {
                self.alloc_regs(want)?;
                self.emit(Instr::LoadNil { dst: base, n: want as u8 });
            }
            return Ok(());
        }
        for (i, e) in exprs.iter().enumerate() {
            let last = i + 1 == exprs.len();
            if !last || i >= want {
                // middle expressions, and extra expressions beyond `want`,
                // are evaluated for side effects (one value, maybe discarded)
                let save = self.fs().free_reg;
                let r = self.alloc_reg()?;
                self.expr_to_reg(e, r)?;
                if i >= want {
                    self.fs().free_reg = save;
                }
            } else {
                let have = i; // values produced so far
                let need = want - have;
                if e.is_multret() {
                    self.multret_tail(e, Some(need))?;
                } else {
                    let r = self.alloc_reg()?;
                    self.expr_to_reg(e, r)?;
                    if need > 1 {
                        let pad = self.alloc_regs(need - 1)?;
                        self.emit(Instr::LoadNil { dst: pad, n: (need - 1) as u8 });
                    }
                }
            }
        }
        debug_assert!(self.fs().free_reg >= base + want.min(exprs.len()) as u8);
        Ok(())
    }

    /// Compiles `exprs` for an open (multret) context: call arguments or
    /// return values. Returns (base, Some(count)) for a fixed number of
    /// values, or (base, None) when the last expression expands to top.
    fn explist_open(&mut self, exprs: &[Expr]) -> Result<(u8, Option<usize>), CompileError> {
        let base = self.fs().free_reg;
        if exprs.is_empty() {
            return Ok((base, Some(0)));
        }
        for (i, e) in exprs.iter().enumerate() {
            let last = i + 1 == exprs.len();
            if last && e.is_multret() {
                self.multret_tail(e, None)?;
                return Ok((base, None));
            }
            let r = self.alloc_reg()?;
            self.expr_to_reg(e, r)?;
        }
        Ok((base, Some(exprs.len())))
    }

    /// Compiles a multret-capable expression (call/method call/vararg) at the
    /// current free register with `nres` results (None = up to top).
    /// Allocates the result registers when `nres` is fixed.
    fn multret_tail(&mut self, e: &Expr, nres: Option<usize>) -> Result<u8, CompileError> {
        match e {
            Expr::Call { .. } | Expr::MethodCall { .. } => self.call_like(e, nres),
            Expr::Vararg(line) => {
                self.at_line(*line);
                if !self.funcs.last().unwrap().is_vararg {
                    return self.err("cannot use '...' outside a vararg function");
                }
                let base = self.alloc_regs(nres.unwrap_or(1).max(1))?;
                if nres.is_none() {
                    self.fs().free_reg = base; // results tracked via top
                }
                self.emit(Instr::Vararg { dst: base, n: enc(nres) });
                Ok(base)
            }
            _ => unreachable!("multret_tail on non-multret expression"),
        }
    }

    /// Compiles a call or method call. Results land at the returned base
    /// register; for fixed `nres` the result registers stay allocated.
    fn call_like(&mut self, e: &Expr, nres: Option<usize>) -> Result<u8, CompileError> {
        let base = self.alloc_reg()?;
        match e {
            Expr::Call { func, args, line } => {
                self.at_line(*line);
                self.expr_to_reg(func, base)?;
                self.fs().free_reg = base + 1;
                let argc = self.compile_args(args)?;
                self.at_line(*line);
                self.emit(Instr::Call {
                    base,
                    nargs: argc.map_or(0, |c| c as u8 + 1),
                    nres: enc(nres),
                });
            }
            Expr::MethodCall { obj, name, args, line } => {
                self.at_line(*line);
                let selfr = self.alloc_reg()?; // base + 1
                self.expr_to_reg(obj, selfr)?;
                self.fs().free_reg = base + 2;
                let k = self.str_const(name.as_bytes())?;
                self.emit(Instr::GetField { dst: base, obj: selfr, k });
                let argc = self.compile_args(args)?;
                self.at_line(*line);
                self.emit(Instr::Call {
                    base,
                    nargs: argc.map_or(0, |c| c as u8 + 2),
                    nres: enc(nres),
                });
            }
            _ => unreachable!("call_like on non-call"),
        }
        // release the call window, keep fixed results
        self.fs().free_reg = base + nres.unwrap_or(0) as u8;
        let watermark = self.local_top();
        if self.fs().free_reg < watermark {
            self.fs().free_reg = watermark;
        }
        if let Some(n) = nres {
            let fs = self.fs();
            let end = base as usize + n;
            if end > fs.max_regs as usize {
                fs.max_regs = end as u8;
            }
        }
        Ok(base)
    }

    /// Compiles `return <call>` as a proper tail call. Mirrors `call_like`
    /// but emits `TailCall` (open results) so the VM reuses the frame.
    fn tail_call_like(&mut self, e: &Expr) -> Result<(), CompileError> {
        let base = self.alloc_reg()?;
        match e {
            Expr::Call { func, args, line } => {
                self.at_line(*line);
                self.expr_to_reg(func, base)?;
                self.fs().free_reg = base + 1;
                let argc = self.compile_args(args)?;
                self.at_line(*line);
                self.emit(Instr::TailCall {
                    base,
                    nargs: argc.map_or(0, |c| c as u8 + 1),
                });
            }
            Expr::MethodCall { obj, name, args, line } => {
                self.at_line(*line);
                let selfr = self.alloc_reg()?; // base + 1
                self.expr_to_reg(obj, selfr)?;
                self.fs().free_reg = base + 2;
                let k = self.str_const(name.as_bytes())?;
                self.emit(Instr::GetField { dst: base, obj: selfr, k });
                let argc = self.compile_args(args)?;
                self.at_line(*line);
                self.emit(Instr::TailCall {
                    base,
                    nargs: argc.map_or(0, |c| c as u8 + 2),
                });
            }
            _ => unreachable!("tail_call_like on non-call"),
        }
        Ok(())
    }

    /// Compiles call arguments into consecutive registers above the current
    /// free register. Returns Some(count) or None for an open tail.
    fn compile_args(&mut self, args: &[Expr]) -> Result<Option<usize>, CompileError> {
        for (i, a) in args.iter().enumerate() {
            let last = i + 1 == args.len();
            if last && a.is_multret() {
                self.multret_tail(a, None)?;
                return Ok(None);
            }
            let r = self.alloc_reg()?;
            self.expr_to_reg(a, r)?;
        }
        Ok(Some(args.len()))
    }

    // ---- expressions ----

    /// Returns a register holding the expression's value: the variable's own
    /// register for plain local names, otherwise a fresh temporary.
    fn expr_to_any(&mut self, e: &Expr) -> Result<u8, CompileError> {
        if let Expr::Name(n, line) = e {
            self.at_line(*line);
            if let NameLoc::Local(r) = self.resolve(n) {
                return Ok(r);
            }
        }
        let r = self.alloc_reg()?;
        self.expr_to_reg(e, r)?;
        Ok(r)
    }

    fn expr_to_reg(&mut self, e: &Expr, dst: u8) -> Result<(), CompileError> {
        match e {
            Expr::Nil => {
                self.emit(Instr::LoadNil { dst, n: 1 });
            }
            Expr::True => {
                self.emit(Instr::LoadBool { dst, b: true });
            }
            Expr::False => {
                self.emit(Instr::LoadBool { dst, b: false });
            }
            Expr::Int(i) => {
                let k = self.const_idx(CKey::Int(*i), Value::Int(*i))?;
                self.emit(Instr::LoadK { dst, k });
            }
            Expr::Float(f) => {
                let k = self.const_idx(CKey::Float(f.to_bits()), Value::Float(*f))?;
                self.emit(Instr::LoadK { dst, k });
            }
            Expr::Str(s) => {
                let k = self.str_const(s)?;
                self.emit(Instr::LoadK { dst, k });
            }
            Expr::Vararg(line) => {
                self.at_line(*line);
                if !self.funcs.last().unwrap().is_vararg {
                    return self.err("cannot use '...' outside a vararg function");
                }
                self.emit(Instr::Vararg { dst, n: 2 });
            }
            Expr::Function(fb) => {
                self.function_to_reg(fb, "anonymous function".into(), dst)?;
            }
            Expr::Name(n, line) => {
                self.at_line(*line);
                match self.resolve(n) {
                    NameLoc::Local(r) => {
                        if r != dst {
                            self.emit(Instr::Move { dst, src: r });
                        }
                    }
                    NameLoc::Upval(up) => {
                        self.emit(Instr::GetUpval { dst, up });
                    }
                    NameLoc::Global => {
                        let k = self.str_const(n.as_bytes())?;
                        match self.env_loc() {
                            EnvLoc::Local(r) => {
                                self.emit(Instr::GetField { dst, obj: r, k });
                            }
                            EnvLoc::Upval(up) => {
                                self.emit(Instr::GetUpval { dst, up });
                                self.emit(Instr::GetField { dst, obj: dst, k });
                            }
                        }
                    }
                }
            }
            Expr::Index { obj, key, line } => {
                self.at_line(*line);
                let save = self.fs().free_reg;
                let o = self.expr_to_any(obj)?;
                if let Expr::Str(s) = &**key {
                    let k = self.str_const(s)?;
                    self.emit(Instr::GetField { dst, obj: o, k });
                } else {
                    let kr = self.expr_to_any(key)?;
                    self.emit(Instr::GetIndex { dst, obj: o, key: kr });
                }
                self.fs().free_reg = save;
            }
            Expr::Call { .. } | Expr::MethodCall { .. } => {
                let save = self.fs().free_reg;
                let base = self.call_like(e, Some(1))?;
                self.fs().free_reg = save;
                if base != dst {
                    self.emit(Instr::Move { dst, src: base });
                }
            }
            Expr::BinOp { op, lhs, rhs, line } => {
                self.at_line(*line);
                self.binop_to_reg(*op, lhs, rhs, dst, *line)?;
            }
            Expr::UnOp { op, operand, line } => {
                self.at_line(*line);
                let save = self.fs().free_reg;
                let src = self.expr_to_any(operand)?;
                let op = match op {
                    UnOp::Neg => UnaryOp::Neg,
                    UnOp::Not => UnaryOp::Not,
                    UnOp::Len => UnaryOp::Len,
                    UnOp::BNot => UnaryOp::BNot,
                };
                self.emit(Instr::Unary { op, dst, src });
                self.fs().free_reg = save;
            }
            Expr::Table { items, pairs, line } => {
                self.at_line(*line);
                self.emit(Instr::NewTable { dst });
                let save = self.fs().free_reg;
                // array items, flushed in batches of 50
                let mut start: u32 = 1;
                let mut pending: usize = 0;
                let mut batch_base = self.fs().free_reg;
                for (i, item) in items.iter().enumerate() {
                    let last = i + 1 == items.len();
                    if last && item.is_multret() {
                        self.multret_tail(item, None)?;
                        self.emit(Instr::SetList { obj: dst, base: batch_base, n: 0, start });
                        pending = 0;
                        break;
                    }
                    let r = self.alloc_reg()?;
                    self.expr_to_reg(item, r)?;
                    pending += 1;
                    if pending == 50 {
                        self.emit(Instr::SetList {
                            obj: dst,
                            base: batch_base,
                            n: pending as u8,
                            start,
                        });
                        start += pending as u32;
                        pending = 0;
                        self.fs().free_reg = save;
                        batch_base = save;
                    }
                }
                if pending > 0 {
                    self.emit(Instr::SetList {
                        obj: dst,
                        base: batch_base,
                        n: pending as u8,
                        start,
                    });
                }
                self.fs().free_reg = save;
                for (k, v) in pairs {
                    let save = self.fs().free_reg;
                    if let Expr::Str(s) = k {
                        let kc = self.str_const(s)?;
                        let vr = self.expr_to_any(v)?;
                        self.emit(Instr::SetField { obj: dst, k: kc, src: vr });
                    } else {
                        let kr = self.expr_to_any(k)?;
                        let vr = self.expr_to_any(v)?;
                        self.emit(Instr::SetIndex { obj: dst, key: kr, src: vr });
                    }
                    self.fs().free_reg = save;
                }
            }
            Expr::Paren(inner) => {
                self.expr_to_reg(inner, dst)?;
            }
        }
        Ok(())
    }

    fn binop_to_reg(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        dst: u8,
        line: u32,
    ) -> Result<(), CompileError> {
        match op {
            BinOp::And | BinOp::Or => {
                self.expr_to_reg(lhs, dst)?;
                let j = self.emit_jump(Instr::Test {
                    src: dst,
                    if_true: op == BinOp::Or,
                    off: 0,
                });
                self.expr_to_reg(rhs, dst)?;
                self.patch_to_here(j);
            }
            BinOp::Concat => {
                // flatten the right-leaning concat chain into one Concat op
                let save = self.fs().free_reg;
                let mut parts: Vec<&Expr> = vec![lhs];
                let mut cur = rhs;
                while let Expr::BinOp { op: BinOp::Concat, lhs, rhs, .. } = cur {
                    parts.push(lhs);
                    cur = rhs;
                }
                parts.push(cur);
                let base = self.fs().free_reg;
                for p in &parts {
                    let r = self.alloc_reg()?;
                    self.expr_to_reg(p, r)?;
                }
                self.at_line(line);
                self.emit(Instr::Concat { dst, base, n: parts.len() as u8 });
                self.fs().free_reg = save;
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let save = self.fs().free_reg;
                let (cmp, l, r) = match op {
                    BinOp::Eq => (CmpOp::Eq, lhs, rhs),
                    BinOp::Ne => (CmpOp::Ne, lhs, rhs),
                    BinOp::Lt => (CmpOp::Lt, lhs, rhs),
                    BinOp::Le => (CmpOp::Le, lhs, rhs),
                    // a > b  ==>  b < a ; a >= b  ==>  b <= a
                    BinOp::Gt => (CmpOp::Lt, rhs, lhs),
                    BinOp::Ge => (CmpOp::Le, rhs, lhs),
                    _ => unreachable!(),
                };
                let lr = self.expr_to_any(l)?;
                let rr = self.expr_to_any(r)?;
                self.at_line(line);
                self.emit(Instr::Cmp { op: cmp, dst, lhs: lr, rhs: rr });
                self.fs().free_reg = save;
            }
            _ => {
                let save = self.fs().free_reg;
                let aop = match op {
                    BinOp::Add => ArithOp::Add,
                    BinOp::Sub => ArithOp::Sub,
                    BinOp::Mul => ArithOp::Mul,
                    BinOp::Div => ArithOp::Div,
                    BinOp::IDiv => ArithOp::IDiv,
                    BinOp::Mod => ArithOp::Mod,
                    BinOp::Pow => ArithOp::Pow,
                    BinOp::BAnd => ArithOp::BAnd,
                    BinOp::BOr => ArithOp::BOr,
                    BinOp::BXor => ArithOp::BXor,
                    BinOp::Shl => ArithOp::Shl,
                    BinOp::Shr => ArithOp::Shr,
                    _ => unreachable!(),
                };
                let lr = self.expr_to_any(lhs)?;
                let rr = self.expr_to_any(rhs)?;
                self.at_line(line);
                self.emit(Instr::Arith { op: aop, dst, lhs: lr, rhs: rr });
                self.fs().free_reg = save;
            }
        }
        Ok(())
    }

    fn function_to_reg(
        &mut self,
        fb: &FuncBody,
        name: String,
        dst: u8,
    ) -> Result<(), CompileError> {
        if fb.params.len() > 200 {
            return self.err("too many parameters");
        }
        let mut fs = FuncState::new(name, fb.params.len() as u8, fb.is_vararg);
        fs.cur_line = fb.line;
        self.funcs.push(fs);
        for p in &fb.params {
            let r = self.alloc_reg()?;
            self.declare_local(p.clone(), r, Attrib::None);
        }
        self.block_scope(&fb.body)?;
        self.check_pending_gotos()?;
        self.emit(Instr::Return { base: 0, n: 1 });
        let done = self.funcs.pop().unwrap();
        let proto = Rc::new(done.into_proto(self.source.clone()));
        let fs = self.fs();
        if fs.protos.len() >= u16::MAX as usize {
            return self.err("too many nested functions");
        }
        let p = fs.protos.len() as u16;
        fs.protos.push(proto);
        self.emit(Instr::Closure { dst, p });
        Ok(())
    }
}
