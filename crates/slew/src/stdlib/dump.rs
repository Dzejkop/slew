//! `string.dump` and binary-chunk decoding for `load`.
//!
//! slew's bytecode is register-based but does not share PUC's opcode or
//! constant encoding, so the payload after the header is slew-specific.
//! The *header* is byte-for-byte the one PUC 5.4 writes, followed by PUC's
//! `LUAC_INT`/`LUAC_NUM` sentinels, so the observable parts of the binary
//! format (signature, sizes, sentinels, mode enforcement, truncation and
//! corruption detection) match upstream even though the body is not
//! portable to PUC's `luac`.
//!
//! Layout:
//! ```text
//! "\x1bLua" 0x54 0x00 "\x19\x93\r\n\x1a\n" 0x04 0x08 0x08   // PUC header
//! i64 0x5678  f64 370.5                                      // sentinels (LE)
//! "SLW1"                                                     // slew payload magic
//! <proto>                                                     // recursive encoding
//! ```
//! Every out-of-bounds read reports "truncated" so `load` errors match the
//! upstream suite.

use std::rc::Rc;

use crate::bytecode::{
    ArithOp, CmpOp, Instr, MAX_REGS, Proto, UNPATCHED_CLOSE, UnaryOp, UpvalDesc,
};
use crate::value::Value;
use crate::vm::Lua;

use super::arg;

/// PUC 5.4 binary signature plus version/format/data/size bytes. The sizes
/// are 4 (instruction), 8 (integer), 8 (number) on slew's targets.
const HEADER: &[u8] = b"\x1bLua\x54\x00\x19\x93\r\n\x1a\n\x04\x08\x08";
/// `LUAC_INT`/`LUAC_NUM` sentinels that PUC writes after the header.
const SENTINEL_INT: i64 = 0x5678;
const SENTINEL_NUM: f64 = 370.5;
/// Marks slew's own payload so a PUC-format body is rejected clearly.
const PAYLOAD_MAGIC: &[u8] = b"SLW1";
const VERSION: u8 = 1;
/// Maximum nesting of sub-protos accepted from a binary chunk: an untrusted
/// chunk must not be able to exhaust the host stack.
const MAX_PROTO_DEPTH: usize = 200;

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Writer { buf: Vec::new() }
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.buf.extend_from_slice(b);
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.buf.len() - self.pos < n {
            return Err("truncated binary chunk".into());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}

/// Allocates a buffer for `n` decoded elements without letting a crafted
/// length prefix abort the host: `n` comes straight from the untrusted chunk,
/// and each element consumes at least one byte, so `n.min(remaining)` is a
/// safe upper bound on what can actually be read.
fn new_vec<T>(r: &Reader<'_>, n: usize) -> Result<Vec<T>, String> {
    let mut v = Vec::new();
    v.try_reserve(n.min(r.remaining()))
        .map_err(|_| "not enough memory".to_string())?;
    Ok(v)
}

// ---------------------------------------------------------------------------
// Dump
// ---------------------------------------------------------------------------

/// True if `bytes` looks like a Lua binary chunk. PUC classifies by the
/// first byte alone (`LUA_SIGNATURE[0]`), so a partially-truncated signature
/// is still "binary" and reports truncation rather than a parse error.
pub(super) fn is_binary(bytes: &[u8]) -> bool {
    bytes.first() == Some(&0x1b)
}

/// `string.dump(f[, strip])`. `strip` is accepted but ignored: slew does not
/// store optional debug info that could be dropped without breaking
/// `debug.getinfo`/`getupvalue` round-trips.
pub(super) fn n_dump<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let Value::Closure(cid) = arg(args, 0) else {
        let got = arg(args, 0).type_name();
        if got == "function" {
            // A native function: PUC reports this specific failure.
            return Err("unable to dump given function".into());
        }
        return Err(format!(
            "bad argument #1 to 'dump' (function expected, got {got})"
        ));
    };
    let proto = lua.closures[cid.0 as usize].proto.clone();
    let mut w = Writer::new();
    w.buf.extend_from_slice(HEADER);
    w.i64(SENTINEL_INT);
    w.f64(SENTINEL_NUM);
    w.buf.extend_from_slice(PAYLOAD_MAGIC);
    w.u8(VERSION);
    write_proto(&mut w, lua, &proto, 0)?;
    Ok(vec![lua.new_string(&w.buf)])
}

