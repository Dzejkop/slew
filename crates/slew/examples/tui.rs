//! Three Lua programs sharing one `Lua<Ctx>`, each with its own execution
//! context, talking over channels.
//!
//! Channels live in Lua (the global `channels`) so message values are ordinary
//! GC'd Lua values; the two natives only *suspend*:
//!
//! ```lua
//! ch.send(chan, msg)      -- blocks while the channel is full, then enqueues
//! ch.recv(chan)           -- blocks while empty, then dequeues
//! ch.try_send(chan, msg)  -- -> true | nil, "full"
//! ch.try_recv(chan)       -- -> msg  | nil, "empty"
//! ```
//!
//! Channels are identified by numbers and are created on first use with
//! capacity 8. What a message *means* is entirely up to the Lua in each
//! column: 1 computes factorials, 2 repeats the number, 3 logs it.
//!
//! The input box is a Lua prompt evaluated against the same VM. It may block
//! too, and `c` abandons whatever it is running — a cancel is pure
//! abandonment (frames dropped, GC roots released, no unwinding, no `__close`),
//! which is safe here because the wait natives never mutate Lua state: the
//! dequeue/enqueue happens only after a wait returns, so cancelling a blocked
//! `ch.recv` cannot lose a message.
//!
//! cargo run --example tui
//!
//! Tab switches between two modes:
//!   typing   — Enter run · Backspace edit · Esc clear
//!   controls — 1/2/3 select · e edit · c cancel · space pause · +/- speed
//!              · 0 reset · q quit
//!
//! `e` opens the selected column's source in `$VISUAL`/`$EDITOR` (or `vi`);
//! saving with changes restarts that program.

use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use slew::{Error, Execution, Lua, NativeContext, NativeOutcome, NativeWait, Step, Value};

/// Cap on retained log lines so a chatty program cannot grow without bound.
const MAX_LOG_LINES: usize = 512;
/// Fuel for one prompt step: interactive, so effectively "until it settles".
const PROMPT_FUEL: u64 = 1_000_000;
/// Fuel for running the channel prelude to completion.
const PRELUDE_FUEL: u64 = 10_000_000;

/// Runtime 1: factorial of each received number.
const FACTORIAL: &str = r#"
-- runtime 1: factorial
while true do
  local n = ch.recv(1)
  local r = 1
  for i = 2, n do
    r = r * i
  end
  log("factorial(" .. n .. ") = " .. r)
end
"#;

/// Runtime 2: the number itself, repeated n times.
const ECHO: &str = r"
-- runtime 2: n copies
while true do
  local n = ch.recv(2)
  for _ = 1, n do
    log(n)
  end
end
";

/// Runtime 3: whatever it is sent, once.
const ONCE: &str = r"
-- runtime 3: once
while true do
  log(ch.recv(3))
end
";

/// Channel plumbing. Values live in this table, so they are traced by the GC
/// like any other Lua data; `__ch_wait_*` only suspends the caller.
const CHANNEL_PRELUDE: &str = r#"
channels = {}

local DEFAULT_CAP = 8

local function get(chan)
  local c = channels[chan]
  if not c then
    c = { first = 1, last = 0, cap = DEFAULT_CAP, items = {} }
    channels[chan] = c
  end
  return c
end

local function count(c)
  return c.last - c.first + 1
end

ch = {}

function ch.try_recv(chan)
  local c = get(chan)
  if count(c) <= 0 then return nil, "empty" end
  local v = c.items[c.first]
  c.items[c.first] = nil
  c.first = c.first + 1
  return v
end

function ch.try_send(chan, msg)
  local c = get(chan)
  if count(c) >= c.cap then return nil, "full" end
  c.last = c.last + 1
  c.items[c.last] = msg
  return true
end

function ch.recv(chan)
  local c = get(chan)
  while count(c) <= 0 do
    __ch_wait_nonempty(chan)
  end
  local v = c.items[c.first]
  c.items[c.first] = nil
  c.first = c.first + 1
  return v
end

function ch.send(chan, msg)
  local c = get(chan)
  while count(c) >= c.cap do
    __ch_wait_room(chan)
  end
  c.last = c.last + 1
  c.items[c.last] = msg
  return true
end
"#;

