//! Browser bindings for slew.
//!
//! Wraps a [`Lua`] VM in a [`Session`] that JavaScript drives step by step:
//! [`Session::run`] compiles and starts a chunk under a fuel budget,
//! [`Session::resume`] grants a suspended run more fuel, and
//! [`Session::abort`] abandons it. `print` output is captured in a buffer
//! the page drains with [`Session::take_output`].

use std::cell::RefCell;

use slew::{Error, Execution, Lua, Step, Value};
use wasm_bindgen::prelude::*;

thread_local! {
    /// Captured `print` output. A thread-local (rather than per-session
    /// state) because native functions are plain `fn` pointers.
    static OUTPUT: RefCell<String> = const { RefCell::new(String::new()) };
}

fn capture_print(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let line = args
        .iter()
        .map(|v| lua.display_value(*v))
        .collect::<Vec<_>>()
        .join("\t");
    OUTPUT.with(|out| {
        let mut out = out.borrow_mut();
        out.push_str(&line);
        out.push('\n');
    });
    Ok(Vec::new())
}

fn repr(lua: &Lua, v: Value) -> String {
    match v {
        Value::Str(_) => format!("{:?}", lua.display_value(v)),
        _ => lua.display_value(v),
    }
}

fn js_error(e: Error) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// A Lua session the page can run and suspend.
#[wasm_bindgen]
pub struct Session {
    lua: Lua,
    exec: Option<Execution>,
    result: String,
}

#[wasm_bindgen]
impl Session {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Session {
        console_error_panic_hook::set_once();
        let mut lua = Lua::new();
        lua.register_native("print", capture_print);
        Session {
            lua,
            exec: None,
            result: String::new(),
        }
    }

    /// Compiles `src` and runs it for at most `fuel` VM instructions,
    /// returning `"done"` or `"suspended"`. Parse, compile, and runtime
    /// errors are thrown as JS exceptions. Any previous run is abandoned.
    pub fn run(&mut self, src: &str, fuel: u32) -> Result<String, JsValue> {
        self.abort();
        self.result.clear();
        self.take_output();
        let chunk = self.lua.load(src).map_err(js_error)?;
        self.exec = Some(self.lua.execute(&chunk));
        self.drive(fuel)
    }

    /// Grants a suspended run another `fuel` budget.
    pub fn resume(&mut self, fuel: u32) -> Result<String, JsValue> {
        self.drive(fuel)
    }

    /// Abandons the suspended run, if any, releasing its VM state.
    pub fn abort(&mut self) {
        if let Some(exec) = self.exec.take() {
            exec.abort(&mut self.lua);
        }
    }

    /// Drains output produced by `print`.
    pub fn take_output(&mut self) -> String {
        OUTPUT.with(|out| std::mem::take(&mut *out.borrow_mut()))
    }

    /// `tostring`-style rendering of the values returned by the last run.
    pub fn result(&self) -> String {
        self.result.clone()
    }

    /// `source:line` of the next instruction of a suspended run.
    pub fn location(&self) -> Option<String> {
        let (source, line) = self.exec.as_ref()?.current_location(&self.lua)?;
        Some(format!("{source}:{line}"))
    }

    /// Current value of a global, rendered like the REPL; `undefined` if nil.
    pub fn global(&self, name: &str) -> Option<String> {
        match self.lua.get_global(name) {
            Value::Nil => None,
            v => Some(repr(&self.lua, v)),
        }
    }

    pub fn memory_used(&self) -> usize {
        self.lua.memory_used()
    }

    pub fn collect_garbage(&mut self) -> usize {
        self.lua.gc()
    }

    pub fn is_suspended(&self) -> bool {
        self.exec.as_ref().is_some_and(|exec| !exec.is_finished())
    }

    fn drive(&mut self, fuel: u32) -> Result<String, JsValue> {
        let Some(exec) = self.exec.as_mut() else {
            return Ok("idle".into());
        };
        match exec.step(&mut self.lua, fuel as u64) {
            Ok(Step::Pending) => Ok("suspended".into()),
            Ok(Step::Done(values)) => {
                self.result = values
                    .iter()
                    .map(|v| repr(&self.lua, *v))
                    .collect::<Vec<_>>()
                    .join("\t");
                self.exec = None;
                Ok("done".into())
            }
            Err(e) => {
                self.exec = None;
                Err(js_error(e))
            }
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
