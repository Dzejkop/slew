//! Game state and robot lifecycle management.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use ratatui::layout::Rect;
use slew::{Execution, Lua, NativeWait, Step};

use crate::config::{ROBOTS, STEP_FUEL, TICK};
use crate::runtime::{BootError, Prompt, Robot, boot_robot, ensure_default_files, sources_dir};
use crate::world::{Ctx, Request, WaitKind, World};

pub(crate) struct App {
    pub(crate) world: Rc<RefCell<World>>,
    pub(crate) robots: Vec<Robot>,
    pub(crate) selected: usize,
    pub(crate) follow: bool,
    pub(crate) input: String,
    pub(crate) editing: bool,
    pub(crate) paused: bool,
    pub(crate) speed: f64,
    pub(crate) clock: Duration,
}

/// Boots `id`'s program, turning a failure into an off robot that carries the
/// error instead of taking down the whole app. The world log records it too,
/// mirroring how `reboot` surfaces a failed reload.
pub(crate) fn boot_or_error(dir: PathBuf, id: usize, world: &Rc<RefCell<World>>) -> Robot {
    match boot_robot(&dir, id, Rc::clone(world)) {
        Ok((lua, program)) => Robot {
            lua: Some(lua),
            program: Some(program),
            ..Robot::blank(dir)
        },
        Err(e) => {
            world.borrow().push_log(id, format!("boot error: {e}"));
            Robot {
                error: Some(e.to_string()),
                ..Robot::blank(dir)
            }
        }
    }
}

impl App {
    pub(crate) fn new() -> Self {
        let world = Rc::new(RefCell::new(World::generate()));
        let root = sources_dir();
        let mut robots = Vec::new();
        for id in 0..ROBOTS {
            let dir = root.join(id.to_string());
            std::fs::create_dir_all(&dir).expect("create robot dir");
            ensure_default_files(&dir, id);
            robots.push(boot_or_error(dir, id, &world));
        }
        world
            .borrow()
            .push_log(0, format!("robots live in {}", root.display()));
        Self {
            world,
            robots,
            selected: 0,
            follow: true,
            input: String::new(),
            editing: true,
            paused: false,
            speed: 1.0,
            clock: Duration::ZERO,
        }
    }

    pub(crate) fn log(&self, text: impl AsRef<str>) {
        self.world.borrow().log_line(text);
    }

    pub(crate) fn tick(&mut self, dt: Duration) {
        self.clock += dt.mul_f64(self.speed);
        while self.clock >= TICK {
            self.clock -= TICK;
            self.world.borrow_mut().tick();
        }

        self.deliver_all();

        let fuel = ((STEP_FUEL as f64 * self.speed) as u64).max(1_000);
        let n = self.robots.len();
        for i in 0..n {
            self.step_robot(i, fuel);
        }

        self.deliver_all();
        self.process_requests();

        for robot in &mut self.robots {
            let line = match (&robot.program, &robot.lua) {
                (Some(exec), Some(lua)) => exec
                    .current_location(lua)
                    .filter(|(src, _)| src == "init")
                    .map(|(_, line)| line),
                _ => None,
            };
            robot.line = line;
        }
    }

    pub(crate) fn deliver_all(&mut self) {
        let world = Rc::clone(&self.world);
        for i in 0..self.robots.len() {
            let Robot {
                lua,
                program,
                program_wait,
                prompt,
                ..
            } = &mut self.robots[i];
            let Some(lua) = lua.as_mut() else { continue };
            if let Some(exec) = program.as_mut() {
                deliver_ready(exec, program_wait, lua, &world);
            }
            if let Some(p) = prompt.as_mut() {
                deliver_ready(&mut p.exec, &mut p.wait, lua, &world);
            }
        }
    }