fn write_proto<C>(w: &mut Writer, lua: &Lua<C>, p: &Proto, depth: usize) -> Result<(), String> {
    // Keep the writer symmetric with the reader's nesting cap so a program
    // slew can dump is also one slew can load.
    if depth > MAX_PROTO_DEPTH {
        return Err("unable to dump given function (too deeply nested)".into());
    }
    w.bytes(p.source.as_bytes());
    w.bytes(p.name.as_bytes());
    w.u8(p.nparams);
    w.u8(p.is_vararg as u8);
    w.u8(p.max_regs);
    w.u32(p.linedefined);
    w.u32(p.lastlinedefined);

    w.u32(p.code.len() as u32);
    for ins in &p.code {
        write_instr(w, *ins);
    }

    w.u32(p.lines.len() as u32);
    for line in &p.lines {
        w.u32(*line);
    }

    w.u32(p.consts.len() as u32);
    for c in &p.consts {
        write_const(w, lua, *c);
    }

    w.u32(p.upvals.len() as u32);
    for u in &p.upvals {
        match u {
            UpvalDesc::Local(r) => {
                w.u8(0);
                w.u8(*r);
            }
            UpvalDesc::Upval(r) => {
                w.u8(1);
                w.u8(*r);
            }
        }
    }

    w.u32(p.upval_names.len() as u32);
    for n in &p.upval_names {
        w.bytes(n.as_bytes());
    }

    w.u32(p.protos.len() as u32);
    for proto in &p.protos {
        write_proto(w, lua, proto, depth + 1)?;
    }

    w.u32(p.call_names.len() as u32);
    for cn in &p.call_names {
        match cn {
            None => w.u8(0),
            Some((what, name)) => {
                w.u8(1);
                w.u8(namewhat_index(what));
                w.bytes(name.as_bytes());
            }
        }
    }

    w.u32(p.reg_extent.len() as u32);
    for e in &p.reg_extent {
        w.u8(*e);
    }
    Ok(())
}

fn write_const<C>(w: &mut Writer, lua: &Lua<C>, v: Value) {
    match v {
        Value::Nil => w.u8(0),
        Value::Bool(false) => w.u8(1),
        Value::Bool(true) => w.u8(2),
        Value::Int(i) => {
            w.u8(3);
            w.i64(i);
        }
        Value::Float(f) => {
            w.u8(4);
            w.f64(f);
        }
        Value::Str(id) => {
            w.u8(5);
            w.bytes(lua.strings.get(id));
        }
        other => unreachable!("non-literal constant in Proto: {other:?}"),
    }
}

