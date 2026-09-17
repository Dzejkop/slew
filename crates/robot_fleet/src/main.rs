//! Robot-fleet TUI game: many isolated `Lua`s, each running one robot program
//! with cooperative coroutines.
//!
//! ```text
//! cargo run -p robot_fleet
//! ```
//!
//! A robot is a directory under `$TMPDIR/slew-robots/<id>/` with an `init.lua`
//! entry point. `init.lua` may `require` sibling modules; reads are confined to
//! the robot's own directory by the host file reader. Each robot gets its own
//! `Lua`, so robots cannot see or mutate each other's globals; they communicate
//! only through host-owned channels.
//!
//! Concurrency inside a robot is cooperative. `robot.wait()`, `ch.wait_nonempty`
//! and `ch.wait_room` are suspendable natives: they park only the calling
//! coroutine until the host sees the world condition hold, and the `sched`
//! prelude wraps them into `sched.recv`/`sched.send` (plus the yield-based
//! `sched.sleep`/`sched.await` helpers). A robot program ends by entering
//! `sched.run{...}`, which drives its coroutines round-robin forever; the host
//! bounds each frame with a fuel budget (`Execution::step`).
//!
//! Controls:
//!   j/k or ↑/↓  select · f follow · n new robot · e edit in $EDITOR
//!   b boot · r reboot · x shutdown
//!   Tab         typing/controls · Enter run (typing) · c cancel prompt
//!   space       pause · +/- speed · 0 reset · q quit
//!
//! Robot programs may also request lifecycle changes: `robot.shutdown()` and
//! `robot.reboot()` set a host request flag that the game loop applies after
//! the current step.

mod app;
mod config;
mod natives;
mod programs;
mod runtime;
mod ui;
mod world;

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};

use app::App;

enum Action {
    Quit,
    Edit(usize),
}

fn main() -> color_eyre::eyre::Result<()> {
    color_eyre::install()?;
    if !std::io::stdout().is_terminal() {
        eprintln!("robot_fleet: needs an interactive terminal");
        return Ok(());
    }

    let mut app = App::new();
    let mut terminal = ratatui::init();
    let result = loop {
        match tui_loop(&mut terminal, &mut app)? {
            Action::Quit => break Ok(()),
            Action::Edit(i) => {
                ratatui::restore();
                app.edit_externally(i);
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

        terminal.draw(|frame| {
            let world = app.world.borrow();
            crate::ui::ui(frame, app, &world);
        })?;

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
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.selected = app.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.selected = (app.selected + 1).min(app.robots.len() - 1);
                    }
                    KeyCode::Char('f') => app.follow = !app.follow,
                    KeyCode::Char('b') => {
                        let i = app.selected;
                        app.boot(i);
                    }
                    KeyCode::Char('r') => {
                        let i = app.selected;
                        app.reboot(i);
                    }
                    KeyCode::Char('x') => {
                        let i = app.selected;
                        app.shutdown(i);
                    }
                    KeyCode::Char('n') => app.new_robot(),
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
mod tests;