    pub(crate) fn step_robot(&mut self, i: usize, fuel: u64) {
        let world = Rc::clone(&self.world);
        let Robot {
            lua,
            program,
            program_wait,
            prompt,
            error,
            ..
        } = &mut self.robots[i];
        let Some(lua) = lua.as_mut() else { return };
        if error.is_some() {
            return;
        }

        let mut finished = false;
        let mut failure = None;
        if let Some(exec) = program.as_mut() {
            match exec.step(lua, fuel) {
                Ok(Step::Done(_)) => finished = true,
                Ok(Step::Pending) => {}
                Ok(Step::Waiting(w)) => *program_wait = Some(w),
                Err(e) => {
                    failure = Some(e.to_string());
                    finished = true;
                }
            }
        }
        if finished {
            *program = None;
            *program_wait = None;
        }
        if let Some(msg) = failure {
            *error = Some(msg.clone());
            world.borrow().push_log(i, format!("program error: {msg}"));
        }

        // Prompt execution: may block on `robot.wait()`.
        let mut done = false;
        let mut perr = None;
        if let Some(p) = prompt.as_mut() {
            if p.wait.is_none() {
                match p.exec.step(lua, fuel) {
                    Ok(Step::Done(_)) => done = true,
                    Ok(Step::Pending) => {}
                    Ok(Step::Waiting(w)) => p.wait = Some(w),
                    Err(e) => {
                        perr = Some(e.to_string());
                        done = true;
                    }
                }
            }
        }
        if done {
            *prompt = None;
        }
        if let Some(msg) = perr {
            world.borrow().push_log(i, format!("prompt error: {msg}"));
        }
    }

    pub(crate) fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        let i = self.selected;
        if self.robots[i].lua.is_none() {
            self.log(format!("[R{i}] robot is off"));
            return;
        }
        if let Some(p) = self.robots[i].prompt.take()
            && let Some(lua) = self.robots[i].lua.as_mut()
        {
            p.exec.abort(lua);
        }
        let world = Rc::clone(&self.world);
        let Robot { lua, prompt, .. } = &mut self.robots[i];
        let Some(lua) = lua.as_mut() else { return };
        match lua.load_named("=prompt", text.as_bytes()) {
            Ok(chunk) => {
                let ctx = Ctx { robot: i, world };
                let exec = lua.execute_with_context(&chunk, ctx);
                *prompt = Some(Prompt { exec, wait: None });
            }
            Err(e) => self.log(format!("[R{i}] {e}")),
        }
    }

    pub(crate) fn cancel_prompt(&mut self) {
        let i = self.selected;
        if let Some(p) = self.robots[i].prompt.take() {
            if let Some(lua) = self.robots[i].lua.as_mut() {
                p.exec.abort(lua);
            }
            self.log(format!("[R{i}] prompt cancelled"));
        } else {
            self.log("[ui] nothing to cancel");
        }
    }

    /// Boots a robot that is currently off.
    pub(crate) fn boot(&mut self, i: usize) {
        if self.robots[i].lua.is_some() {
            self.log(format!("[ui] R{i} already running"));
            return;
        }
        match self.boot_inner(i) {
            Ok(()) => self.log(format!("[ui] booted R{i}")),
            Err(e) => self.log(format!("[!] boot R{i}: {e}")),
        }
    }

    /// Creates the robot's `Lua` and program Execution. Assumes it is off.
    pub(crate) fn boot_inner(&mut self, i: usize) -> Result<(), BootError> {
        let dir = self.robots[i].dir.clone();
        let (lua, program) = boot_robot(&dir, i, Rc::clone(&self.world))?;
        self.robots[i].lua = Some(lua);
        self.robots[i].program = Some(program);
        self.robots[i].program_wait = None;
        self.robots[i].prompt = None;
        self.robots[i].error = None;
        self.robots[i].line = None;
        Ok(())
    }

    /// Tears down a robot's runtime, dropping its `Lua`. World state persists.
    pub(crate) fn shutdown_inner(&mut self, i: usize) {
        if let Some(mut lua) = self.robots[i].lua.take() {
            if let Some(p) = self.robots[i].prompt.take() {
                p.exec.abort(&mut lua);
            }
            if let Some(p) = self.robots[i].program.take() {
                p.abort(&mut lua);
            }
        }
        self.robots[i].program_wait = None;
        self.robots[i].line = None;
    }

    pub(crate) fn shutdown(&mut self, i: usize) {
        let was_on = self.robots[i].lua.is_some();
        self.shutdown_inner(i);
        if was_on {
            self.log(format!("[ui] shut down R{i}"));
        } else {
            self.log(format!("[ui] R{i} already off"));
        }
    }

    pub(crate) fn reboot(&mut self, i: usize) {
        self.shutdown_inner(i);
        match self.boot_inner(i) {
            Ok(()) => self.log(format!("[ui] rebooted R{i}")),
            Err(e) => self.log(format!("[!] reboot R{i}: {e}")),
        }
    }

    /// Creates a brand-new robot (with the default program) and boots it.
    pub(crate) fn new_robot(&mut self) {
        let id = self.world.borrow_mut().add_robot();
        let dir = sources_dir().join(id.to_string());
        let _ = std::fs::create_dir_all(&dir);
        ensure_default_files(&dir, id);
        self.robots.push(Robot::blank(dir));
        self.selected = id;
        match self.boot_inner(id) {
            Ok(()) => self.log(format!("[ui] created R{id}")),
            Err(e) => self.log(format!("[!] create R{id}: {e}")),
        }
    }

    /// Applies lifecycle actions requested by robot programs.
    pub(crate) fn process_requests(&mut self) {
        for i in 0..self.robots.len() {
            let req = self
                .world
                .borrow_mut()
                .requests
                .get_mut(i)
                .and_then(Option::take);
            match req {
                Some(Request::Shutdown) => self.shutdown(i),
                Some(Request::Reboot) => self.reboot(i),
                None => {}
            }
        }
    }

    /// Opens the selected robot's `init.lua` in `$VISUAL`/`$EDITOR`/`vi` and
    /// reboots it. The caller must restore the terminal first.
    pub(crate) fn edit_externally(&mut self, i: usize) {
        let path = self.robots[i].dir.join("init.lua");
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());
        match Command::new(&editor).arg(&path).status() {
            Ok(status) if status.success() => self.reboot(i),
            Ok(status) => self.log(format!("[!] {editor} exited with {status}")),
            Err(e) => self.log(format!("[!] cannot run {editor}: {e}")),
        }
    }

    pub(crate) fn bump_speed(&mut self, factor: f64) {
        self.speed = (self.speed * factor).clamp(1.0 / 16.0, 16.0);
    }

    pub(crate) fn camera(&self, area: Rect, world: &World) -> (i32, i32) {
        let w = area.width as i32;
        let h = area.height as i32;
        let (mut cx, mut cy) = (0, 0);
        if self.follow && self.selected < world.robots.len() {
            let r = &world.robots[self.selected];
            cx = r.x - w / 2;
            cy = r.y - h / 2;
        }
        cx = cx.clamp(0, (world.w - w).max(0));
        cy = cy.clamp(0, (world.h - h).max(0));
        (cx, cy)
    }
}

