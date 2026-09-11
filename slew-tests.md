# Handoff: slew — Lua 5.4 conformance harness and the missing-API plan

Date: 2026-09-11
Repo: `/Users/jakubtrad/Projects/github.com/Dzejkop/slew`
Branch: `main`. Phase 1 (modules + dynamic loading) is implemented in the
working tree (uncommitted at time of writing); the rest of this document is
the original plan and still applies to Phases 2–8.

This handoff focuses on the testing setup and the functionality still missing,
per the request. The TUI example (`d264d67`) is unrelated and only relevant as
a demo of the suspendable-execution API.

## Where things are

- `scripts/run-lua-tests.sh` — fetches the pinned Lua 5.4.9 archive
  (`https://www.lua.org/tests/`, SHA-256
  `7d971845f545ffc09fbb3128a86b2c6524161c70d0fdf0154a16e8c00c343fca`), caches
  it under `target/lua-tests/`, then runs the harness.
- `tests/conformance.rs` — the runner. 17 curated upstream files, per-file
  preamble/shims, wall-clock budget, `catch_unwind` so Rust panics are
  reported as failures rather than killing the run. `#[ignore]`d by default,
  so offline `cargo test` stays fast.
- `tests/lua-conformance.baseline` — source of truth for each file's first
  failure. Read it; do not copy it into other docs.
- `docs/research/lua-test-suites.md` — why this corpus, licensing, basic vs
  full mode.
- `DESIGN.md` — architecture and known deviations (no weak tables/`__gc`, no
  `io`/`os`, `%a` missing, `ipairs` raw indexing, `print` raw tostring).
- Fixes + harness landed in commit `cd52bba`; commit message lists the five
  reported bugs and the three compiler panics fixed.

## Current conformance state (summary only; baseline is authoritative)

After Phase 1: 3/17 files run to completion (`verybig`, `vararg`, `bitwise`),
zero Rust panics. The remaining first stops are: `debug` (still an empty
stub: constructs, goto, coroutine, events, literals), tail-call stack depth
(calls), `collectgarbage`/weak tables (closure timeout, nextvar), `tracegc`
(locals), `table.move` (sort), `utf8`/`%p` (pm, strings), a math gap (math),
and `io` (attrib, terminal for now). Re-check `tests/lua-conformance.baseline`
rather than trusting this list.

## How the harness works (operational notes)

- Env: `SLEW_LUA_TESTS_DIR` to reuse an extraction, `SLEW_LUA_TESTS_TIMEOUT`
  per-file wall clock (default 20s), `SLEW_BLESS=1` to rewrite the baseline,
  `SLEW_LUA_TESTS_VERBOSE=1` to print each case's error.