fn write_instr(w: &mut Writer, ins: Instr) {
    match ins {
        Instr::LoadK { dst, k } => {
            w.u8(0);
            w.u8(dst);
            w.u16(k);
        }
        Instr::LoadNil { dst, n } => {
            w.u8(1);
            w.u8(dst);
            w.u8(n);
        }
        Instr::LoadBool { dst, b } => {
            w.u8(2);
            w.u8(dst);
            w.u8(b as u8);
        }
        Instr::Move { dst, src } => {
            w.u8(3);
            w.u8(dst);
            w.u8(src);
        }
        Instr::GetUpval { dst, up } => {
            w.u8(4);
            w.u8(dst);
            w.u8(up);
        }
        Instr::SetUpval { up, src } => {
            w.u8(5);
            w.u8(up);
            w.u8(src);
        }
        Instr::GetIndex { dst, obj, key } => {
            w.u8(6);
            w.u8(dst);
            w.u8(obj);
            w.u8(key);
        }
        Instr::GetField { dst, obj, k } => {
            w.u8(7);
            w.u8(dst);
            w.u8(obj);
            w.u16(k);
        }
        Instr::SetIndex { obj, key, src } => {
            w.u8(8);
            w.u8(obj);
            w.u8(key);
            w.u8(src);
        }
        Instr::SetField { obj, k, src } => {
            w.u8(9);
            w.u8(obj);
            w.u16(k);
            w.u8(src);
        }
        Instr::NewTable { dst } => {
            w.u8(10);
            w.u8(dst);
        }
        Instr::SetList {
            obj,
            base,
            n,
            start,
        } => {
            w.u8(11);
            w.u8(obj);
            w.u8(base);
            w.u8(n);
            w.u32(start);
        }
        Instr::Arith { op, dst, lhs, rhs } => {
            w.u8(12);
            w.u8(arith_index(op));
            w.u8(dst);
            w.u8(lhs);
            w.u8(rhs);
        }
        Instr::Unary { op, dst, src } => {
            w.u8(13);
            w.u8(unary_index(op));
            w.u8(dst);
            w.u8(src);
        }
        Instr::Cmp { op, dst, lhs, rhs } => {
            w.u8(14);
            w.u8(cmp_index(op));
            w.u8(dst);
            w.u8(lhs);
            w.u8(rhs);
        }
        Instr::Concat { dst, base, n } => {
            w.u8(15);
            w.u8(dst);
            w.u8(base);
            w.u8(n);
        }
        Instr::Jump { off } => {
            w.u8(16);
            w.i32(off);
        }
        Instr::Test { src, if_true, off } => {
            w.u8(17);
            w.u8(src);
            w.u8(if_true as u8);
            w.i32(off);
        }
        Instr::Call { base, nargs, nres } => {
            w.u8(18);
            w.u8(base);
            w.u8(nargs);
            w.u8(nres);
        }
        Instr::TailCall { base, nargs } => {
            w.u8(19);
            w.u8(base);
            w.u8(nargs);
        }
        Instr::Return { base, n } => {
            w.u8(20);
            w.u8(base);
            w.u8(n);
        }
        Instr::Vararg { dst, n } => {
            w.u8(21);
            w.u8(dst);
            w.u8(n);
        }
        Instr::Closure { dst, p } => {
            w.u8(22);
            w.u8(dst);
            w.u16(p);
        }
        Instr::Close { from } => {
            w.u8(23);
            w.u8(from);
        }
        Instr::Tbc { reg, name } => {
            w.u8(24);
            w.u8(reg);
            w.u16(name);
        }
        Instr::ForPrep { base, off } => {
            w.u8(25);
            w.u8(base);
            w.i32(off);
        }
        Instr::ForLoop { base, off } => {
            w.u8(26);
            w.u8(base);
            w.i32(off);
        }
        Instr::TForLoop { base, off } => {
            w.u8(27);
            w.u8(base);
            w.i32(off);
        }
    }
}

fn arith_index(op: ArithOp) -> u8 {
    match op {
        ArithOp::Add => 0,
        ArithOp::Sub => 1,
        ArithOp::Mul => 2,
        ArithOp::Div => 3,
        ArithOp::IDiv => 4,
        ArithOp::Mod => 5,
        ArithOp::Pow => 6,
        ArithOp::BAnd => 7,
        ArithOp::BOr => 8,
        ArithOp::BXor => 9,
        ArithOp::Shl => 10,
        ArithOp::Shr => 11,
    }
}

fn unary_index(op: UnaryOp) -> u8 {
    match op {
        UnaryOp::Neg => 0,
        UnaryOp::Not => 1,
        UnaryOp::Len => 2,
        UnaryOp::BNot => 3,
    }
}

fn cmp_index(op: CmpOp) -> u8 {
    match op {
        CmpOp::Eq => 0,
        CmpOp::Ne => 1,
        CmpOp::Lt => 2,
        CmpOp::Le => 3,
    }
}

fn namewhat_index(what: &str) -> u8 {
    match what {
        "local" => 1,
        "upvalue" => 2,
        "method" => 3,
        "field" => 4,
        _ => 0,
    }
}

fn namewhat_from_index(i: u8) -> &'static str {
    match i {
        1 => "local",
        2 => "upvalue",
        3 => "method",
        4 => "field",
        _ => "global",
    }
}

// ---------------------------------------------------------------------------
// Undump
// ---------------------------------------------------------------------------

/// Decodes a slew binary chunk and builds its closure. Errors are the
/// `nil, message` text `load` reports (PUC-style: truncation, bad header,
/// unknown opcode).
pub(super) fn undump<C>(
    lua: &mut Lua<C>,
    bytes: &[u8],
    env: Option<Value>,
) -> Result<Value, String> {
    let mut r = Reader::new(bytes);
    let header = r.take(HEADER.len())?;
    if header != HEADER {
        return Err("bad binary format (corrupted header)".into());
    }
    if r.i64()? != SENTINEL_INT || r.f64()? != SENTINEL_NUM {
        return Err("bad binary format (missing integer/number sentinels)".into());
    }
    if r.take(PAYLOAD_MAGIC.len())? != PAYLOAD_MAGIC {
        return Err("bad binary format (not a slew bytecode chunk)".into());
    }
    if r.u8()? != VERSION {
        return Err("bad binary format (unsupported slew bytecode version)".into());
    }
    let proto = read_proto(&mut r, lua, 0)?;
    if r.pos != bytes.len() {
        return Err("trailing bytes in binary chunk".into());
    }
    validate_proto(&proto)?;
    Ok(lua.make_function_from_proto(Rc::new(proto), env))
}