/// Completes parked native calls once their own condition holds.
///
/// A wait on the program's root thread surfaces as `Step::Waiting` and is
/// tracked in `root_wait`. A wait on a coroutine parks only that coroutine and
/// never surfaces, so every parked token is rediscovered here through
/// [`Execution::pending_waits`]. The token encodes what it is waiting for, so
/// a wait stays completable after another execution adopts its coroutine.
pub(crate) fn deliver_ready(
    exec: &mut Execution<Ctx>,
    root_wait: &mut Option<NativeWait>,
    lua: &mut Lua<Ctx>,
    world: &Rc<RefCell<World>>,
) {
    for token in exec.pending_waits(lua) {
        let Some(kind) = WaitKind::from_token(token) else {
            continue;
        };
        if !kind.is_ready(&world.borrow()) {
            continue;
        }
        // The token came from `pending_waits`, so this cannot fail.
        let _ = exec.complete_native(lua, token, Ok(Vec::new()));
    }
    // A host may complete the root wait through another route (the token is not
    // in `pending_waits` once its completion is set), so drop the latch whenever
    // the token is no longer outstanding — otherwise the execution would never
    // be stepped again.
    if let Some(token) = *root_wait
        && !exec.pending_waits(lua).contains(&token)
    {
        *root_wait = None;
    }
}
