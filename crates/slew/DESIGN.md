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
  they are suspendable like all Lua code. `io`/`os` exist only when an
  embedder installs a capability host (see below); without one they are
  absent.
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
- `Lua::set_host` installs the `io`/`os` capability host (`src/host.rs`).
  While no host is installed, `io`/`os` do not exist (globals unset,
  `require` fails) and the core never opens a file, reads the environment, or
  spawns a process. `Lua::has_host`/`clear_host` detect and remove it. `os.exit`
  is surfaced as a request (`Lua::take_exit_request`) plus a controlled error;
  the host process is never terminated. `os.execute` is absent (process
  authority is a non-goal). A std-backed `StdHost` (root-confined filesystem,
  captured std streams, UTC calendar) ships for tests and simple embedders.
  File handles are userdata whose host resources are released on `close`, on
  `__gc`, and on sweep of an unreachable handle.

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
  honors `__pairs` and `ipairs` honors `__index` (PUC 5.4 semantics). `print`
  and `collectgarbage` are intrinsics (not prelude Lua closures) so they are
  C-like functions with no upvalues.
- Weak tables (`__mode` = `k`/`v`/`kv`), ephemeron semantics, and `__gc`
  finalizers (run once, may resurrect, LIFO) are implemented in the mark-sweep
  collector, as is `collectgarbage([opt[, arg]])` and `coroutine.close`.
- `collectgarbage("step", n)`: the collector has no resumable incremental
  phases, so one `step` runs a full (bounded) mark-sweep collection and
  returns `true`, matching PUC incremental mode's contract that the call
  reports a finished collection cycle (`false` only while a cycle is still
  in progress, which never happens here). `n` is type-checked like PUC but
  cannot select a partial amount of work. This keeps the upstream
  `repeat ... until collectgarbage("step", siz)` loops terminating.
  `collectgarbage` option #1 coerces numbers to strings (so
  `collectgarbage(5)` reports `invalid option '5'`), as PUC does. Calling
  `collectgarbage` from inside a `__gc` handler reports every option as
  invalid and yields a single `nil` (PUC's "collection running" state), so a
  reentrant call is a no-op.
- **Root precision**: the compiler records, per instruction, the live register
  extent at that point (`Proto::reg_extent`). The collector roots only
  `frame.base .. base + reg_extent[pc]` for each frame — not the whole thread
  stack — so slots of popped/tail-replaced frames and dead temporaries above
  the current top are not roots. Open multret arguments of an intrinsic call
  and return values staged while `__close` handlers run are rooted explicitly.
  Suspended coroutines keep their live registers rooted across resume/yield.
  This matches PUC's weak-table reclamation (upstream `gc.lua` passes) without
  premature collection.
- Binary chunks carry PUC 5.4's header and `LUAC_INT`/`LUAC_NUM` sentinels,
  but the proto body after them is slew-specific (see `src/stdlib/dump.rs`),
  so dumps round-trip through slew's `load` and are not portable to PUC's
  `luac`/`undump`.
- `package.cpath`/`package.loadlib` are inert; dynamic C libraries are not
  supported.
- `debug` is implemented as VM intrinsics (`getinfo`, `traceback`, the
  upvalue API, and the metatable bypass). One deviation follows from the
  architecture: prelude stdlib functions (e.g. `pairs`, `ipairs`,
  `table.sort`) are Lua closures carrying an `_ENV` upvalue, so
  `debug.getinfo` reports `what == "Lua"` for them instead of `"C"` and
  `debug.upvaluejoin` treats them like any closure.
- Synthetic C frames are *not* modelled. PUC reports a C frame for the
  `pcall`/`xpcall`/`coroutine.close` boundaries (and for `yield`/`resume`), so
  `debug.getinfo`/`debug.traceback` level numbering around those boundaries
  diverges: slew counts Lua frames only. slew still keeps an internal
  `BoundaryFrame` to carry a protected call's `__close`/error-delivery
  continuations, but it is invisible to the debug API. This is why upstream
  `locals.lua` and `coroutine.lua` are excluded from the conformance set
  (both assert on the missing C-frame levels). PUC's `metamethod 'close'`
  frame naming is likewise not implemented.
- `next` iteration order is stable per table state but not PUC's; per-call
  cost is O(n) (acceptable until tables move to an insertion-ordered map).

## Non-goals

- Bug-for-bug PUC-Lua compatibility (e.g. exact error message strings, exact
  `next` iteration order).
- The C API.
- JIT or competitive raw performance; predictable, budgetable execution wins
  every trade-off.
