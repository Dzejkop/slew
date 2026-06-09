# suslua — design

A Lua 5.4 interpreter written from scratch in Rust, built around one core property:
**the embedder controls execution exactly**. A script is loaded once, then driven with
`step(fuel)` calls; the VM runs at most that much work and suspends, resumable later.

## Why stackless

Suspension anywhere — mid-loop, mid-call, inside `pcall`, inside a metamethod —
is only possible if the interpreter never uses the host (Rust) call stack for Lua
calls. So:

- Call frames live in a `Vec<Frame>` owned by the thread, not on the Rust stack.
- A Lua→Lua call pushes a frame and continues the dispatch loop; a return pops one.
- Metamethod invocations are also pushed frames (with a return-target register),
  never reentrant Rust calls.
- `pcall` is a VM intrinsic that marks a frame as a protection boundary; error
  unwinding walks the frame vec.
- Coroutines are additional threads (frame stack + register stack) the VM switches
  between; they fall out of the architecture almost for free.

## Fuel

`Execution::step(fuel)` runs the dispatch loop, charging fuel per instruction
(non-uniform costs are possible later: e.g. table allocation, string concat sized
by length). Fuel is signed; a native call may overdraw and the debt carries into
the next `step`. Result is `Step::Done(values)`, `Step::Pending`, or `Err(Error)`.

Future knobs under the same "execution profile" umbrella: memory ceiling (heap is
fully owned, so allocation accounting is straightforward), call-depth limit,
per-step wall-clock is the embedder's business (they own the loop).

## Pipeline

```
source ──lexer──▶ tokens ──parser──▶ AST ──compiler──▶ Proto (bytecode)
                                                          │
                                              Lua::execute(chunk)
                                                          ▼
                                            Execution::step(fuel) ─▶ Done/Pending
```

- **Lexer** (`lexer.rs`): full 5.4 lexical grammar — long strings/comments with
  levels, all escapes (`\x`, `\ddd`, `\u{}`, `\z`), hex floats. Strings are byte
  strings (`Box<[u8]>`), as in Lua.
- **Parser** (`parser.rs`): recursive descent, full 5.4 grammar including
  `goto`/labels and attributes (`<const>`, `<close>`).
- **Compiler** (`compiler.rs`): AST → register-based bytecode, Lua-style: registers
  are stack slots, locals occupy fixed slots, temporaries above. Closures with
  upvalue capture (open upvalues pointing into the register stack, closed on scope
  exit). Globals compile to `_ENV` upvalue accesses, per 5.4.
- **VM** (`vm.rs`): the stackless dispatch loop described above.
- **Heap** (`value.rs`): all GC objects (strings, tables, closures, threads) live in
  arenas owned by the `Lua` state, referenced by index handles. `Value` is `Copy`.
  Strings are interned. Real mark-sweep GC is a later milestone; handle-based
  design keeps it tractable (no `unsafe`, no `Rc` cycles).

## Semantics targets (Lua 5.4)

Everything implemented follows 5.4 rules from the start, in particular:

- Integers (`i64`, wrapping arithmetic) and floats (`f64`); `/` and `^` always
  float, `//` `%` stay integer on integers; mathematically correct int↔float
  comparison; `1 == 1.0`; float keys with integral values normalize to integer
  table keys; NaN keys are errors.
- Byte strings, not UTF-8.
- Multiple returns / multret call expressions, varargs.
- Metatables on all types, full metamethod set (milestone M4).
- `error` with arbitrary values, `pcall`/`xpcall` as intrinsics.

## Milestones

- **M1 (this slice)**: lexer, parser (full grammar), compiler + VM for the core
  language — locals, control flow, numeric/generic `for`, functions, closures,
  multiple returns, varargs, tables, full operator set on primitives — plus the
  public fuel API with suspension tests.
- **M2**: metatables + metamethods, `pcall`/`error`, `goto` compilation.
- **M3**: coroutines (full `coroutine.*`), to-be-closed variables.
- **M4**: stdlib (`string`, `table`, `math`, sandboxed `os`/`io` opt-ins);
  natives that call back into Lua (e.g. `table.sort`) via continuation frames.
- **M5**: mark-sweep GC + memory budget as part of the execution profile.

## Non-goals

- Bug-for-bug PUC-Lua compatibility (e.g. exact error message strings, exact
  `next` iteration order).
- The C API.
- JIT or competitive raw performance; predictable, budgetable execution wins
  every trade-off.
