//! Official Lua 5.4.9 conformance runner.
//!
//! The upstream corpus is not vendored. `scripts/run-lua-tests.sh` downloads
//! the pinned archive into `target/lua-tests`, then runs this test. Without
//! the corpus the test is a no-op, so offline `cargo test` stays green.
//!
//! Each curated file is executed from a fresh `Lua` state. The result is the
//! first failure (line, timeout, or Rust panic), compared against
//! `tests/lua-conformance.baseline`: getting *further* than the baseline is
//! progress and passes; failing earlier is a regression, and a Rust panic is
//! always a regression. Refresh the baseline after intentional progress with
//! `SLEW_BLESS=1 cargo test --test conformance -- --ignored --nocapture`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use slew::{Error, Lua, StdHost, Step};

/// Mirrors the environment `all.lua` sets up for `lua -e"_U=true" all.lua`:
/// the globals the standalone files expect, with the soft/portable switches
/// so long or memory-hungry sections are skipped.
const PREAMBLE: &str = r#"
_G = _ENV
_VERSION = "Lua 5.4"
arg = {}
_U = true
_soft = true
_port = true
_nomsg = true
T = nil
package.path = "?.lua;libs/?.lua"
"#;

struct Case {
    file: &'static str,
    shims: &'static str,
}

/// Curated upstream files, run from a fresh `Lua`. `locals.lua` and
/// `coroutine.lua` are deliberately absent: both assert on `debug.getinfo`
/// level numbering around `pcall`/`coroutine.close`, which slew does not model
/// (see DESIGN.md, "Known deviations"). Their remaining coverage is not worth
/// the synthetic C-frame machinery those assertions would require.
const CASES: &[Case] = &[
    Case {
        file: "vararg.lua",
        shims: "",
    },
    Case {
        file: "closure.lua",
        shims: "",
    },
    Case {
        file: "gc.lua",
        shims: "",
    },
    Case {
        file: "nextvar.lua",
        shims: "",
    },
    Case {
        file: "pm.lua",
        shims: "",
    },
    Case {
        file: "utf8.lua",
        shims: "",
    },
    Case {
        file: "sort.lua",
        shims: "",
    },
    Case {
        file: "strings.lua",
        shims: "",
    },
    Case {
        file: "math.lua",
        shims: "",
    },
    Case {
        file: "verybig.lua",
        shims: "",
    },
    Case {
        file: "constructs.lua",
        shims: "",
    },
    Case {
        file: "goto.lua",
        shims: "",
    },
    Case {
        file: "calls.lua",
        shims: "",
    },
    Case {
        file: "events.lua",
        shims: "",
    },
    Case {
        file: "bitwise.lua",
        shims: "",
    },
    Case {
        file: "tpack.lua",
        shims: "",
    },
    Case {
        file: "literals.lua",
        shims: "",
    },
    Case {
        file: "attrib.lua",
        shims: "",
    },
];

/// Fuel granted per `step`; execution is bounded by wall clock, not fuel.
const STEP_FUEL: u64 = 10_000_000;
const DEFAULT_TIMEOUT_SECS: u64 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    /// First runtime/compile error at this source line.
    Line(u32),
    /// Ran past the wall-clock budget (e.g. a loop waiting on weak tables).
    Timeout,
    /// Rust panic: never acceptable, always reported as a regression.
    Panic,
}

impl Status {
    /// Monotonic "how far did the file get" rank; a drop is a regression.
    fn rank(self) -> u64 {
        match self {
            Status::Panic => 0,
            Status::Line(n) => n as u64,
            Status::Timeout => 1_000_000,
            Status::Ok => u64::MAX,
        }
    }

    fn label(self) -> String {
        match self {
            Status::Ok => "ok".into(),
            Status::Line(n) => format!("line:{n}"),
            Status::Timeout => "timeout".into(),
            Status::Panic => "PANIC".into(),
        }
    }

    fn parse(s: &str) -> Option<Status> {
        match s {
            "ok" => Some(Status::Ok),
            "timeout" => Some(Status::Timeout),
            "panic" => Some(Status::Panic),
            _ => s.strip_prefix("line:")?.parse().ok().map(Status::Line),
        }
    }
}

fn suite_dir() -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("SLEW_LUA_TESTS_DIR").map(PathBuf::from),
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("target/lua-tests/lua-5.4.9-tests")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|dir| dir.join("vararg.lua").is_file())
}