- The baseline is a **ratchet**: failing *earlier* than the recorded point is
  a regression; getting further passes. A Rust panic always fails, even when
  blessing. Progress is measured by `RuntimeError::root_line` (the case
  file's own call site), so failures inside helpers, the prelude, or loaded
  chunks don't misrank.
- Per-case shims exist only for `collectgarbage` (no-op) and
  `string.packsize` (returns 8). Remove them as the real APIs land.
- Add a file to `CASES` in `tests/conformance.rs` to widen coverage; bless
  afterward.
- The harness installs a suite-rooted reader via `set_fs_file_reader` (or a
  plain closure when the `fs` feature is off) and sets
  `package.path = "?.lua;libs/?.lua"` in the preamble.

## The plan (the main thing not captured in any artifact)

Deliverable: move each baseline stop forward, phase by phase, prioritizing
files unblocked per unit of work. No new panics; keep `io`/`os` ambient
authority out of the core. Verify + bless + commit per phase.

### Architectural constraints that force the mechanism

- Natives (`fn(&mut Lua, &[Value])`) cannot call Lua or metamethods
  (`src/stdlib/mod.rs:4`). Callback-shaped stdlib goes in the Lua prelude
  (`src/stdlib/prelude.lua`).
- The active thread is `mem::take`n out of `arena` during dispatch
  (`src/vm.rs:707`), so natives cannot see frames. Anything introspective
  must be an **intrinsic**.
- Closures with a chosen `_ENV` are built like `Lua::execute` does:
  `new_upval(Closed(env))` + `alloc_closure` (`src/vm.rs:479`).

### Missing surface inventory (hits across the 17 files)

| API | Uses | Mechanism |
|---|---|---|
| `package` + `require` | attrib(40), locals, goto, calls, events, bitwise, literals, coroutine, closure, nextvar, strings | Prelude + host loader hook |
| `load`/`loadfile`/`dofile` | calls(40), literals(20), attrib(17), constructs(8), locals(8), strings(4), bitwise(4), math, vararg, goto, nextvar, coroutine, pm, sort | Native + prelude (string chunks compile in Rust; reader chunks concatenated in the prelude) |
| `table.move` + metamethod-aware `remove`/`concat`/`unpack` | sort(24), nextvar | Prelude |
| `print`→`__tostring`, `pairs`→`__pairs`, `ipairs`→`__index` | events, nextvar, broadly | Prelude |
| `utf8.*` | pm(2), strings | Native |
| `string.pack`/`unpack`/`packsize` | strings, calls, bitwise | Native |
| `%p`/`%a`, `math.deg`/`rad`, `rep` overflow | strings, math, calls | Native |
| `collectgarbage`, weak tables, `__gc`, `coroutine.close` | closure, locals(15), events, coroutine(13) | Native + GC/VM |
| `debug.*` | closure(22), goto(18), coroutine(18), events(15), locals(14), calls(7) | Intrinsic, tiered |
| `io.*`, `os.*` | several | Out of scope; host-provided or accept the stop |

### Phases

1. **Modules + dynamic loading — done (Phase 1).** Landed as:
   `package{loaded,preload,path,cpath,config,searchers,searchpath}` created in
   `src/stdlib/mod.rs` with `require`, the four PUC searchers, `load`'s
   reader loop, and `dofile` in `src/stdlib/prelude.lua`; `load`/`loadfile`
   natives (string chunks via `parse`/`compile`, function-reader chunks
   concatenated in the prelude under `pcall`, `nil, message` on errors,
   `chunkname`, `mode`, present-but-nil `env`); `Lua::set_file_reader` seam
   plus `Lua::make_function` for `package.preload`; default-on `fs` feature
   adding only `Lua::set_fs_file_reader(root)` (`src/fs.rs`). An empty
   `debug` stub is registered so the six `require "debug"` files reveal
   their real next stop. New tests live in `tests/modules.rs`.
2. **Table library** — `table.move` in the prelude; migrate `remove`,
   `concat`, `unpack` to use `lua_len` + metamethod-aware indexing.
3. **Base metamethod conformance** — `print` via `tostring`, `pairs` via
   `__pairs`, `ipairs` via `__index` (shared iterator identity, which the
   suite asserts).
4. **`utf8`** — full library, native.
5. **String/math gaps** — `%p`, `%a`, pack/unpack/packsize,
   `math.deg`/`rad`, `rep` "too large".
6. **GC surface** — real `collectgarbage` options, weak tables in mark-sweep,
   `__gc` finalizers, `coroutine.close`.
7. **`debug`** — tier (a): `setmetatable`, upvalue get/set/id/join,
   `traceback`, static `getinfo`; tier (b): `getlocal`/`setlocal` (needs
   compiler local-name metadata); tier (c): `sethook` (needs dispatch hooks).
8. **Explicit non-goals** — `io`, `os` time/process. Keep them absent; note
   terminal stops. (`string.dump`/binary chunks are implemented in
   `src/stdlib/dump.rs`: the PUC 5.4 header and `LUAC_INT`/`LUAC_NUM`
   sentinels are emitted verbatim for observable compatibility, then a
   slew-specific `Proto` payload that only slew's `load` can read.)

Not in the original inventory: `calls.lua` now stops at line 120 on
"stack overflow" in its tail-call tests — proper tail calls are unimplemented
(a frame-reuse change in the dispatch loop), and `locals.lua` stops inside
`tracegc.lua`, which likely needs `collectgarbage` introspection.

### Verification loop (per phase)

```bash
cargo test --all-features
cargo test --no-default-features
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
scripts/run-lua-tests.sh                    # or SLEW_LUA_TESTS_DIR=<extraction>
SLEW_BLESS=1 scripts/run-lua-tests.sh       # after intentional progress
```

Review the `tests/lua-conformance.baseline` diff before committing; it must
only move forward.

## Environment gotchas

- Plain `cargo clippy` fails here: rustup's `clippy-driver` 1.98 shadows the
  Nix toolchain 1.97 and chokes on proc-macro rmeta. Always run clippy through
  `nix develop -c`.
- Never run `cargo fmt --all`; the repo has pre-existing rustfmt drift and it
  reformats ~20 unrelated files. Format only the file you touched:
  `nix develop -c rustfmt --edition 2024 <file>`.
- The suite lives at `target/lua-tests/lua-5.4.9-tests` after the first run.
- `table.unpack`'s result cap is 1,000,000, not PUC's `INT_MAX` (pre-existing;
  revisit if exact conformance matters).

## Open questions / risks

- `debug` tier (b)/(c) is a real project (compiler debug metadata, VM hooks);
  decide how far to go before touching it.
- `__gc`/weak tables touch mark-sweep and root handling; scope carefully.
- `os.setlocale` appears in strings/literals; a deterministic "C"-only
  implementation may be acceptable, but it is a design call.
- Whether to keep the two test-only shims once real `collectgarbage` and
  `string.packsize` land.

## Suggested skills

- `implement` — drive the phase work from the plan:
  `/Users/jakubtrad/.agents/skills/implement/SKILL.md`
- `tdd` — for the new APIs; keep regression tests alongside the harness:
  `/Users/jakubtrad/.agents/skills/tdd/SKILL.md`
- `codebase-design` — when shaping `load`/`require`/loader-hook interfaces:
  `/Users/jakubtrad/.agents/skills/codebase-design/SKILL.md`
- `code-review` — review each phase before bless/commit:
  `/Users/jakubtrad/.agents/skills/code-review/SKILL.md`
- `diagnosing-bugs` — for any new upstream failure or host panic:
  `/Users/jakubtrad/.agents/skills/diagnosing-bugs/SKILL.md`
- `to-tickets` / `wayfinder` — if the phases should be broken into tracked
  tickets before implementation:
  `/Users/jakubtrad/.agents/skills/to-tickets/SKILL.md`,
  `/Users/jakubtrad/.agents/skills/wayfinder/SKILL.md`

## Suggested immediate next step

Phase 1 is done. Next: **Phase 2** (`table.move` + metamethod-aware
`remove`/`concat`/`unpack` in the prelude) and **Phase 3** (`print`/`pairs`/
`ipairs` metamethod conformance), both small and independent. Then `utf8`
(Phase 4) and the string/math gaps (Phase 5). The `debug` stub is deliberate
recon and gets replaced in Phase 7; the `calls.lua` tail-call stop is new and
may deserve its own phase.

No secrets, credentials, or personal data are included in this handoff.