/// What a parked execution is waiting for. Read by the host to decide when a
/// suspended native call can be completed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WaitKind {
    NonEmpty(i64),
    Room(i64),
}

impl WaitKind {
    fn channel(self) -> i64 {
        match self {
            WaitKind::NonEmpty(c) | WaitKind::Room(c) => c,
        }
    }
}

/// Per-execution context. Channels themselves live in Lua; this only carries
/// the log handle and the current wait.
struct Ctx {
    label: String,
    log: Rc<RefCell<Vec<String>>>,
    waiting: Option<WaitKind>,
    waits: u64,
}

impl Ctx {
    fn program(label: String, log: Rc<RefCell<Vec<String>>>) -> Self {
        Self {
            label,
            log,
            waiting: None,
            waits: 0,
        }
    }

    fn prompt(log: Rc<RefCell<Vec<String>>>) -> Self {
        Self::program(">".into(), log)
    }
}

struct Runtime {
    id: i64,
    label: String,
    path: PathBuf,
    source: String,
    exec: Execution<Ctx>,
    /// Base fuel per simulated second, before the global speed multiplier.
    rate: f64,
    /// Fractional fuel carried between frames.
    fuel_acc: f64,
    /// Set while the execution is suspended on a `__ch_wait_*` native.
    wait: Option<NativeWait>,
    line: Option<u32>,
    finished: bool,
    error: Option<String>,
}

impl Runtime {
    /// All runtimes share one `Lua<Ctx>`; only the execution and its context
    /// are per-runtime.
    fn new(
        lua: &mut Lua<Ctx>,
        id: i64,
        source: &str,
        rate: f64,
        log: Rc<RefCell<Vec<String>>>,
        dir: &Path,
    ) -> Self {
        let label = id.to_string();
        let path = dir.join(format!("{id}.lua"));
        let _ = std::fs::write(&path, source);
        let exec = spawn_program(lua, &label, source, Ctx::program(label.clone(), log));
        Self {
            id,
            label,
            path,
            source: source.to_string(),
            exec,
            rate,
            fuel_acc: 0.0,
            wait: None,
            line: None,
            finished: false,
            error: None,
        }
    }

    fn status(&self) -> String {
        if let Some(e) = &self.error {
            return format!("error: {e}");
        }
        if self.finished {
            return "done".into();
        }
        match self.exec.context().waiting {
            Some(WaitKind::NonEmpty(c)) => return format!("waiting on ch {c}"),
            Some(WaitKind::Room(c)) => return format!("waiting for room on ch {c}"),
            None => {}
        }
        self.line
            .map_or_else(|| "…".into(), |l| format!("line {l}"))
    }

    /// Grants `rate × speed × dt` fuel; the fraction accrues across frames so
    /// a sub-1-fuel frame is not lost.
    fn advance(&mut self, lua: &mut Lua<Ctx>, dt: Duration, speed: f64) {
        if self.finished || self.wait.is_some() {
            // Parked on a channel: bank nothing, or a long wait would release
            // one huge budget the moment a message arrives.
            self.fuel_acc = 0.0;
            return;
        }
        self.fuel_acc += self.rate * speed * dt.as_secs_f64();
        let fuel = self.fuel_acc.floor();
        if fuel < 1.0 {
            return;
        }
        self.fuel_acc -= fuel;
        match self.exec.step(lua, fuel as u64) {
            Ok(Step::Done(_)) => self.finished = true,
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(wait)) => self.wait = Some(wait),
            Err(e) => {
                self.finished = true;
                if let Error::Runtime(rt) = &e {
                    self.line = Some(rt.line);
                }
                self.error = Some(e.to_string());
            }
        }
        if !self.finished
            && let Some((source, line)) = self.exec.current_location(lua)
            && source == self.label
        {
            self.line = Some(line);
        }
    }
}

/// A prompt invocation. It may block like any other program, so it persists
/// across frames and is stepped alongside the runtimes.
struct Prompt {
    exec: Execution<Ctx>,
    wait: Option<NativeWait>,
}