fn validate_proto(p: &Proto) -> Result<(), String> {
    let bad = |what: &str| Err(format!("bad binary format ({what})"));
    if p.max_regs == 0 || p.max_regs > MAX_REGS {
        return bad("register count out of range");
    }
    let regs = p.max_regs as usize;
    if p.nparams > p.max_regs {
        return bad("too many parameters");
    }
    let ncode = p.code.len();
    if p.lines.len() != ncode || p.reg_extent.len() != ncode || p.call_names.len() != ncode {
        return bad("debug tables are not parallel to code");
    }
    if p.upval_names.len() != p.upvals.len() {
        return bad("upvalue names are not parallel to upvalues");
    }
    // Every register the instruction names, and every static run it reads or
    // writes, must fit the frame's `max_regs` slots. Counts use the VM's
    // encoding: `0` is the open form and `n` means `n - 1` items for the
    // multi-value fields (`Call`/`TailCall` `nargs`, `Return`, `Vararg`),
    // while `LoadNil`/`SetList`/`Concat` carry a raw count.
    let one = |r: u8| (r as usize) < regs; // a named register slot
    let at_most = |r: u8| r as usize <= regs; // ...or the one-past-end watermark
    let run = |base: u8, count: usize| (base as usize) < regs && base as usize + count <= regs;
    for (pc, ins) in p.code.iter().enumerate() {
        let regs_ok = match *ins {
            Instr::LoadK { dst, .. }
            | Instr::LoadBool { dst, .. }
            | Instr::GetUpval { dst, .. }
            | Instr::NewTable { dst }
            | Instr::Closure { dst, .. } => one(dst),
            Instr::LoadNil { dst, n } => run(dst, n as usize),
            Instr::Move { dst, src } | Instr::Unary { dst, src, .. } => one(dst) && one(src),
            Instr::SetUpval { src, .. } | Instr::Test { src, .. } => one(src),
            Instr::GetIndex { dst, obj, key } => one(dst) && one(obj) && one(key),
            Instr::GetField { dst, obj, .. } => one(dst) && one(obj),
            Instr::SetIndex { obj, key, src } => one(obj) && one(key) && one(src),
            Instr::SetField { obj, src, .. } => one(obj) && one(src),
            // `n == 0` takes its count from the dynamic top, so only the
            // object and base slots are pinned.
            Instr::SetList { obj, base, n, .. } => {
                one(obj) && one(base) && (n == 0 || run(base, n as usize))
            }
            Instr::Arith { dst, lhs, rhs, .. } | Instr::Cmp { dst, lhs, rhs, .. } => {
                one(dst) && one(lhs) && one(rhs)
            }
            // `n == 0` is likewise open; `n` is the raw register count.
            Instr::Concat { dst, base, n } => {
                one(dst) && one(base) && (n == 0 || run(base, n as usize))
            }
            // `nargs` is `count + 1`; the callee sits at `base`, so the window
            // is `base .. base + nargs - 1` (`0` is the open call).
            Instr::Call { base, nargs, .. } | Instr::TailCall { base, nargs } => {
                one(base) && run(base, nargs.max(1) as usize)
            }
            // `n == 1` returns no values, and `n == 0` runs from `base` up to
            // the dynamic top, so neither pins a static window; `base` may sit
            // just past the last register (`free_reg == max_regs` is a shape
            // the compiler emits).
            Instr::Return { base, n } => {
                if n <= 1 {
                    at_most(base)
                } else {
                    run(base, (n - 1) as usize)
                }
            }
            // `n == 1` copies no varargs; `n == 0` copies the whole list.
            Instr::Vararg { dst, n } => one(dst) && (n <= 1 || run(dst, (n - 1) as usize)),
            // `UNPATCHED_CLOSE` never names a register; `Jump` names none.
            Instr::Close {
                from: UNPATCHED_CLOSE,
            }
            | Instr::Jump { .. } => true,
            Instr::Close { from } => one(from),
            Instr::Tbc { reg, .. } => one(reg),
            Instr::ForPrep { base, .. } | Instr::ForLoop { base, .. } => run(base, 4),
            Instr::TForLoop { base, .. } => run(base, 5),
        };
        if !regs_ok {
            return bad("register operand out of range");
        }
        match *ins {
            Instr::LoadK { k, .. }
            | Instr::GetField { k, .. }
            | Instr::SetField { k, .. }
            | Instr::Tbc { name: k, .. } => {
                if k as usize >= p.consts.len() {
                    return bad("constant index out of range");
                }
            }
            Instr::Closure { p: child, .. } => {
                if child as usize >= p.protos.len() {
                    return bad("sub-prototype index out of range");
                }
            }
            Instr::GetUpval { up, .. } | Instr::SetUpval { up, .. } => {
                if up as usize >= p.upvals.len() {
                    return bad("upvalue index out of range");
                }
            }
            Instr::Jump { off }
            | Instr::Test { off, .. }
            | Instr::ForPrep { off, .. }
            | Instr::ForLoop { off, .. }
            | Instr::TForLoop { off, .. } => {
                // Offsets are relative to the instruction after the jump.
                let target = pc as i64 + 1 + i64::from(off);
                if target < 0 || target as usize >= ncode {
                    return bad("jump target out of range");
                }
            }
            _ => {}
        }
    }
    // A child closure captures from *this* frame, so its descriptors must
    // resolve against this proto's registers and upvalue list.
    for child in &p.protos {
        for d in &child.upvals {
            match *d {
                UpvalDesc::Local(r) if r >= p.max_regs => {
                    return bad("upvalue register out of range");
                }
                UpvalDesc::Upval(i) if i as usize >= p.upvals.len() => {
                    return bad("upvalue index out of range");
                }
                _ => {}
            }
        }
        validate_proto(child)?;
    }
    Ok(())
}

