//! Per-robot runtime: its `Lua`, program/prompt executions, and booting.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slew::{Execution, Lua, NativeWait, Step};

use crate::natives::install_natives;
use crate::programs::{DEFAULT_NAV, PRELUDE, default_program_for};
use crate::world::{Ctx, World};

pub(crate) struct Prompt {
    pub(crate) exec: Execution<Ctx>,
    pub(crate) wait: Option<NativeWait>,
}

pub(crate) struct Robot {
    pub(crate) dir: PathBuf,
    pub(crate) lua: Option<Lua<Ctx>>,
    pub(crate) program: Option<Execution<Ctx>>,
    pub(crate) program_wait: Option<NativeWait>,
    pub(crate) prompt: Option<Prompt>,
    pub(crate) error: Option<String>,
    pub(crate) line: Option<u32>,
}

impl Robot {
    /// A robot with no runtime attached, used for a failed boot and as the
    /// base for a freshly created robot.
    pub(crate) fn blank(dir: PathBuf) -> Self {
        Robot {
            dir,
            lua: None,
            program: None,
            program_wait: None,
            prompt: None,
            error: None,
            line: None,
        }
    }

    pub(crate) fn status(&self) -> &'static str {
        if self.lua.is_none() {
            "off"
        } else if self.error.is_some() {
            "error"
        } else if self.program.is_none() {
            "halted"
        } else if self.program_wait.is_some() {
            "waiting"
        } else {
            "running"
        }
    }
}

pub(crate) fn sources_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("slew-robots");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub(crate) fn ensure_default_files(dir: &Path, id: usize) {
    let init = dir.join("init.lua");
    if !init.exists() {
        let _ = std::fs::write(&init, default_program_for(id));
    }
    let nav = dir.join("nav.lua");
    if !nav.exists() {
        let _ = std::fs::write(&nav, DEFAULT_NAV);
    }
}

/// A file reader confined to `root` (the robot's directory).
pub(crate) fn make_reader(
    root: &Path,
) -> impl FnMut(&str) -> Result<Option<Vec<u8>>, String> + 'static {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    move |path: &str| {
        let p = Path::new(path);
        let candidate = if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        };
        let Ok(c) = candidate.canonicalize() else {
            return Ok(None);
        };
        if !c.starts_with(&root) {
            return Ok(None);
        }
        match std::fs::read(&c) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(_) => Ok(None),
        }
    }
}

/// Why a robot failed to boot: its prelude or `init.lua` did not load or run.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BootError {
    #[error("{0}")]
    Script(#[source] slew::Error),
    #[error("prelude suspended")]
    Suspended,
    #[error("cannot read init.lua: {0}")]
    Read(#[from] std::io::Error),
    #[error("init.lua: {0}")]
    Init(#[source] slew::Error),
}

pub(crate) fn run_to_completion(
    lua: &mut Lua<Ctx>,
    exec: &mut Execution<Ctx>,
) -> Result<(), BootError> {
    loop {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(_)) => return Ok(()),
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => return Err(BootError::Suspended),
            Err(e) => return Err(BootError::Script(e)),
        }
    }
}

pub(crate) fn boot_robot(
    dir: &Path,
    id: usize,
    world: Rc<RefCell<World>>,
) -> Result<(Lua<Ctx>, Execution<Ctx>), BootError> {
    let mut lua = Lua::<Ctx>::new();
    lua.set_file_reader(make_reader(dir));
    install_natives(&mut lua);

    let ctx = Ctx {
        robot: id,
        world,
        waiting: Vec::new(),
    };
    let prelude = lua
        .load_named("=prelude", PRELUDE)
        .map_err(BootError::Script)?;
    let mut pexec = lua.execute_with_context(&prelude, ctx.clone());
    run_to_completion(&mut lua, &mut pexec)?;

    let src = std::fs::read(dir.join("init.lua"))?;
    let chunk = lua.load_named("init", &src).map_err(BootError::Init)?;
    let program = lua.execute_with_context(&chunk, ctx);
    Ok((lua, program))
}