struct App {
    lua: Lua<Ctx>,
    runtimes: Vec<Runtime>,
    log: Rc<RefCell<Vec<String>>>,
    prompt: Option<Prompt>,
    input: String,
    /// Index of the selected column (for `e`).
    selected: usize,
    /// Global multiplier applied to every runtime's rate.
    speed: f64,
    paused: bool,
    /// True while keystrokes go to the prompt instead of the controls.
    editing: bool,
}

impl App {
    fn new() -> Self {
        let mut lua = Lua::<Ctx>::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        install(&mut lua, &log);

        let dir = sources_dir();
        let runtimes = [(1, FACTORIAL, 80.0), (2, ECHO, 400.0), (3, ONCE, 80.0)]
            .into_iter()
            .map(|(id, source, rate)| {
                Runtime::new(&mut lua, id, source, rate, Rc::clone(&log), &dir)
            })
            .collect();

        let app = Self {
            lua,
            runtimes,
            log,
            prompt: None,
            input: String::new(),
            selected: 0,
            speed: 1.0,
            paused: false,
            editing: true,
        };
        app.push_log("[ui] channels: ch.send(1, 5) · ch.recv(n) blocks".into());
        app.push_log(format!("[ui] sources: {}", dir.display()));
        app
    }

    fn push_log(&self, line: String) {
        push_log(&self.log, line);
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }

        if let Some(prompt) = self.prompt.take() {
            if prompt.wait.is_some() {
                self.prompt = Some(prompt);
                self.push_log("[ui] prompt is already waiting — press c to cancel".into());
                return;
            }

            // Still running: abandon it so the new input can run.
            prompt.exec.abort(&mut self.lua);
        }

        self.input.clear();

        match self.lua.load_named("=prompt", &text) {
            Ok(chunk) => {
                let exec = self
                    .lua
                    .execute_with_context(&chunk, Ctx::prompt(Rc::clone(&self.log)));
                self.prompt = Some(Prompt { exec, wait: None });
            }
            Err(e) => self.push_log(format!("[!] {e}")),
        }
    }

    /// Abandons the current prompt invocation. Safe without rollback because
    /// the wait natives never mutate: the channel update happens after the
    /// wait returns, so a cancelled `ch.recv` leaves its message queued.
    fn cancel_prompt(&mut self) {
        match self.prompt.take() {
            Some(prompt) => {
                prompt.exec.abort(&mut self.lua);
                self.push_log("[ui] prompt cancelled".into());
            }
            None => self.push_log("[ui] nothing to cancel".into()),
        }
    }

    fn bump_speed(&mut self, factor: f64) {
        self.speed = (self.speed * factor).clamp(1.0 / 16.0, 16.0);
    }

    fn tick(&mut self, dt: Duration) {
        let speed = self.speed;
        // Wake anyone whose channel is ready before granting new fuel.
        self.deliver_pending();

        for runtime in &mut self.runtimes {
            runtime.advance(&mut self.lua, dt, speed);
        }
        self.advance_prompt();

        // A send from this frame can wake a column in the same frame.
        self.deliver_pending();
    }

    fn deliver_pending(&mut self) {
        for runtime in &mut self.runtimes {
            deliver_if_ready(&mut self.lua, &mut runtime.exec, &mut runtime.wait);
        }

        if let Some(prompt) = &mut self.prompt {
            deliver_if_ready(&mut self.lua, &mut prompt.exec, &mut prompt.wait);
        }
    }

    fn advance_prompt(&mut self) {
        let Some(mut prompt) = self.prompt.take() else {
            return;
        };
        if prompt.wait.is_some() {
            self.prompt = Some(prompt);
            return;
        }
        match prompt.exec.step(&mut self.lua, PROMPT_FUEL) {
            Ok(Step::Done(values)) => {
                if !values.is_empty() {
                    let text = values
                        .iter()
                        .map(|v| self.lua.display_value(*v))
                        .collect::<Vec<_>>()
                        .join("\t");
                    self.push_log(format!("[>] {text}"));
                }
            }
            Ok(Step::Pending) => self.prompt = Some(prompt),
            Ok(Step::Waiting(wait)) => {
                prompt.wait = Some(wait);
                self.prompt = Some(prompt);
            }
            Err(e) => self.push_log(format!("[!] {e}")),
        }
    }

    /// Opens the selected column's source in `$VISUAL`/`$EDITOR`/`vi` and
    /// restarts it if the file changed. The caller restores the terminal
    /// first; the editor needs a normal screen.
    fn edit_externally(&mut self, index: usize) {
        let Some(runtime) = self.runtimes.get(index) else {
            return;
        };
        let path = runtime.path.clone();
        let source = runtime.source.clone();
        if let Err(e) = std::fs::write(&path, &source) {
            self.push_log(format!("[!] cannot write {}: {e}", path.display()));
            return;
        }
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());
        match Command::new(&editor).arg(&path).status() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                self.push_log(format!("[!] {editor} exited with {status}"));
                return;
            }
            Err(e) => {
                self.push_log(format!("[!] cannot run {editor}: {e}"));
                return;
            }
        }
        match std::fs::read_to_string(&path) {
            Ok(new_source) if new_source == source => {
                self.push_log(format!("[ui] {} unchanged", self.runtimes[index].label));
            }
            Ok(new_source) => {
                self.runtimes[index].source = new_source;
                self.restart(index);
            }
            Err(e) => self.push_log(format!("[!] cannot read {}: {e}", path.display())),
        }
    }

    /// Recompiles a column and swaps in a fresh execution, releasing the old
    /// one's GC roots. On a compile error the running program is left alone.
    fn restart(&mut self, index: usize) {
        let label = self.runtimes[index].label.clone();
        let source = self.runtimes[index].source.clone();
        let chunk = match self.lua.load_named(&label, &source) {
            Ok(chunk) => chunk,
            Err(e) => {
                self.push_log(format!("[!] {label}: {e}"));
                return;
            }
        };
        let exec = self
            .lua
            .execute_with_context(&chunk, Ctx::program(label.clone(), Rc::clone(&self.log)));
        let runtime = &mut self.runtimes[index];
        let old = std::mem::replace(&mut runtime.exec, exec);
        old.abort(&mut self.lua);
        runtime.wait = None;
        runtime.line = None;
        runtime.finished = false;
        runtime.error = None;
        runtime.fuel_acc = 0.0;
        self.push_log(format!("[ui] restarted {label}"));
    }
}

