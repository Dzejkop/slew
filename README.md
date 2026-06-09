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
