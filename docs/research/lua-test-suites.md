# Lua test suites for `slew`

Research date: 2026-09-10

## Recommendation

Use the official **Lua 5.4.9 release test archive** as the long-term conformance corpus, but initially run a curated language-only subset through a small Rust harness. Do not expect the upstream `all.lua` runner to work unchanged yet.

The archive is the best fit because `slew` explicitly targets Lua 5.4 ([README](../../README.md), [design](../../DESIGN.md)), and Lua publishes a test archive for every 5.4 patch release through 5.4.9. Lua says to use the exact release when possible and warns that suites do not work across different major/minor versions. The official 5.4.9 archive and its SHA-256 are listed on the [Lua test-suite page](https://www.lua.org/tests/) (direct archive: [lua-5.4.9-tests.tar.gz](https://www.lua.org/tests/lua-5.4.9-tests.tar.gz)).

The project does not declare a 5.4 patch target. Start with 5.4.9 because it is the current maintained 5.4 suite; if a failure looks tied to a post-5.4.8 bug fix, compare with the [5.4.8 archive](https://www.lua.org/tests/lua-5.4.8-tests.tar.gz). Pin the selected archive and published checksum in CI rather than following a moving branch.

## What exists upstream

| Corpus | Target | Harness and requirements | Reuse here |
| --- | --- | --- | --- |
| [Official Lua release suites](https://www.lua.org/tests/) | Exact Lua releases, including 5.4.0–5.4.9 | Basic: run `lua -e"_U=true" all.lua` from the extracted suite directory. Full: run `all.lua`, build native modules under `libs/`, and provide the standard C API/dynamic loader. Internal: compile upstream with `ltests.c`, `ltests.h`, and `LUA_USER_H`. | **Best source.** Basic-mode Lua files can be curated and adapted. Full and internal modes are not black-box compatible with this Rust interpreter. |
| [`lua/lua/testes`](https://github.com/lua/lua/tree/master/testes) | Current development Lua (currently 5.5) | Development runner, standard libraries, dump/load round trips, and optional internal `T` hooks; see its [`all.lua`](https://github.com/lua/lua/blob/master/testes/all.lua). | Useful later for watching new regressions, not as the 5.4 baseline. The Lua team's [README](https://github.com/lua/lua/blob/master/README.md) calls this an irregular mirror and points users to Lua.org releases. |
| [`lua/tests`](https://github.com/lua/tests) | Historical Lua 5.3 snapshot | Cloneable mostly-Lua corpus; its [`all.lua`](https://github.com/lua/tests/blob/master/all.lua) hard-checks `Lua 5.3`. | Wrong language version and stale. It offers no advantage over the 5.4 release archive. |

Lua describes the release suite as unsupported internal tooling. Its basic mode sets `_U=true`, which skips internal, nonportable, long-running, and high-memory cases; success is reaching `final OK`. Complete mode deliberately exercises system-dependent library and C-API corners, while internal mode must be compiled into PUC-Lua itself. These distinctions and commands come from the [official test instructions](https://www.lua.org/tests/).

## Compatibility with the current project

The language core is promising: the project claims the full 5.4 lexical grammar, parser, core VM, metamethods, errors, coroutines, to-be-closed variables, patterns, and selected standard libraries ([design](../../DESIGN.md)). Existing Rust tests cover those areas under `tests/`.

The upstream runner is nevertheless **not directly runnable**:

- The CLI accepts a script path or stdin, but not Lua's `-e` option ([REPL runner](../../src/bin/repl.rs)). A wrapper or Rust integration harness must set `_U` before execution.
- The installed base environment does not define `_G`, `_VERSION`, `arg`, `load`, `loadfile`, `dofile`, `collectgarbage`, `warn`, or `require` ([stdlib installer](../../src/stdlib/mod.rs), [VM initialization](../../src/vm.rs)). `all.lua` needs these immediately.
- `io`, `os`, `package`, `debug`, and `utf8` are absent; `os` and `io` are explicit non-goals for the sandboxed runtime ([design](../../DESIGN.md)). The official runner uses all of them for orchestration and coverage.
- Library coverage is intentionally partial. Examples needed by the suite but currently absent include `string.dump`, `string.pack`, `string.unpack`, `string.packsize`, `table.move`, `coroutine.close`, `math.deg`, and `math.rad`. Known deviations also include no weak tables/finalizers and no `%a` in `string.format` ([design](../../DESIGN.md)).
- Full mode's C modules and internal mode's `T`/`ltests` hooks assume PUC-Lua's C API and internals. The C API is a stated non-goal, so those modes should be classified as out of scope rather than treated as failing conformance.

As a quick direct-use check, running the archive's standalone `vararg.lua` with the already-built `target/debug/slew` reached the test body but failed at line 13 (`arg == _G.arg`) because `_G` is nil. This confirms that even relatively self-contained files need a compatibility prelude or selective extraction; it does not indicate that vararg semantics themselves failed.

## Practical adoption plan

1. Vendor or fetch the pinned 5.4.9 archive with its published checksum and retain its license notice.
2. Add a Rust integration harness that creates one `Lua` state, installs test-only `_G`, `_VERSION = "Lua 5.4"`, `arg`, and `_U`, loads scripts by path, and drives executions to completion with an explicit high fuel ceiling.
3. Begin with language-focused files such as `constructs.lua`, `locals.lua`, `vararg.lua`, `closure.lua`, `goto.lua`, `calls.lua`, `events.lua`, `bitwise.lua`, and `literals.lua`. Maintain a manifest of skipped assertions/files with one reason per skip; these files still use helper globals and dynamic loading in places, so curate by observed dependency rather than filename alone.
4. Add library-focused files incrementally as APIs land (`pm.lua`, `strings.lua`, `sort.lua`, `math.lua`, `coroutine.lua`, GC files). Separate expected design deviations from bugs.
5. Do not make upstream `all.lua` itself the first milestone. A complete run requires broad standard-library, filesystem, module-loading, bytecode-dump, debug, and C-API compatibility that the project does not currently intend to provide.

## License

The test page says all archives use the Lua license. It is the MIT license: use, modification, redistribution, sublicensing, and sale are allowed, provided the copyright and permission notice remain in copies or substantial portions. See Lua's [official license](https://www.lua.org/copyright.html). Vendoring an adapted subset is therefore allowed; preserve the notice and clearly mark local modifications.

## Bottom line

A strong, authoritative test corpus exists and is legally reusable. For `slew`, its immediate value is as a **curated 5.4 language conformance suite**, not a drop-in full-runtime test command. The highest-leverage next step is the test-only harness plus an explicit allow/skip manifest; that would expose semantic gaps without forcing `os`, `io`, `debug`, package loading, or the C API into the product.