// ---- native functions and helpers ----

/// Suspends the caller until `chan` is non-empty. Mutates nothing.
fn wait_nonempty(
    ctx: &mut NativeContext<'_, Ctx>,
    args: &[Value],
) -> Result<NativeOutcome, String> {
    wait_on(ctx, args, "__ch_wait_nonempty", WaitKind::NonEmpty)
}

/// Suspends the caller until `chan` has room. Mutates nothing.
fn wait_room(ctx: &mut NativeContext<'_, Ctx>, args: &[Value]) -> Result<NativeOutcome, String> {
    wait_on(ctx, args, "__ch_wait_room", WaitKind::Room)
}

fn wait_on(
    ctx: &mut NativeContext<'_, Ctx>,
    args: &[Value],
    name: &str,
    make: fn(i64) -> WaitKind,
) -> Result<NativeOutcome, String> {
    let chan = int_arg(args, 0, name)?;
    let state = ctx.context_mut();
    state.waiting = Some(make(chan));
    state.waits += 1;
    Ok(NativeOutcome::Wait(NativeWait(state.waits)))
}

/// Appends one line to the shared log.
fn log_message(ctx: &mut NativeContext<'_, Ctx>, args: &[Value]) -> Result<NativeOutcome, String> {
    let label = ctx.context().label.clone();
    let log = Rc::clone(&ctx.context().log);
    let text = args
        .iter()
        .map(|v| ctx.display_value(*v))
        .collect::<Vec<_>>()
        .join(" ");
    push_log(&log, format!("[{label}] {text}"));
    Ok(NativeOutcome::Return(Vec::new()))
}

fn int_arg(args: &[Value], index: usize, name: &str) -> Result<i64, String> {
    match args.get(index) {
        Some(Value::Int(n)) => Ok(*n),
        Some(Value::Float(f)) if f.fract() == 0.0 => Ok(*f as i64),
        Some(v) => Err(format!(
            "bad argument #{} to '{name}' (number expected, got {})",
            index + 1,
            v.type_name()
        )),
        None => Err(format!(
            "bad argument #{} to '{name}' (number expected, got no value)",
            index + 1
        )),
    }
}