fn read_proto<C>(r: &mut Reader, lua: &mut Lua<C>, depth: usize) -> Result<Proto, String> {
    if depth > MAX_PROTO_DEPTH {
        return Err("binary chunk too deeply nested".into());
    }
    let source = String::from_utf8_lossy(r.bytes()?).into_owned();
    let name = String::from_utf8_lossy(r.bytes()?).into_owned();
    let nparams = r.u8()?;
    let is_vararg = r.u8()? != 0;
    let max_regs = r.u8()?;
    let linedefined = r.u32()?;
    let lastlinedefined = r.u32()?;

    let n = r.u32()? as usize;
    let mut code = new_vec(r, n)?;
    for _ in 0..n {
        code.push(read_instr(r)?);
    }

    let n = r.u32()? as usize;
    let mut lines = new_vec(r, n)?;
    for _ in 0..n {
        lines.push(r.u32()?);
    }

    let n = r.u32()? as usize;
    let mut consts = new_vec(r, n)?;
    for _ in 0..n {
        consts.push(read_const(r, lua)?);
    }

    let n = r.u32()? as usize;
    let mut upvals = new_vec(r, n)?;
    for _ in 0..n {
        let tag = r.u8()?;
        let reg = r.u8()?;
        upvals.push(match tag {
            0 => UpvalDesc::Local(reg),
            1 => UpvalDesc::Upval(reg),
            _ => return Err("bad binary format (upvalue kind)".into()),
        });
    }

    let n = r.u32()? as usize;
    let mut upval_names = new_vec(r, n)?;
    for _ in 0..n {
        upval_names.push(
            String::from_utf8_lossy(r.bytes()?)
                .into_owned()
                .into_boxed_str(),
        );
    }

    let n = r.u32()? as usize;
    let mut protos = new_vec(r, n)?;
    for _ in 0..n {
        protos.push(Rc::new(read_proto(r, lua, depth + 1)?));
    }

    let n = r.u32()? as usize;
    let mut call_names = new_vec(r, n)?;
    for _ in 0..n {
        if r.u8()? == 0 {
            call_names.push(None);
        } else {
            let what = namewhat_from_index(r.u8()?);
            let name = String::from_utf8_lossy(r.bytes()?)
                .into_owned()
                .into_boxed_str();
            call_names.push(Some((what, name)));
        }
    }

    // `reg_extent` is a GC rooting hint, so a chunk must not be able to
    // under-report it: root the whole declared frame for loaded protos.
    let n = r.u32()? as usize;
    for _ in 0..n {
        let _ = r.u8()?;
    }
    let reg_extent = vec![max_regs; code.len()];

    Ok(Proto {
        code,
        source: Rc::from(source),
        lines,
        consts,
        protos,
        upvals,
        upval_names,
        nparams,
        is_vararg,
        max_regs,
        reg_extent,
        name,
        linedefined,
        lastlinedefined,
        call_names,
    })
}

