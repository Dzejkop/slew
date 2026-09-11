# slew — design

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

## Milestones (all complete)

- **M1**: lexer, parser (full grammar), compiler + VM for the core
  language — locals, control flow, numeric/generic `for`, functions, closures,
  multiple returns, varargs, tables, full operator set on primitives — plus the
  public fuel API with suspension tests.
- **M2**: metatables + the full metamethod set (as shaped frames, never
  reentrant Rust), error values, `pcall`/`xpcall` as protected frames,
  `goto` with scope rules.
- **M3**: coroutines (threads in the arena; resume/yield switch the dispatch
  loop), to-be-closed variables with `__close` on all exit paths including
  error unwinding.
- **M4**: stdlib — `string` with a full Lua-pattern engine, `table`, `math`
  (deterministically seeded PRNG). Callback-using functions (`table.sort`,
  `gsub`, `gmatch`) are written in a Lua prelude compiled at startup, so
  they are suspendable like all Lua code. `os`/`io` deliberately absent.
- **M5**: mark-sweep GC over the handle arenas (free-list slot reuse) with
  roots from globals, live executions, and host anchors; `lua.gc()`,
  `memory_used()`, auto-collection by allocation threshold, and
  `memory_limit` as part of the execution profile.

## Host capabilities

The interpreter has no ambient authority: it never opens files, reads the
environment, or spawns processes. Anything that would need the host goes
through a capability the embedder installs explicitly.

- `Lua::set_file_reader` installs the byte source used by `loadfile`,
  `dofile`, and `package.searchpath`'s probes. Without one, `loadfile`
  reports "cannot open", and `require` resolves only `package.preload` and
  modules already in `package.loaded`.
- The `fs` (default-on) feature adds just `Lua::set_fs_file_reader(root)`, a
  filesystem adapter confined to `root` (absolute paths, `..`, and symlinks
  escaping the root are refused). Disable the feature for targets without a
  filesystem; the core still compiles.
- Module policy lives one level up, in the Lua prelude: `package.path`,
  `package.searchers`, and `require` are ordinary Lua and can be replaced or
  extended. `package.path` is not a security boundary; the reader (or a
  custom searcher) is.
- `load` (string or reader-function chunks) is pure. Only `loadfile`,
  `dofile`, and `package.searchpath`'s existence probes cross the host seam,
  so the filesystem surface stays a single function.

## Execution profile knobs

- `Execution::step(fuel)` — the core budget; debt from surcharges carries.
- `Lua::memory_limit` — approximate byte ceiling, enforced at collection
  points ("not enough memory" error).
- `Lua::gc_alloc_threshold` — auto-GC cadence (0 disables; `lua.gc()` is
  always available).
- Call-depth cap (frames are heap data, the host stack is never consumed).
- `math.random` is deterministic by default (fixed seed).

## Known deviations / caveats

- `Value` handles held by the host are not GC roots: use `lua.anchor(v)` or
  keep them reachable from Lua. Suspended `Execution`s are roots until they
  finish or are `abort`ed.
- `pcall(coroutine.wrap(f))` does not catch errors raised after the wrapped
  coroutine suspends and later fails (the protection has no frame to attach
  to across the switch); `coroutine.resume`'s `false, err` convention works.
- An error raised by a `__close` handler during unwinding supersedes the
  original error and skips remaining closes up to the next handler.
- `tostring`/`print` honor `__tostring` and fall back to `__name`; `pairs`
  honors `__pairs` and `ipairs` honors `__index` (PUC 5.4 semantics).
- No weak tables (`__mode`) or finalizers (`__gc`): sandboxed scripting
  rarely needs them; resources should be host-managed.
- Binary chunks are rejected: `string.dump` is absent and `load` refuses the
  `\x1bLua` signature.
- `package.cpath`/`package.loadlib` are inert; dynamic C libraries are not
  supported.
- `debug` is implemented as VM intrinsics (`getinfo`, `traceback`, the
  upvalue API, and the metatable bypass). Two deviations follow from the
  architecture: prelude stdlib functions are Lua closures carrying an `_ENV`
  upvalue, so `debug.getinfo` reports `what == "Lua"` for e.g. `print`
  instead of `"C"` and `debug.upvaluejoin` treats them like any closure; and
  there is no C-frame model for `pcall`/`coroutine.yield`, so traceback's C
  frames, level 0 on a suspended coroutine, and `getinfo` level 0 differ
  from PUC.
- `next` iteration order is stable per table state but not PUC's; per-call
  cost is O(n) (acceptable until tables move to an insertion-ordered map).

## Non-goals

- Bug-for-bug PUC-Lua compatibility (e.g. exact error message strings, exact
  `next` iteration order).
- The C API.
- JIT or competitive raw performance; predictable, budgetable execution wins
  every trade-off.