fn push_log(log: &Rc<RefCell<Vec<String>>>, line: String) {
    let mut lines = log.borrow_mut();
    lines.push(line);
    if lines.len() > MAX_LOG_LINES {
        let excess = lines.len() - MAX_LOG_LINES;
        lines.drain(..excess);
    }
}

/// Re-reads `channels[chan]` from Lua and reports whether `kind` is satisfied.
/// Read-only: the dequeue/enqueue is done by the Lua side once it resumes.
fn channel_ready(lua: &mut Lua<Ctx>, kind: WaitKind) -> bool {
    let channels = lua.get_global("channels");
    if !matches!(channels, Value::Table(_)) {
        return false;
    }
    let entry = lua.table_get(channels, Value::Int(kind.channel()));
    if !matches!(entry, Value::Table(_)) {
        return false;
    }
    let first = field_int(lua, entry, "first");
    let last = field_int(lua, entry, "last");
    let cap = field_int(lua, entry, "cap");
    let queued = last - first + 1;
    match kind {
        WaitKind::NonEmpty(_) => queued > 0,
        WaitKind::Room(_) => queued < cap,
    }
}

fn field_int(lua: &mut Lua<Ctx>, table: Value, name: &str) -> i64 {
    let key = lua.new_string(name.as_bytes());
    match lua.table_get(table, key) {
        Value::Int(n) => n,
        Value::Float(f) => f as i64,
        _ => 0,
    }
}

/// If `exec` is parked on a satisfied condition, hand it back to Lua.
fn deliver_if_ready(lua: &mut Lua<Ctx>, exec: &mut Execution<Ctx>, wait: &mut Option<NativeWait>) {
    let Some(token) = *wait else {
        return;
    };
    let Some(kind) = exec.context().waiting else {
        return;
    };
    if !channel_ready(lua, kind) {
        return;
    }
    if exec.complete_native(lua, token, Ok(Vec::new())).is_ok() {
        *wait = None;
        exec.context_mut().waiting = None;
    }
}

/// Registers the natives and loads the channel prelude.
fn install(lua: &mut Lua<Ctx>, log: &Rc<RefCell<Vec<String>>>) {
    lua.register_suspendable_native("log", log_message);
    lua.register_suspendable_native("__ch_wait_nonempty", wait_nonempty);
    lua.register_suspendable_native("__ch_wait_room", wait_room);

    let chunk = lua
        .load_named("=ch", CHANNEL_PRELUDE)
        .unwrap_or_else(|e| panic!("channel prelude does not compile: {e}"));
    let mut exec = lua.execute_with_context(&chunk, Ctx::program("ch".into(), Rc::clone(log)));
    loop {
        match exec.step(lua, PRELUDE_FUEL) {
            Ok(Step::Done(_)) => break,
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("channel prelude must not wait"),
            Err(e) => panic!("channel prelude failed: {e}"),
        }
    }
}

fn sources_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("slew-tui");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn spawn_program(lua: &mut Lua<Ctx>, name: &str, source: &str, ctx: Ctx) -> Execution<Ctx> {
    let chunk = lua
        .load_named(name, source)
        .unwrap_or_else(|e| panic!("{name} does not compile: {e}"));
    lua.execute_with_context(&chunk, ctx)
}