fn read_const<C>(r: &mut Reader, lua: &mut Lua<C>) -> Result<Value, String> {
    Ok(match r.u8()? {
        0 => Value::Nil,
        1 => Value::Bool(false),
        2 => Value::Bool(true),
        3 => Value::Int(r.i64()?),
        4 => Value::Float(r.f64()?),
        5 => {
            let bytes = r.bytes()?;
            Value::Str(lua.strings.intern(bytes))
        }
        _ => return Err("bad binary format (constant tag)".into()),
    })
}

fn read_instr(r: &mut Reader) -> Result<Instr, String> {
    let tag = r.u8()?;
    Ok(match tag {
        0 => Instr::LoadK {
            dst: r.u8()?,
            k: r.u16()?,
        },
        1 => Instr::LoadNil {
            dst: r.u8()?,
            n: r.u8()?,
        },
        2 => Instr::LoadBool {
            dst: r.u8()?,
            b: r.u8()? != 0,
        },
        3 => Instr::Move {
            dst: r.u8()?,
            src: r.u8()?,
        },
        4 => Instr::GetUpval {
            dst: r.u8()?,
            up: r.u8()?,
        },
        5 => Instr::SetUpval {
            up: r.u8()?,
            src: r.u8()?,
        },
        6 => Instr::GetIndex {
            dst: r.u8()?,
            obj: r.u8()?,
            key: r.u8()?,
        },
        7 => Instr::GetField {
            dst: r.u8()?,
            obj: r.u8()?,
            k: r.u16()?,
        },
        8 => Instr::SetIndex {
            obj: r.u8()?,
            key: r.u8()?,
            src: r.u8()?,
        },
        9 => Instr::SetField {
            obj: r.u8()?,
            k: r.u16()?,
            src: r.u8()?,
        },
        10 => Instr::NewTable { dst: r.u8()? },
        11 => Instr::SetList {
            obj: r.u8()?,
            base: r.u8()?,
            n: r.u8()?,
            start: r.u32()?,
        },
        12 => Instr::Arith {
            op: arith_from_index(r.u8()?)?,
            dst: r.u8()?,
            lhs: r.u8()?,
            rhs: r.u8()?,
        },
        13 => Instr::Unary {
            op: unary_from_index(r.u8()?)?,
            dst: r.u8()?,
            src: r.u8()?,
        },
        14 => Instr::Cmp {
            op: cmp_from_index(r.u8()?)?,
            dst: r.u8()?,
            lhs: r.u8()?,
            rhs: r.u8()?,
        },
        15 => Instr::Concat {
            dst: r.u8()?,
            base: r.u8()?,
            n: r.u8()?,
        },
        16 => Instr::Jump { off: r.i32()? },
        17 => Instr::Test {
            src: r.u8()?,
            if_true: r.u8()? != 0,
            off: r.i32()?,
        },
        18 => Instr::Call {
            base: r.u8()?,
            nargs: r.u8()?,
            nres: r.u8()?,
        },
        19 => Instr::TailCall {
            base: r.u8()?,
            nargs: r.u8()?,
        },
        20 => Instr::Return {
            base: r.u8()?,
            n: r.u8()?,
        },
        21 => Instr::Vararg {
            dst: r.u8()?,
            n: r.u8()?,
        },
        22 => Instr::Closure {
            dst: r.u8()?,
            p: r.u16()?,
        },
        23 => Instr::Close { from: r.u8()? },
        24 => Instr::Tbc {
            reg: r.u8()?,
            name: r.u16()?,
        },
        25 => Instr::ForPrep {
            base: r.u8()?,
            off: r.i32()?,
        },
        26 => Instr::ForLoop {
            base: r.u8()?,
            off: r.i32()?,
        },
        27 => Instr::TForLoop {
            base: r.u8()?,
            off: r.i32()?,
        },
        _ => return Err("bad binary format (instruction tag)".into()),
    })
}

