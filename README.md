# suslua

A suspendable Lua 5.4 interpreter in Rust. Load a script once, then run it with an
exact fuel budget — the embedder controls precisely how fast a Lua program executes.

```rust
let mut lua = Lua::new();
let chunk = lua.load(r#"
    local n = 0
    while true do n = n + 1 end
"#)?;

let mut exec = lua.execute(&chunk);
loop {
    match exec.step(&mut lua, 1_000)? {
        Step::Done(values) => break,
        Step::Pending => { /* script paused after ~1000 ops; do other work */ }
    }
}
```

See [DESIGN.md](DESIGN.md) for architecture and roadmap.

## REPL

```
cargo run            # interactive (reedline: multiline editing, history)
cargo run script.lua # run a file
echo 'print(1+2)' | cargo run   # run piped source
```

The REPL runs every input under a fuel budget — a runaway loop suspends
instead of hanging, and `:more` grants it another budget:

```
suslua> n = 0 while true do n = n + 1 end
~ suspended after 1000000 fuel (:more to continue, new input to abandon)
suslua> n
(abandoned suspended execution)
333327
```

Commands: `:fuel N`, `:more`, `:mem`, `:gc`, `:help`, `:quit`.

The `repl` feature (on by default) pulls in reedline; library consumers can
use `default-features = false`.