fn ui(frame: &mut Frame, app: &App) {
    let [columns, log_area, input_area, footer] = Layout::vertical([
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let areas = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(columns);
    for (i, area) in areas.iter().enumerate() {
        draw_runtime(frame, *area, &app.runtimes[i], i == app.selected);
    }

    draw_log(frame, log_area, app);
    draw_input(frame, input_area, app);

    let hint = if app.editing {
        "typing · Tab: controls · Enter run · Backspace edit · Esc clear".to_string()
    } else {
        format!(
            "1/2/3 select · e edit · c cancel · space {} · +/- speed ×{:.2} · 0 reset · q quit",
            if app.paused { "resume" } else { "pause" },
            app.speed,
        )
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        footer,
    );
}

fn draw_runtime(frame: &mut Frame, area: Rect, runtime: &Runtime, selected: bool) {
    let current_line = Style::new().bg(Color::Indexed(236));
    let lines: Vec<Line> = runtime
        .source
        .lines()
        .enumerate()
        .map(|(i, text)| {
            let number = i as u32 + 1;
            let current = runtime.line == Some(number);
            let gutter = Span::styled(
                format!("{number:>3} "),
                if current {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new().fg(Color::DarkGray)
                },
            );
            let mut spans = vec![gutter];
            spans.extend(highlight(text));
            let line = Line::from(spans);
            if current {
                line.style(current_line)
            } else {
                line
            }
        })
        .collect();

    let title = format!(
        " {} {} · {:.0} fuel/s · {} ",
        if selected { "▸" } else { " " },
        runtime.id,
        runtime.rate,
        runtime.status()
    );
    let style = if runtime.error.is_some() {
        Style::new().fg(Color::Red)
    } else if selected {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title).border_style(style)),
        area,
    );
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let log = app.log.borrow();
    let height = area.height.saturating_sub(2) as usize;
    let skip = log.len().saturating_sub(height);
    let body: Vec<Line> = log[skip..].iter().map(|s| Line::raw(s.clone())).collect();
    let title = format!(" log · {} lines ", log.len());
    frame.render_widget(
        Paragraph::new(body).block(Block::bordered().title(title)),
        area,
    );
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let line = if app.input.is_empty() {
        Line::from(Span::styled(
            "e.g.  ch.send(1, 5)",
            Style::new().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::raw(app.input.clone()),
            Span::styled("▏", Style::new().fg(Color::Yellow)),
        ])
    };
    let title = match &app.prompt {
        Some(p) if p.wait.is_some() => " prompt · waiting (c cancels) ",
        Some(_) => " prompt · running ",
        None if app.editing => " prompt · typing ",
        None => " prompt · Tab to type ",
    };
    let border = if app.editing {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(line).block(Block::bordered().title(title).border_style(border)),
        area,
    );
}

/// Tiny Lua tokenizer: enough for keywords, strings, numbers, comments.
fn highlight(text: &str) -> Vec<Span<'static>> {
    const KEYWORDS: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if",
        "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ];
    let keyword = Style::new().fg(Color::Cyan);
    let string = Style::new().fg(Color::Green);
    let number = Style::new().fg(Color::Yellow);
    let comment = Style::new().fg(Color::DarkGray);

    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                spans.push(Span::styled(text[i..].to_string(), comment));
                break;
            }
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        c if c == quote => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let end = i.min(bytes.len());
                spans.push(Span::styled(text[start..end].to_string(), string));
            }
            c if c.is_ascii_digit() => {
                i += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.' || bytes[i] == b'_')
                {
                    i += 1;
                }
                spans.push(Span::styled(text[start..i].to_string(), number));
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &text[start..i];
                let style = if KEYWORDS.contains(&word) {
                    keyword
                } else {
                    Style::new()
                };
                spans.push(Span::styled(word.to_string(), style));
            }
            _ => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii() && !is_token_start(bytes[i]) {
                    i += 1;
                }
                spans.push(Span::raw(text[start..i].to_string()));
            }
        }
    }
    spans
}

fn is_token_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b.is_ascii_digit() || matches!(b, b'-' | b'"' | b'\'' | b'_')
}

enum Action {
    Quit,
    Edit(usize),
}

fn main() -> color_eyre::eyre::Result<()> {
    color_eyre::install()?;
    if !std::io::stdout().is_terminal() {
        eprintln!("slew tui example: needs an interactive terminal");
        return Ok(());
    }

    let mut app = App::new();
    let mut terminal = ratatui::init();
    let result = loop {
        match tui_loop(&mut terminal, &mut app)? {
            Action::Quit => break Ok(()),
            Action::Edit(index) => {
                // The editor needs the real screen back.
                ratatui::restore();
                app.edit_externally(index);
                terminal = ratatui::init();
            }
        }
    };
    ratatui::restore();
    result
}