fn arith_from_index(i: u8) -> Result<ArithOp, String> {
    Ok(match i {
        0 => ArithOp::Add,
        1 => ArithOp::Sub,
        2 => ArithOp::Mul,
        3 => ArithOp::Div,
        4 => ArithOp::IDiv,
        5 => ArithOp::Mod,
        6 => ArithOp::Pow,
        7 => ArithOp::BAnd,
        8 => ArithOp::BOr,
        9 => ArithOp::BXor,
        10 => ArithOp::Shl,
        11 => ArithOp::Shr,
        _ => return Err("bad binary format (arithmetic op)".into()),
    })
}

fn unary_from_index(i: u8) -> Result<UnaryOp, String> {
    Ok(match i {
        0 => UnaryOp::Neg,
        1 => UnaryOp::Not,
        2 => UnaryOp::Len,
        3 => UnaryOp::BNot,
        _ => return Err("bad binary format (unary op)".into()),
    })
}

fn cmp_from_index(i: u8) -> Result<CmpOp, String> {
    Ok(match i {
        0 => CmpOp::Eq,
        1 => CmpOp::Ne,
        2 => CmpOp::Lt,
        3 => CmpOp::Le,
        _ => return Err("bad binary format (comparison op)".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    /// Builds a proto over `code` with parallel debug tables, so a test can
    /// vary one field and see the validator's verdict.
    fn proto_with(code: Vec<Instr>, consts: Vec<Value>, max_regs: u8) -> Proto {
        let n = code.len();
        Proto {
            code,
            source: Rc::from("=t"),
            lines: vec![0; n],
            consts,
            protos: Vec::new(),
            upval_names: Vec::new(),
            upvals: Vec::new(),
            nparams: 0,
            is_vararg: true,
            max_regs,
            reg_extent: vec![max_regs; n],
            name: "t".into(),
            linedefined: 0,
            lastlinedefined: 0,
            call_names: vec![None; n],
        }
    }

    #[test]
    fn validate_proto_accepts_a_well_formed_proto() {
        let p = proto_with(
            vec![
                Instr::LoadK { dst: 0, k: 0 },
                Instr::Jump { off: 0 },
                Instr::Return { base: 0, n: 2 },
            ],
            vec![Value::Int(1)],
            2,
        );
        assert_eq!(validate_proto(&p), Ok(()));
    }

    #[test_case(
        vec![Instr::LoadK { dst: 0, k: 7 }, Instr::Return { base: 0, n: 2 }],
        vec![Value::Int(1)],
        2
        ; "constant_index_past_table")]
    #[test_case(vec![Instr::Move { dst: 9, src: 0 }], Vec::new(), 2 ; "register_beyond_max_regs")]
    #[test_case(
        vec![Instr::Call { base: 0, nargs: 9, nres: 0 }],
        Vec::new(),
        2
        ; "call_argument_window")]
    #[test_case(vec![Instr::Jump { off: 100 }], Vec::new(), 2 ; "jump_past_end")]
    #[test_case(vec![Instr::GetUpval { dst: 0, up: 0 }], Vec::new(), 2 ; "upvalue_index")]
    fn validate_proto_rejects(code: Vec<Instr>, consts: Vec<Value>, max_regs: u8) {
        let p = proto_with(code, consts, max_regs);
        assert!(validate_proto(&p).is_err());
    }

    #[test]
    fn validate_proto_rejects_non_parallel_debug_tables() {
        let mut p = proto_with(vec![Instr::Return { base: 0, n: 1 }], Vec::new(), 2);
        p.lines.clear();
        assert!(validate_proto(&p).is_err());
    }

    #[test]
    fn validate_proto_rejects_a_bad_sub_proto() {
        // Closure referring to a sub-proto that does not exist.
        let p = proto_with(vec![Instr::Closure { dst: 0, p: 3 }], Vec::new(), 2);
        assert!(validate_proto(&p).is_err());

        // A child capture register that the parent frame cannot provide.
        let mut child = proto_with(vec![Instr::Return { base: 0, n: 1 }], Vec::new(), 2);
        child.upvals = vec![UpvalDesc::Local(9)];
        child.upval_names = vec!["x".into()];
        let mut p = proto_with(vec![Instr::Closure { dst: 0, p: 0 }], Vec::new(), 2);
        p.protos = vec![Rc::new(child)];
        assert!(validate_proto(&p).is_err());
    }
}