fn timeout() -> Duration {
    let secs = std::env::var("SLEW_LUA_TESTS_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

#[test]
#[ignore = "requires the official suite; run scripts/run-lua-tests.sh"]
fn official_lua_suite() {
    let Some(dir) = suite_dir() else {
        eprintln!(
            "official Lua suite not found; run scripts/run-lua-tests.sh \
             or set SLEW_LUA_TESTS_DIR"
        );
        return;
    };

    let mut results = Vec::new();
    for case in CASES {
        let status = run_case(&dir, case);
        eprintln!(
            "{:>8}  {}{}",
            status.label(),
            case.file,
            if case.shims.is_empty() {
                ""
            } else {
                "  (+shims)"
            }
        );
        results.push((case.file, status));
    }

    let baseline_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lua-conformance.baseline");
    if std::env::var_os("SLEW_BLESS").is_some() {
        let mut lines = vec![
            "# First failure per official Lua 5.4.9 test file.".to_string(),
            "# status: ok | line:N | timeout | panic".to_string(),
        ];
        for (file, status) in &results {
            lines.push(format!("{file}\t{}", status.label()));
        }
        std::fs::write(&baseline_path, lines.join("\n") + "\n").expect("write baseline");
        eprintln!("baseline written to {}", baseline_path.display());
    }

    let baseline = read_baseline(&baseline_path);
    let mut problems = Vec::new();
    for (file, actual) in &results {
        match actual {
            Status::Panic => problems.push(format!("{file}: Rust panic")),
            _ => match baseline.get(*file) {
                Some(expected) if actual.rank() < expected.rank() => problems.push(format!(
                    "{file}: regression, {actual:?} is earlier than baseline {expected:?}",
                    actual = actual.label(),
                    expected = expected.label()
                )),
                Some(_) => {}
                None => problems.push(format!("{file}: missing from baseline")),
            },
        }
    }
    assert!(
        problems.is_empty(),
        "conformance regressions:\n  {}",
        problems.join("\n  ")
    );
}

fn read_baseline(path: &Path) -> std::collections::HashMap<String, Status> {
    let mut map = std::collections::HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((file, status)) = line.split_once('\t')
            && let Some(status) = Status::parse(status.trim())
        {
            map.insert(file.to_string(), status);
        }
    }
    map
}

fn run_case(dir: &Path, case: &Case) -> Status {
    let path = dir.join(case.file);
    let Ok(src) = std::fs::read(&path) else {
        return Status::Line(0);
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_source(dir, &path, &src, case.shims)
    }));
    outcome.unwrap_or(Status::Panic)
}

/// Module files are resolved under the suite root. The `fs` feature supplies
/// the sandboxed adapter; without it, tests still get a reader (test code may
/// touch the filesystem even when the library does not). A std-backed
/// capability host is installed too, so `io`/`os` exist for the cases that
/// exercise them (rooted at the suite dir, so `attrib.lua`'s temporary files
/// stay contained).
fn install_suite_reader(lua: &mut Lua, dir: &Path) {
    #[cfg(feature = "fs")]
    {
        lua.set_fs_file_reader(dir);
    }
    #[cfg(not(feature = "fs"))]
    {
        let root = dir.to_path_buf();
        lua.set_file_reader(move |path| match std::fs::read(root.join(path)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        });
    }
    lua.set_host(StdHost::new(dir));
}

fn run_source(dir: &Path, path: &Path, src: &[u8], shims: &str) -> Status {
    let mut lua = Lua::new();
    install_suite_reader(&mut lua, dir);
    if let Err(e) = drive(&mut lua, "=preamble", PREAMBLE.as_bytes()) {
        return Status::Line(error_line(&e));
    }
    if !shims.is_empty()
        && let Err(e) = drive(&mut lua, "=shims", shims.as_bytes())
    {
        return Status::Line(error_line(&e));
    }
    let name = path.display().to_string();
    let chunk = match lua.load_named(&name, src) {
        Ok(c) => c,
        Err(e) => return Status::Line(error_line(&e)),
    };

    let deadline = Instant::now() + timeout();
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(&mut lua, STEP_FUEL) {
            Ok(Step::Done(_)) => return Status::Ok,
            Ok(Step::Pending) => {
                if Instant::now() >= deadline {
                    return Status::Timeout;
                }
            }
            Ok(Step::Waiting(_)) => return Status::Line(0),
            Err(e) => {
                if std::env::var_os("SLEW_LUA_TESTS_VERBOSE").is_some() {
                    eprintln!("    {e}");
                }
                return Status::Line(error_line(&e));
            }
        }
    }
}

fn drive(lua: &mut Lua, name: &str, src: &[u8]) -> Result<(), Error> {
    let chunk = lua.load_named(name, src)?;
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(lua, STEP_FUEL)? {
            Step::Done(_) => return Ok(()),
            Step::Pending => {}
            Step::Waiting(_) => panic!("unexpected native wait"),
        }
    }
}

fn error_line(e: &Error) -> u32 {
    match e {
        Error::Parse(e) => e.line,
        Error::Compile(e) => e.line,
        // progress is measured in the case file itself, not in a helper or a
        // dynamically loaded chunk the case happened to call
        Error::Runtime(e) if e.root_line != 0 => e.root_line,
        Error::Runtime(e) => e.line,
    }
}
