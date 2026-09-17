//! Two Lua programs interleaved by the host on strict fuel budgets:
//! the embedder decides exactly how much each script runs per slice.
//!
//!     cargo run --example budget

use color_eyre::eyre;
use slew::{Lua, Step};

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let mut lua = Lua::new();

    let fast = lua.load("fast = 0 while true do fast = fast + 1 end")?;
    let slow = lua.load("slow = 0 while true do slow = slow + 1 end")?;

    let mut fast_exec = lua.execute(&fast);
    let mut slow_exec = lua.execute(&slow);

    // round-robin scheduler: `fast` gets 10x the fuel of `slow`
    for round in 1..=5 {
        let _ = fast_exec.step(&mut lua, 10_000)?;
        let _ = slow_exec.step(&mut lua, 1_000)?;
        println!(
            "round {round}: fast = {}, slow = {}",
            lua.display_value(lua.get_global("fast")),
            lua.display_value(lua.get_global("slow")),
        );
    }

    // a script that terminates returns its values through Step::Done
    let sum = lua.load("local s = 0 for i = 1, 100 do s = s + i end return s")?;
    let mut exec = lua.execute(&sum);
    loop {
        match exec.step(&mut lua, 50)? {
            Step::Done(vals) => {
                println!("sum finished: {}", lua.display_value(vals[0]));
                break;
            }
            Step::Pending => println!("sum: not done yet, giving it another 50 fuel"),
            Step::Waiting(wait) => eyre::bail!("unexpected native wait: {wait:?}"),
        }
    }
    Ok(())
}
