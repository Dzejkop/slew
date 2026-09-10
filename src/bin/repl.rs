//! Interactive REPL for slew.
//!
//! Doubles as a demo of the execution-profile machinery: every input runs
//! under a fuel budget; a runaway computation suspends instead of hanging
//! the terminal, and `:more` grants it another budget.

use std::borrow::Cow;
use std::io::{IsTerminal, Read};

use reedline::{
    DefaultHinter, Prompt, PromptEditMode, PromptHistorySearch, Reedline, Signal,
    ValidationResult, Validator,
};
use slew::{Execution, Lua, Step, Value};

const DEFAULT_FUEL: u64 = 1_000_000;

fn main() {
    // script-runner modes: `slew file.lua`, or piped stdin
    if let Some(path) = std::env::args().nth(1) {
        let src = std::fs::read(&path).unwrap_or_else(|e| {
            eprintln!("slew: cannot read {path}: {e}");
            std::process::exit(1);
        });
        run_script(&path, &src);
        return;
    }
    if !std::io::stdin().is_terminal() {
        let mut src = Vec::new();
        std::io::stdin().read_to_end(&mut src).expect("read stdin");
        run_script("stdin", &src);
        return;
    }
    repl();
}

/// Runs a whole script to completion (stepped, so the process stays
/// drivable; an unbounded script simply keeps running).
fn run_script(name: &str, src: &[u8]) {
    let mut lua = Lua::new();
    let chunk = match lua.load_named(name, src) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("slew: {e}");
            std::process::exit(1);
        }
    };
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(&mut lua, 10_000_000) {
            Ok(Step::Done(_)) => return,
            Ok(Step::Pending) => continue,
            Err(e) => {
                eprintln!("slew: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn repl() {
    let mut lua = Lua::new();
    let mut editor = Reedline::create()
        .with_validator(Box::new(LuaValidator))
        .with_hinter(Box::new(DefaultHinter::default()));
    let mut fuel = DEFAULT_FUEL;
    let mut suspended: Option<Execution> = None;

    println!("slew {} — a suspendable Lua 5.4", env!("CARGO_PKG_VERSION"));
    println!("fuel budget per input: {fuel} (:help for commands)");

    loop {
        match editor.read_line(&LuaPrompt) {
            Ok(Signal::Success(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(cmd) = line.strip_prefix(':') {
                    handle_command(cmd, &mut lua, &mut fuel, &mut suspended);
                    continue;
                }
                // new input abandons any suspended execution
                if let Some(old) = suspended.take() {
                    old.abort(&mut lua);
                    println!("(abandoned suspended execution)");
                }
                // expression first (auto-print), statement otherwise
                let chunk = match lua.load_named("repl", format!("return {line}")) {
                    Ok(c) => c,
                    Err(_) => match lua.load_named("repl", line) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("{e}");
                            continue;
                        }
                    },
                };
                let exec = lua.execute(&chunk);
                suspended = drive(&mut lua, exec, fuel);
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Ok(_) => continue, // Signal is non-exhaustive
            Err(e) => {
                eprintln!("input error: {e}");
                break;
            }
        }
    }
}

/// Runs an execution for one fuel budget; returns it if still suspended.
fn drive(lua: &mut Lua, mut exec: Execution, fuel: u64) -> Option<Execution> {
    match exec.step(lua, fuel) {
        Ok(Step::Done(vals)) => {
            if !vals.is_empty() {
                let line =
                    vals.iter().map(|v| repr(lua, *v)).collect::<Vec<_>>().join("\t");
                println!("{line}");
            }
            None
        }
        Ok(Step::Pending) => {
            println!("~ suspended after {fuel} fuel (:more to continue, new input to abandon)");
            Some(exec)
        }
        Err(e) => {
            eprintln!("{e}");
            None
        }
    }
}

/// `tostring`-style rendering, with strings quoted so `"1"` and `1` are
/// distinguishable at the prompt.
fn repr(lua: &Lua, v: Value) -> String {
    match v {
        Value::Str(_) => format!("{:?}", lua.display_value(v)),
        _ => lua.display_value(v),
    }
}

fn handle_command(cmd: &str, lua: &mut Lua, fuel: &mut u64, suspended: &mut Option<Execution>) {
    let mut parts = cmd.split_whitespace();
    match parts.next().unwrap_or("") {
        "more" | "m" => match suspended.take() {
            Some(exec) => *suspended = drive(lua, exec, *fuel),
            None => println!("nothing is suspended"),
        },
        "fuel" => match parts.next().and_then(|s| s.parse::<u64>().ok()) {
            Some(n) => {
                *fuel = n;
                println!("fuel budget per input: {n}");
            }
            None => println!("fuel budget per input: {fuel}  (usage: :fuel N)"),
        },
        "mem" => println!("~{} bytes in use", lua.memory_used()),
        "gc" => {
            let after = lua.gc();
            println!("collected; ~{after} bytes in use");
        }
        "help" | "h" => {
            println!(":fuel N   set the per-input fuel budget (currently {fuel})");
            println!(":more     resume a suspended execution with another budget");
            println!(":mem      approximate memory in use");
            println!(":gc       run a garbage collection");
            println!(":quit     exit (also Ctrl-D)");
        }
        "quit" | "q" | "exit" => std::process::exit(0),
        other => println!("unknown command ':{other}' (:help)"),
    }
}

struct LuaPrompt;

impl Prompt for LuaPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("slew")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _edit_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("..> ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("search: ")
    }
}

/// Multiline editing: Enter inserts a newline while the input is an
/// incomplete Lua chunk (detected like PUC's REPL: the parse error sits at
/// `<eof>`), and submits once it parses (or fails for a non-eof reason).
struct LuaValidator;

impl Validator for LuaValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if is_incomplete(line) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

fn is_incomplete(src: &str) -> bool {
    use slew::parser::parse;
    if parse(format!("return {src}").as_bytes()).is_ok() {
        return false;
    }
    match parse(src.as_bytes()) {
        Ok(_) => false,
        Err(e) => e.to_string().contains("<eof>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_detection() {
        // complete inputs submit
        assert!(!is_incomplete("1 + 2"));
        assert!(!is_incomplete("x = 1"));
        assert!(!is_incomplete("function f() return 1 end"));
        assert!(!is_incomplete("for i = 1, 3 do print(i) end"));
        // unfinished constructs keep the editor open
        assert!(is_incomplete("function f()"));
        assert!(is_incomplete("if x then"));
        assert!(is_incomplete("local t = {"));
        assert!(is_incomplete("while true do"));
        assert!(is_incomplete("print('unterminated call'"));
        // genuinely broken input submits (so the error gets shown)
        assert!(!is_incomplete("x = = 1"));
        assert!(!is_incomplete(")("));
    }

    #[test]
    fn expression_vs_statement_and_suspension() {
        let mut lua = Lua::new();
        // expression path: `return <line>` parses
        let chunk = lua.load_named("repl", "return 6 * 7").unwrap();
        let exec = lua.execute(&chunk);
        assert!(drive(&mut lua, exec, 1_000).is_none());
        // statement path: side effects persist across inputs
        let chunk = lua.load_named("repl", "answer = 42").unwrap();
        let exec = lua.execute(&chunk);
        assert!(drive(&mut lua, exec, 1_000).is_none());
        assert_eq!(lua.get_global("answer"), Value::Int(42));
        // runaway input suspends instead of hanging; :more resumes it
        let chunk = lua.load_named("repl", "n = 0 while true do n = n + 1 end").unwrap();
        let exec = lua.execute(&chunk);
        let suspended = drive(&mut lua, exec, 5_000);
        assert!(suspended.is_some());
        let Value::Int(n1) = lua.get_global("n") else { panic!() };
        let still = drive(&mut lua, suspended.unwrap(), 5_000);
        assert!(still.is_some());
        let Value::Int(n2) = lua.get_global("n") else { panic!() };
        assert!(n2 > n1, "resume must make progress");
        // abandoning releases the execution
        still.unwrap().abort(&mut lua);
    }

    #[test]
    fn repr_quotes_strings() {
        let mut lua = Lua::new();
        let s = lua.new_string(b"hi");
        assert_eq!(repr(&lua, s), "\"hi\"");
        assert_eq!(repr(&lua, Value::Int(1)), "1");
    }
}