fn tui_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<Action> {
    let mut last = Instant::now();
    loop {
        let now = Instant::now();
        let dt = now.duration_since(last).min(Duration::from_millis(250));
        last = now;

        terminal.draw(|frame| ui(frame, app))?;

        if event::poll(Duration::from_millis(16))?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            if key.code == KeyCode::Tab {
                app.editing = !app.editing;
            } else if app.editing {
                match key.code {
                    KeyCode::Enter => app.submit(),
                    KeyCode::Esc => app.input.clear(),
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => app.input.push(c),
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
                    KeyCode::Char('1') => app.selected = 0,
                    KeyCode::Char('2') => app.selected = 1,
                    KeyCode::Char('3') => app.selected = 2,
                    KeyCode::Char('e') => return Ok(Action::Edit(app.selected)),
                    KeyCode::Char('c') => app.cancel_prompt(),
                    KeyCode::Char(' ') => app.paused = !app.paused,
                    KeyCode::Char('+' | '=') => app.bump_speed(1.5),
                    KeyCode::Char('-' | '_') => app.bump_speed(1.0 / 1.5),
                    KeyCode::Char('0') => app.speed = 1.0,
                    _ => {}
                }
            }
        }

        if !app.paused {
            app.tick(dt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Must match `local DEFAULT_CAP` in `CHANNEL_PRELUDE`.
    const DEFAULT_CAP: i64 = 8;

    struct Harness {
        lua: Lua<Ctx>,
        log: Rc<RefCell<Vec<String>>>,
    }

    impl Harness {
        fn new() -> Self {
            let mut lua = Lua::<Ctx>::new();
            let log = Rc::new(RefCell::new(Vec::new()));
            install(&mut lua, &log);
            Self { lua, log }
        }

        fn spawn(&mut self, src: &str) -> Execution<Ctx> {
            spawn_program(
                &mut self.lua,
                "=test",
                src,
                Ctx::prompt(Rc::clone(&self.log)),
            )
        }

        /// Steps until the execution finishes or parks on a channel.
        fn settle(
            &mut self,
            exec: &mut Execution<Ctx>,
            wait: &mut Option<NativeWait>,
        ) -> Result<Step, Error> {
            for _ in 0..64 {
                deliver_if_ready(&mut self.lua, exec, wait);
                let step = exec.step(&mut self.lua, PROMPT_FUEL)?;
                if let Step::Waiting(w) = &step {
                    *wait = Some(*w);
                }
                if !matches!(step, Step::Pending) {
                    return Ok(step);
                }
            }
            panic!("execution did not settle");
        }

        fn run(&mut self, src: &str) -> Vec<Value> {
            let mut exec = self.spawn(src);
            let mut wait = None;
            match self.settle(&mut exec, &mut wait) {
                Ok(Step::Done(vals)) => vals,
                Ok(other) => panic!("expected completion, got {other:?}"),
                Err(e) => panic!("{e}"),
            }
        }

        fn text(&self, v: Value) -> String {
            self.lua.display_value(v)
        }
    }

    #[test]
    fn try_recv_on_empty_reports_empty() {
        let mut h = Harness::new();
        let vals = h.run("return ch.try_recv(4)");
        assert_eq!(vals[0], Value::Nil);
        assert_eq!(h.text(vals[1]), "empty");
    }

    #[test]
    fn try_send_stops_at_capacity() {
        let mut h = Harness::new();
        let vals = h.run(&format!(
            "for i = 1, {DEFAULT_CAP} do assert(ch.try_send(9, i)) end return ch.try_send(9, 99)"
        ));
        assert_eq!(vals[0], Value::Nil);
        assert_eq!(h.text(vals[1]), "full");
    }

    #[test]
    fn send_then_recv_round_trips_a_value() {
        let mut h = Harness::new();
        let vals = h.run("ch.send(2, 'hi') return ch.recv(2)");
        assert_eq!(h.text(vals[0]), "hi");
    }

    #[test]
    fn recv_blocks_until_a_message_arrives() {
        let mut h = Harness::new();
        let mut waiter = h.spawn("return ch.recv(5)");
        let mut wait = None;
        assert!(matches!(
            h.settle(&mut waiter, &mut wait),
            Ok(Step::Waiting(_))
        ));

        h.run("ch.send(5, 99)");
        match h.settle(&mut waiter, &mut wait) {
            Ok(Step::Done(vals)) => assert_eq!(vals[0], Value::Int(99)),
            other => panic!("waiter should resume, got {other:?}"),
        }
    }

    #[test]
    fn send_blocks_on_a_full_channel_then_resumes() {
        let mut h = Harness::new();
        h.run(&format!(
            "for i = 1, {DEFAULT_CAP} do assert(ch.try_send(6, i)) end"
        ));
        let mut sender = h.spawn("ch.send(6, 99) return 'sent'");
        let mut wait = None;
        assert!(matches!(
            h.settle(&mut sender, &mut wait),
            Ok(Step::Waiting(_))
        ));

        // Freeing one slot must wake the blocked sender.
        h.run("assert(ch.try_recv(6) == 1)");
        match h.settle(&mut sender, &mut wait) {
            Ok(Step::Done(vals)) => assert_eq!(h.text(vals[0]), "sent"),
            other => panic!("sender should resume, got {other:?}"),
        }
        assert_eq!(h.run("return ch.try_recv(6)")[0], Value::Int(2));
    }

    #[test]
    fn queued_message_survives_gc() {
        let mut h = Harness::new();
        h.run("ch.send(3, { tag = 'keep' })");
        let _ = h.lua.gc();
        let vals = h.run("return ch.recv(3).tag");
        assert_eq!(h.text(vals[0]), "keep");
    }

    #[test]
    fn cancelling_a_blocked_recv_keeps_the_message() {
        let mut h = Harness::new();
        let mut waiter = h.spawn("return ch.recv(8)");
        let mut wait = None;
        assert!(matches!(
            h.settle(&mut waiter, &mut wait),
            Ok(Step::Waiting(_))
        ));
        waiter.abort(&mut h.lua);

        h.run("ch.send(8, 'later')");
        let vals = h.run("return ch.recv(8)");
        assert_eq!(h.text(vals[0]), "later");
    }

    #[test]
    fn channels_are_created_on_first_use() {
        let mut h = Harness::new();
        let vals = h.run("return ch.try_send(12345, 'x'), (channels[12345] ~= nil)");
        assert_eq!(vals[0], Value::Bool(true));
        assert_eq!(vals[1], Value::Bool(true));
    }

    /// The real column programs must compile and consume a message.
    fn drive_column(h: &mut Harness, label: &str, source: &str, send: &str) -> String {
        let ctx = Ctx::program(label.into(), Rc::clone(&h.log));
        let mut program = spawn_program(&mut h.lua, label, source, ctx);
        let mut wait = None;
        assert!(
            matches!(h.settle(&mut program, &mut wait), Ok(Step::Waiting(_))),
            "{label} should park on ch.recv"
        );
        h.run(send);
        assert!(
            matches!(h.settle(&mut program, &mut wait), Ok(Step::Waiting(_))),
            "{label} should log and park again"
        );
        h.log.borrow().join("\n")
    }

    #[test]
    fn factorial_column_consumes_a_message() {
        let mut h = Harness::new();
        let log = drive_column(&mut h, "1", FACTORIAL, "ch.send(1, 5)");
        assert!(log.contains("[1] factorial(5) = 120"), "log was:\n{log}");
    }

    #[test]
    fn once_column_logs_an_arbitrary_message() {
        let mut h = Harness::new();
        let log = drive_column(&mut h, "3", ONCE, "ch.send(3, 'hello')");
        assert!(log.contains("[3] hello"), "log was:\n{log}");
    }

    #[test]
    fn cancelling_a_running_prompt_clears_it() {
        let mut app = App::new();
        app.input = "ch.recv(99)".into();
        app.submit();

        let mut parked = false;
        for _ in 0..16 {
            app.tick(Duration::from_millis(16));
            if app.prompt.as_ref().is_some_and(|p| p.wait.is_some()) {
                parked = true;
                break;
            }
        }
        assert!(parked, "prompt should park on a channel nobody sends to");

        app.cancel_prompt();
        assert!(app.prompt.is_none(), "cancel should clear the prompt");

        // The prompt slot must be reusable straight away.
        app.input = "return 1 + 1".into();
        app.submit();
        app.tick(Duration::from_millis(16));
        assert!(app.prompt.is_none(), "the new prompt should run and finish");
        assert!(
            app.log.borrow().iter().any(|l| l.contains("[>] 2")),
            "log was:\n{}",
            app.log.borrow().join("\n")
        );
    }
}
