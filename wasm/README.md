# slew in the browser

A [wasm-bindgen](https://wasm-bindgen.github.io/wasm-bindgen/) wrapper plus a
small page that drives slew step by step. The demo is the point of the
project: a `while true` loop suspends after its fuel budget and hands control
back to the browser, and `Resume` (or `auto-step`) grants it another budget.

## Build

Requires a Rust toolchain with the `wasm32-unknown-unknown` target
(`rustup target add wasm32-unknown-unknown`), `wasm-pack`, and a `wasm-bindgen`
CLI matching the crate version pinned in `Cargo.toml`.

The crate is built with the `repl` feature disabled (`slew` is a path
dependency with `default-features = false`).

```
wasm-pack build --dev --target web --out-dir www/pkg
```

`--dev` skips `wasm-opt`; drop it for a release build if you have Binaryen
installed.

Note: the repository's Nix dev shell ships a rustc without `rust-lld`, so the
wasm link step fails there. Point the build at a rustup toolchain instead:

```
PATH="$HOME/.cargo/bin:$PATH" wasm-pack build --dev --target web --out-dir www/pkg
```

## Run

`init()` fetches the `.wasm` file, so the page needs to be served over HTTP:

```
python3 -m http.server -d www 8000
# then open http://localhost:8000
```

## Test

`smoke.mjs` loads the wasm synchronously and exercises the bindings under
Node — no browser needed:

```
node smoke.mjs
```

## The bindings

`wasm/src/lib.rs` exposes a `Session`:

| method | purpose |
| --- | --- |
| `run(src, fuel)` | compile and start `src`, run at most `fuel` VM instructions; returns `"done"` or `"suspended"` |
| `resume(fuel)` | grant a suspended run another budget |
| `abort()` | abandon a suspended run and release its VM state |
| `take_output()` | drain `print` output captured since the last call |
| `result()` | return values of the last completed run, `tostring`-style |
| `global(name)` | render a global, or `undefined` if nil |
| `location()` | `source:line` of the next instruction of a suspended run |
| `is_suspended()` | whether a run is parked |
| `memory_used()` / `collect_garbage()` | VM memory introspection |

`print` is intercepted by the demo (native functions are `fn` pointers, so the
buffer is a thread-local) and harvested from JS after every step. The buffer is
process-wide, not per-`Session`: keep one `Session` alive per page. Parse,
compile, and runtime errors are thrown as JS exceptions.
