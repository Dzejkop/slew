# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/Dzejkop/slew/releases/tag/v0.1.0) - 2026-09-12

### Other

- add release-plz releases with crates.io trusted publishing
- run tests and Lua conformance through the Nix flake
- *(lint)* enforce clippy all + pedantic and rustfmt
- Add C-call yield boundaries and debug hooks
- Fix the last three upstream conformance stops
- Match PUC 5.4 short/long string identity
- Model synthetic C frames for pcall/xpcall and coroutine.close
- Make GC roots precise per frame register extent
- Implement string.dump and binary chunk loading
- Keep closing and chain errors when __close raises while unwinding
- Match PUC 5.4 math.random xoshiro256** exactly
- Accept numeric strings in numeric for control/limit/step
- Add regression tests for the five conformance fixes
- Fix five upstream conformance blockers
- Fix io number reads and native finalizer draining
- Add capability-gated io/os via host trait and userdata values
- Add polish-pass regression tests and fix small-array sort guard
- Fix lexer escapes, signed tonumber, sort limits, and xpcall handler errors
- Fix collectgarbage("step") cycle semantics and add gc.lua to the suite
- Implement weak tables, __gc finalizers, collectgarbage, and coroutine.close
- Enforce PUC goto/label scoping and const/close attributes
- Accept native functions in the debug upvalue API and add getinfo 'r'
- Fix debug.getinfo line metadata, level coercion, and upvalue API validation
- Implement debug.getinfo/traceback/upvalues and the debug-metatable API
- Implement proper tail calls
- Fix string.pack default alignment and add tpack.lua to the corpus
- Implement string.pack/unpack/packsize, %a, and math gaps
- Add the utf8 library, %z pattern class, and %p formatting
- Honor __tostring/__name in tostring, __pairs and __index in pairs/ipairs
- Implement table.move and metamethod-aware table.remove/concat/unpack
- Add package/require, load/loadfile, and the host file-reader seam
- Add slew-tests.md handoff: conformance harness state and missing-API plan
- Add a three-column TUI example with independent fuel rates
- Fix Lua 5.4 edge-case conformance bugs and add the upstream suite harness
- Add Nix flake with dev shell, package, and app
- Add research notes on Lua conformance test suites
- Rename suslua to slew
- Add REPL/script-runner binary (reedline, optional 'repl' feature)
- mark-sweep GC and memory budget
- stdlib — string with Lua patterns, table, math, Lua prelude
- coroutines and to-be-closed variables
- metatables, metamethods, error values, pcall/xpcall, goto
- Add round-robin scheduling example, non-mutating get_global, clippy fixes
- Core runtime: bytecode compiler, stackless VM, fuel-budgeted stepping
- full Lua 5.4 grammar to AST
- full Lua 5.4 lexical grammar
- Scaffold suslua: design doc and crate skeleton
