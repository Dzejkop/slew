//! A toy example with 3 Lua programs running at the same time, but each at a
//! different rate. They share a VM: A produces into a queue, B and C consume.
//! Current line highlighted.
//!
//! cargo run --example tui
//! cargo run --example tui -- --frames 300 --headless
//!
//! q/Esc quit, space pause, +/- speed, 0 reset.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use slew::{Error, Execution, Lua, Step, Value};

/// Shared state, installed once. Each program gets a private output table by
/// slot (executions interleave during startup, so creation order is not the
/// column order); `q` is the producer/consumer queue they all see.
const SETUP: &str = r#"
logs = {{}, {}, {}}
q = {}
function logger(tag, index)
  local out = logs[index]
  return function(...)
    local parts = {}
    for i = 1, select('#', ...) do
      parts[#parts + 1] = tostring((select(i, ...)))
    end
    out[#out + 1] = tag .. ": " .. table.concat(parts, " ")
  end
end
"#;

/// Producer: a `coroutine.wrap` generator fills the shared queue.
const PRODUCER: &str = r#"
local print = logger("A", 1)
local gen = coroutine.wrap(function()
  local x = 1
  while true do
    x = (x * 48271) % 2147483647
    coroutine.yield(x)
  end
end)
local n = 0
while true do
  n = n + 1
  q[#q + 1] = gen()
  if n % 10 == 0 then
    print("produced", "n=" .. n, "queue=" .. #q)
  end
end
"#;

/// Slow consumer: reads the queue with its own cursor.
const CONSUMER: &str = r#"
local print = logger("B", 2)
local i, sum = 0, 0
while true do
  local v = q[i + 1]
  if v == nil then
    -- nothing new yet; keep polling
  else
    i = i + 1
    sum = (sum + v) % 1000000007
    print("consumed", "i=" .. i, "sum=" .. sum)
  end
end
"#;

/// Fast auditor: feeds items into a `coroutine.create` sink.
const AUDITOR: &str = r#"
local print = logger("C", 3)
local sink = coroutine.create(function()
  local count, sum = 0, 0
  while true do
    local v = coroutine.yield(count, sum)
    count = count + 1
    sum = (sum + v) % 2147483647
  end
end)
coroutine.resume(sink)
local i = 0
while true do
  local v = q[i + 1]
  if v ~= nil then
    i = i + 1
    local _, count, sum = coroutine.resume(sink, v)
    if i % 25 == 0 then
      print("audited", "i=" .. count, "sum=" .. sum)
    end
  end
end
"#;

struct Column {
    name: &'static str,
    /// 1-based index into the shared `logs` table.
    log_index: i64,
    source: &'static str,
    /// Base fuel per simulated second (before the global speed multiplier).
    rate: f64,
    exec: Execution,
    /// Source line the VM will execute next, when it is in this chunk.
    line: Option<u32>,
    output: Vec<String>,
    /// Highest `logs[i]` index already pulled into `output`.
    read_upto: u64,
    fuel_acc: f64,
    fuel_last: u64,
    total_fuel: u64,
    finished: bool,
    error: Option<String>,
}

impl Column {
    fn new(
        lua: &mut Lua,
        log_index: i64,
        name: &'static str,
        source: &'static str,
        rate: f64,
    ) -> Self {
        let chunk = lua
            .load_named(name, source)
            .unwrap_or_else(|e| panic!("column {name} does not compile: {e}"));
        Self {
            name,
            log_index,
            source,
            rate,
            exec: lua.execute(&chunk),
            line: None,
            output: Vec::new(),
            read_upto: 0,
            fuel_acc: 0.0,
            fuel_last: 0,
            total_fuel: 0,
            finished: false,
            error: None,
        }
    }

    /// Grants this column `rate * speed * dt` fuel (fractional fuel accrues).
    fn advance(&mut self, lua: &mut Lua, dt: Duration, speed: f64) {
        if !self.finished {
            self.fuel_acc += self.rate * speed * dt.as_secs_f64();
            let fuel = self.fuel_acc.floor();
            if fuel >= 1.0 {
                self.fuel_acc -= fuel;
                let fuel = fuel as u64;
                self.fuel_last = fuel;
                self.total_fuel += fuel;
                match self.exec.step(lua, fuel) {
                    Ok(Step::Done(_)) => self.finished = true,
                    Ok(Step::Pending) => {}
                    Err(e) => {
                        self.finished = true;
                        if let Error::Runtime(rt) = &e {
                            self.line = Some(rt.line);
                        }
                        self.error = Some(e.to_string());
                    }
                }
            } else {
                self.fuel_last = 0;
            }
            if !self.finished {
                // Only track frames in *this* chunk: calls into the shared
                // logger would otherwise highlight unrelated program lines.
                if let Some((source, line)) = self.exec.current_location(lua)
                    && source == self.name
                {
                    self.line = Some(line);
                }
            }
        }
        self.pull_output(lua);
    }

    fn pull_output(&mut self, lua: &mut Lua) {
        let logs = lua.get_global("logs");
        if !matches!(logs, Value::Table(_)) {
            return;
        }
        let log = lua.table_get(logs, Value::Int(self.log_index));
        loop {
            let i = self.read_upto + 1;
            let v = lua.table_get(log, Value::Int(i as i64));
            if v == Value::Nil {
                break;
            }
            self.output.push(lua.display_value(v));
            self.read_upto = i;
        }
    }

    fn status(&self) -> String {
        if self.finished {
            "done".into()
        } else {
            match self.line {
                Some(l) => format!("line {l}"),
                None => "…".into(),
            }
        }
    }
}

fn run_to_completion(lua: &mut Lua, src: &str) {
    let chunk = lua.load_named("setup", src).expect("setup compiles");
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(lua, 1_000_000).expect("setup runs") {
            Step::Done(_) => return,
            Step::Pending => continue,
        }
    }
}

struct App {
    lua: Lua,
    columns: [Column; 3],
    /// Global multiplier applied to every column's rate, so the ratio holds.
    speed: f64,
    frames: u64,
    max_frames: Option<u64>,
    paused: bool,
}

impl App {
    fn new() -> Self {
        let mut lua = Lua::new();
        run_to_completion(&mut lua, SETUP);
        let columns = [
            Column::new(&mut lua, 1, "A", PRODUCER, 200.0),
            Column::new(&mut lua, 2, "B", CONSUMER, 20.0),
            Column::new(&mut lua, 3, "C", AUDITOR, 2_000.0),
        ];
        Self {
            lua,
            columns,
            speed: 1.0,
            frames: 0,
            max_frames: None,
            paused: false,
        }
    }

    fn tick(&mut self, dt: Duration) {
        self.frames += 1;
        let speed = self.speed;
        for column in &mut self.columns {
            column.advance(&mut self.lua, dt, speed);
        }
    }

    fn bump_speed(&mut self, factor: f64) {
        self.speed = (self.speed * factor).clamp(1.0 / 16.0, 16.0);
    }
}

fn ui(frame: &mut Frame, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let areas = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(main);
    for (i, area) in areas.iter().enumerate() {
        draw_column(frame, *area, &app.columns[i]);
    }
    let hint = format!(
        "speed ×{:.2}  [+/- · 0 reset]   q quit · space {}",
        app.speed,
        if app.paused { "resume" } else { "pause" }
    );
    frame.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        footer,
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

fn draw_column(frame: &mut Frame, area: Rect, column: &Column) {
    let [source_area, output_area] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);

    let title = format!(
        " {} · {:.0} fuel/s · {} ",
        column.name,
        column.rate,
        column.status()
    );
    let current_line = Style::new().bg(Color::Indexed(236));
    let source: Vec<Line> = column
        .source
        .lines()
        .enumerate()
        .map(|(i, text)| {
            let number = i as u32 + 1;
            let current = column.line == Some(number);
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
    frame.render_widget(
        Paragraph::new(source).block(Block::bordered().title(title)),
        source_area,
    );

    let height = output_area.height.saturating_sub(2) as usize;
    let skip = column.output.len().saturating_sub(height);
    let body: Vec<Line> = column.output[skip..]
        .iter()
        .map(|s| Line::raw(s.clone()))
        .collect();
    let output_title = match &column.error {
        Some(e) => format!(" error: {e} "),
        None => format!(" output · {} lines ", column.output.len()),
    };
    frame.render_widget(
        Paragraph::new(body).block(Block::bordered().title(output_title)),
        output_area,
    );
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let max_frames = args
        .iter()
        .position(|a| a == "--frames")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u64>().ok());
    let headless = args.iter().any(|a| a == "--headless") || !std::io::stdout().is_terminal();
    let speed = args
        .iter()
        .position(|a| a == "--speed")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| *s > 0.0);

    let mut app = App::new();
    app.max_frames = max_frames;
    if let Some(speed) = speed {
        app.speed = speed;
    }

    if headless {
        let frames = max_frames.unwrap_or(200);
        for _ in 0..frames {
            app.tick(Duration::from_millis(33));
            let status: Vec<String> = app
                .columns
                .iter()
                .map(|c| {
                    format!(
                        "{}@{} +{} fuel (total {}, {} out)",
                        c.name,
                        c.line.map_or("-".into(), |l| l.to_string()),
                        c.fuel_last,
                        c.total_fuel,
                        c.output.len(),
                    )
                })
                .collect();
            println!("frame {:>4} | {}", app.frames, status.join(" | "));
        }
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let result = tui_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn tui_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
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
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char(' ') => app.paused = !app.paused,
                KeyCode::Char('+') | KeyCode::Char('=') => app.bump_speed(1.5),
                KeyCode::Char('-') | KeyCode::Char('_') => app.bump_speed(1.0 / 1.5),
                KeyCode::Char('0') => app.speed = 1.0,
                _ => {}
            }
        }

        if !app.paused {
            app.tick(dt);
        }
        if app.max_frames.is_some_and(|max| app.frames >= max) {
            return Ok(());
        }
    }
}
