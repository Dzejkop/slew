use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use slew::{Execution, Lua, Step, Value};

use crate::app::{App, boot_or_error, deliver_ready};
use crate::config::{CHANNEL_CAP, MINE_TICKS, MOVE_TICKS, ROBOTS};
use crate::natives::install_natives;
use crate::programs::{DEFAULT_NAV, PRELUDE, default_program_for};
use crate::runtime::{boot_robot, run_to_completion, sources_dir};
use crate::ui::ui;
use crate::world::{Cell, Channel, Ctx, Job, JobKind, Msg, Request, World};

fn test_env(id: usize) -> (Lua<Ctx>, Rc<RefCell<World>>) {
    let world = Rc::new(RefCell::new(World::generate()));
    let mut lua = Lua::<Ctx>::new();
    install_natives(&mut lua);
    let ctx = Ctx {
        robot: id,
        world: Rc::clone(&world),
    };
    let prelude = lua.load_named("=prelude", PRELUDE).unwrap();
    let mut pe = lua.execute_with_context(&prelude, ctx);
    run_to_completion(&mut lua, &mut pe).unwrap();
    (lua, world)
}

fn run_src(lua: &mut Lua<Ctx>, world: &Rc<RefCell<World>>, id: usize, src: &str) -> Vec<Value> {
    let chunk = lua.load_named("=t", src.as_bytes()).unwrap();
    let ctx = Ctx {
        robot: id,
        world: Rc::clone(world),
    };
    let mut exec = lua.execute_with_context(&chunk, ctx);
    loop {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(v)) => return v,
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("unexpected wait"),
            Err(e) => panic!("{e}"),
        }
    }
}

/// Steps `exec` to completion, handing ready waits to the host each round.
/// Panics if a wait surfaces on the root thread; tests that expect one drive
/// `deliver_ready` themselves.
fn step_to_done(
    lua: &mut Lua<Ctx>,
    world: &Rc<RefCell<World>>,
    exec: &mut Execution<Ctx>,
) -> Vec<Value> {
    let mut root_wait = None;
    loop {
        deliver_ready(exec, &mut root_wait, lua, world);
        match exec.step(lua, 1_000_000).unwrap() {
            Step::Done(v) => return v,
            Step::Pending => {}
            Step::Waiting(w) => panic!("unexpected wait {w:?}"),
        }
    }
}

#[test]
fn channel_roundtrip_and_gating() {
    let (mut lua, world) = test_env(0);
    run_src(
        &mut lua,
        &world,
        0,
        r"
            ch.send(0, 'hi')
            log(ch.recv(0))
            local v, err = ch.recv(999)
            log(tostring(v) .. '/' .. tostring(err))
            ",
    );
    let log = world.borrow().log.borrow().join("\n");
    assert!(log.contains("hi"), "log was:\n{log}");
    assert!(log.contains("nil/denied"), "log was:\n{log}");
}

#[test]
fn recv_parks_on_an_empty_channel() {
    let (mut lua, world) = test_env(0);
    // With no message queued, `ch.recv` suspends the calling thread instead of
    // reporting an empty channel.
    let chunk = lua.load_named("=t", "ch.recv(0)").unwrap();
    let ctx = Ctx {
        robot: 0,
        world: Rc::clone(&world),
    };
    let mut exec = lua.execute_with_context(&chunk, ctx);
    assert!(matches!(
        exec.step(&mut lua, 1_000_000).unwrap(),
        Step::Waiting(_)
    ));
}

#[test]
fn sending_nil_is_rejected() {
    // `nil` cannot be distinguished from "nothing arrived" by `sched.recv`,
    // so it must be refused rather than silently dropped into the channel.
    let (mut lua, world) = test_env(0);
    run_src(
        &mut lua,
        &world,
        0,
        "local ok, err = pcall(ch.send, 0, nil) \
         log(tostring(ok) .. '/' .. tostring(err))",
    );
    let log = world.borrow().log.borrow().join("\n");
    assert!(log.contains("false/"), "log was:\n{log}");
    assert!(log.contains("message must be"), "log was:\n{log}");
}

#[test]
fn world_tick_completes_a_move() {
    let mut world = World::generate();
    // Place robot 0 at a known empty tile and start a move east.
    world.robots[0].x = 1;
    world.robots[0].y = 1;
    world.robots[0].job = Some(Job {
        kind: JobKind::Move(2, 1),
        remaining: MOVE_TICKS,
    });
    for _ in 0..MOVE_TICKS {
        world.tick();
    }
    assert_eq!((world.robots[0].x, world.robots[0].y), (2, 1));
}

#[test]
fn blocked_coroutine_does_not_stall_others() {
    let (mut lua, world) = test_env(0);
    let chunk = lua
        .load_named(
            "=t",
            r"
                sched.spawn(function() while true do sched.recv(100) end end)
                sched.spawn(function()
                  for i = 1, 5 do
                    log('tick ' .. i)
                    sched.yield()
                  end
                end)
                sched.loop()
                ",
        )
        .unwrap();
    let ctx = Ctx {
        robot: 0,
        world: Rc::clone(&world),
    };
    let mut exec = lua.execute_with_context(&chunk, ctx);
    for _ in 0..200 {
        let _ = exec.step(&mut lua, 2_000);
        if world
            .borrow()
            .log
            .borrow()
            .iter()
            .any(|s| s.contains("tick 5"))
        {
            break;
        }
    }
    let log = world.borrow().log.borrow().join("\n");
    assert!(log.contains("tick 5"), "log was:\n{log}");
}

#[test]
fn coroutine_wait_is_completed_by_the_host() {
    let (mut lua, world) = test_env(0);
    // An in-flight job makes `robot.wait()` park the calling coroutine.
    world.borrow_mut().robots[0].job = Some(Job {
        kind: JobKind::Mine,
        remaining: MINE_TICKS,
    });
    let chunk = lua
        .load_named(
            "=t",
            r"
                sched.spawn(function() robot.wait(); done = true end)
                sched.loop()
                ",
        )
        .unwrap();
    let ctx = Ctx {
        robot: 0,
        world: Rc::clone(&world),
    };
    let mut exec = lua.execute_with_context(&chunk, ctx);

    // The coroutine wait parks without blocking the execution, so it never
    // surfaces as `Step::Waiting`; it is discovered via `pending_waits`.
    assert_eq!(exec.step(&mut lua, 100_000).unwrap(), Step::Pending);
    assert_ne!(exec.pending_waits(&lua).len(), 0);

    // Once the job finishes, the host driver must complete the coroutine wait.
    world.borrow_mut().robots[0].job = None;
    let mut root_wait = None;
    deliver_ready(&mut exec, &mut root_wait, &mut lua, &world);

    step_to_done(&mut lua, &world, &mut exec);
    assert_eq!(lua.get_global("done"), Value::Bool(true));
}

#[test]
fn recv_wait_resumes_when_a_message_arrives() {
    let (mut lua, world) = test_env(0);
    // `sched.recv` blocks on the native channel wait instead of yielding.
    let chunk = lua
        .load_named(
            "=t",
            r"
                sched.spawn(function() got = sched.recv(0) end)
                sched.loop()
                ",
        )
        .unwrap();
    let ctx = Ctx {
        robot: 0,
        world: Rc::clone(&world),
    };
    let mut exec = lua.execute_with_context(&chunk, ctx);

    // Parked on the empty channel: the execution stays runnable and the wait
    // is tracked rather than surfacing as `Step::Waiting`.
    assert_eq!(exec.step(&mut lua, 100_000).unwrap(), Step::Pending);
    assert_ne!(exec.pending_waits(&lua).len(), 0);

    // A message arrives; the host completes the channel wait.
    world.borrow_mut().channels.insert(
        0,
        Channel {
            cap: CHANNEL_CAP,
            items: VecDeque::from([Msg::Int(7)]),
        },
    );
    step_to_done(&mut lua, &world, &mut exec);
    assert_eq!(lua.get_global("got"), Value::Int(7));
}

#[test]
fn root_wait_is_completed_by_the_host() {
    let (mut lua, world) = test_env(0);
    world.borrow_mut().robots[0].job = Some(Job {
        kind: JobKind::Mine,
        remaining: MINE_TICKS,
    });
    let chunk = lua
        .load_named("=t", "local n = robot.wait() done = (n == nil)")
        .unwrap();
    let ctx = Ctx {
        robot: 0,
        world: Rc::clone(&world),
    };
    let mut exec = lua.execute_with_context(&chunk, ctx);

    // A root-thread wait surfaces as `Step::Waiting` and is tracked.
    let token = match exec.step(&mut lua, 1_000_000).unwrap() {
        Step::Waiting(t) => t,
        other => panic!("expected a root wait, got {other:?}"),
    };
    assert_eq!(exec.pending_waits(&lua), vec![token]);

    world.borrow_mut().robots[0].job = None;
    let mut root_wait = Some(token);
    deliver_ready(&mut exec, &mut root_wait, &mut lua, &world);
    assert!(root_wait.is_none(), "the host cleared the root wait");
    step_to_done(&mut lua, &world, &mut exec);
    assert_eq!(lua.get_global("done"), Value::Bool(true));
}

#[test]
fn root_wait_latch_clears_when_the_host_completes_it_directly() {
    let (mut lua, world) = test_env(0);
    world.borrow_mut().robots[0].job = Some(Job {
        kind: JobKind::Mine,
        remaining: MINE_TICKS,
    });
    let chunk = lua
        .load_named("=t", "local n = robot.wait() done = (n == nil)")
        .unwrap();
    let ctx = Ctx {
        robot: 0,
        world: Rc::clone(&world),
    };
    let mut exec = lua.execute_with_context(&chunk, ctx);
    let token = match exec.step(&mut lua, 1_000_000).unwrap() {
        Step::Waiting(t) => t,
        other => panic!("expected a root wait, got {other:?}"),
    };

    // Complete the wait directly, not through `deliver_ready`. The latch must
    // still clear, or the execution would never be stepped again.
    exec.complete_native(&mut lua, token, Ok(Vec::new()))
        .unwrap();
    let mut root_wait = Some(token);
    deliver_ready(&mut exec, &mut root_wait, &mut lua, &world);
    assert!(
        root_wait.is_none(),
        "the latch clears once the wait is no longer outstanding"
    );

    step_to_done(&mut lua, &world, &mut exec);
    assert_eq!(lua.get_global("done"), Value::Bool(true));
}

#[test]
fn send_wait_resumes_when_the_channel_has_room() {
    let (mut lua, world) = test_env(0);
    world.borrow_mut().channels.insert(
        0,
        Channel {
            cap: CHANNEL_CAP,
            items: (0..CHANNEL_CAP).map(|i| Msg::Int(i as i64)).collect(),
        },
    );
    let chunk = lua
        .load_named(
            "=t",
            "sched.spawn(function() sent = sched.send(0, 99) end) sched.loop()",
        )
        .unwrap();
    let ctx = Ctx {
        robot: 0,
        world: Rc::clone(&world),
    };
    let mut exec = lua.execute_with_context(&chunk, ctx);

    // Blocked on the full channel, not blocking the execution.
    assert_eq!(exec.step(&mut lua, 1_000_000).unwrap(), Step::Pending);
    assert_ne!(exec.pending_waits(&lua).len(), 0);

    // Freeing one slot must wake the sender.
    world
        .borrow_mut()
        .channels
        .get_mut(&0)
        .unwrap()
        .items
        .pop_front();
    step_to_done(&mut lua, &world, &mut exec);
    assert_eq!(lua.get_global("sent"), Value::Bool(true));
}

#[test]
fn ungranted_recv_returns_denied_without_parking() {
    let (mut lua, world) = test_env(0);
    // Channel 999 is not granted to robot 0, so `sched.recv` returns at once
    // instead of parking forever.
    let vals = run_src(
        &mut lua,
        &world,
        0,
        "local m, err = sched.recv(999) return m == nil, err",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(lua.display_value(vals[1]), "denied");
}

#[test]
fn adopted_wait_is_completed_by_the_adopting_execution() {
    let (mut lua, world) = test_env(0);
    // Execution A parks a coroutine on channel 0 and finishes.
    let a = lua
        .load_named(
            "=a",
            "co = coroutine.create(function() got = sched.recv(0) end)\ncoroutine.resume(co)",
        )
        .unwrap();
    let mut ea = lua.execute_with_context(
        &a,
        Ctx {
            robot: 0,
            world: Rc::clone(&world),
        },
    );
    assert!(matches!(
        ea.step(&mut lua, 1_000_000).unwrap(),
        Step::Done(_)
    ));

    // Execution B adopts the wait by resuming the parked coroutine.
    let b = lua
        .load_named("=b", "while got == nil do coroutine.resume(co) end")
        .unwrap();
    let mut eb = lua.execute_with_context(
        &b,
        Ctx {
            robot: 0,
            world: Rc::clone(&world),
        },
    );
    assert_eq!(eb.step(&mut lua, 1_000_000).unwrap(), Step::Pending);
    assert_ne!(
        eb.pending_waits(&lua).len(),
        0,
        "the adopting execution owns the wait"
    );

    world.borrow_mut().channels.insert(
        0,
        Channel {
            cap: CHANNEL_CAP,
            items: VecDeque::from([Msg::Int(7)]),
        },
    );
    step_to_done(&mut lua, &world, &mut eb);
    assert_eq!(lua.get_global("got"), Value::Int(7));
}

/// Force the default programs into place so App tests are hermetic.
fn reset_robot_dirs() {
    let root = sources_dir();
    for id in 0..ROBOTS {
        let dir = root.join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("init.lua"), default_program_for(id)).unwrap();
        std::fs::write(dir.join("nav.lua"), DEFAULT_NAV).unwrap();
    }
}

#[test]
fn app_boots_and_runs() {
    reset_robot_dirs();
    let mut app = App::new();
    for _ in 0..30 {
        app.tick(Duration::from_millis(100));
    }
    for (i, r) in app.robots.iter().enumerate() {
        assert!(r.error.is_none(), "robot {i} errored: {:?}", r.error);
        assert!(
            r.lua.is_some() && r.program.is_some(),
            "robot {i} did not boot a program"
        );
        assert!(
            matches!(r.status(), "running" | "waiting"),
            "robot {i} status was {}",
            r.status()
        );
    }
    let log = app.world.borrow().log.borrow().join("\n");
    assert!(log.contains("[R"), "log was:\n{log}");
}

#[test]
fn lua_shutdown_sets_request() {
    let (mut lua, world) = test_env(0);
    run_src(&mut lua, &world, 0, "robot.shutdown()");
    assert_eq!(world.borrow().requests[0], Some(Request::Shutdown));
}

#[test]
fn probe_reports_neighbors() {
    let (mut lua, world) = test_env(0);
    // Robot 0 sits at (1,1); its west is the border wall, east we set to ore.
    {
        let mut w = world.borrow_mut();
        let idx = (w.w + 2) as usize;
        w.cells[idx] = Cell::Ore(2);
    }
    run_src(
        &mut lua,
        &world,
        0,
        "log(robot.probe('east')) log(robot.probe('west'))",
    );
    let log = world.borrow().log.borrow().join("\n");
    assert!(log.contains("ore"), "log was:\n{log}");
    assert!(log.contains("wall"), "log was:\n{log}");
}

#[test]
fn boot_failure_leaves_robot_off_with_error() {
    let dir = std::env::temp_dir().join(format!("slew-boot-fail-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("init.lua"), "this is not lua (").unwrap();
    let world = Rc::new(RefCell::new(World::generate()));
    let robot = boot_or_error(dir.clone(), 0, &world);
    assert!(robot.lua.is_none(), "failed boot must leave the robot off");
    assert!(robot.error.is_some(), "the boot error should be retained");
    assert_eq!(robot.status(), "off");
    let log = world.borrow().log.borrow().join("\n");
    assert!(log.contains("boot error"), "log was:\n{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lifecycle_and_new_robot() {
    reset_robot_dirs();
    let mut app = App::new();
    let before = app.robots.len();
    app.shutdown(0);
    assert!(app.robots[0].lua.is_none());
    assert_eq!(app.robots[0].status(), "off");
    app.boot(0);
    assert!(app.robots[0].lua.is_some());
    app.reboot(1);
    assert!(app.robots[1].lua.is_some());
    app.new_robot();
    assert_eq!(app.robots.len(), before + 1);
    assert_eq!(app.world.borrow().robots.len(), before + 1);
    assert_eq!(app.world.borrow().grants.len(), before + 1);
    app.tick(Duration::from_millis(100));
}

#[test]
fn explorer_builds_a_mental_map() {
    reset_robot_dirs();
    let mut app = App::new();
    // Silence the chatty miners so the capped log keeps the explorer's map.
    app.shutdown(0);
    app.shutdown(2);
    app.shutdown(3);
    app.speed = 32.0;
    for _ in 0..40 {
        app.tick(Duration::from_millis(100));
    }
    let log = app.world.borrow().log.borrow().join("\n");
    assert!(log.contains("mental map"), "log was:\n{log}");
    assert!(log.contains('@'), "map should mark the robot:\n{log}");
}

#[test]
fn ui_renders_without_panic() {
    reset_robot_dirs();
    let app = App::new();
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let world = app.world.borrow();
    terminal.draw(|frame| ui(frame, &app, &world)).unwrap();
}

#[test]
fn boot_isolates_robots() {
    let root = std::env::temp_dir().join(format!("slew-robots-test-{}", std::process::id()));
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("init.lua"), "seed = 1111 sched.run({})").unwrap();
    std::fs::write(b.join("init.lua"), "seed = 2222 sched.run({})").unwrap();

    let world = Rc::new(RefCell::new(World::generate()));
    let (mut la, mut ea) = boot_robot(&a, 0, Rc::clone(&world)).unwrap();
    let (mut lb, mut eb) = boot_robot(&b, 1, Rc::clone(&world)).unwrap();

    // Boot chunks run until `sched.run` (which returns immediately here).
    loop {
        match ea.step(&mut la, 1_000_000) {
            Ok(Step::Done(_)) => break,
            Ok(Step::Pending) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    loop {
        match eb.step(&mut lb, 1_000_000) {
            Ok(Step::Done(_)) => break,
            Ok(Step::Pending) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(la.get_global("seed"), Value::Int(1111));
    assert_eq!(lb.get_global("seed"), Value::Int(2222));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn facing_name_and_aliases_round_trip() {
    use crate::world::Facing;
    // `name()` feeds the Lua-facing `robot.facing()`; the bundled `nav.turn`
    // keys on the lower-case words, so it must not pick up a title-cased alias.
    for (facing, name) in [
        (Facing::North, "north"),
        (Facing::East, "east"),
        (Facing::South, "south"),
        (Facing::West, "west"),
    ] {
        assert_eq!(facing.name(), name, "canonical name must stay lower-case");
        assert_eq!(Facing::from_name(name.as_bytes()), Some(facing));
        // Capitalised initial ("N") and full word ("North") are accepted aliases.
        assert_eq!(
            Facing::from_name(name[..1].to_uppercase().as_bytes()),
            Some(facing)
        );
        let capitalised = {
            let mut c = name.to_string();
            c.replace_range(..1, &name[..1].to_uppercase());
            c
        };
        assert_eq!(Facing::from_name(capitalised.as_bytes()), Some(facing));
    }
    assert_eq!(Facing::from_name(b"N"), Some(Facing::North));
    assert_eq!(Facing::from_name(b"n"), None);
    assert_eq!(Facing::from_name(b"NORTH"), None);
    assert_eq!(Facing::from_name(b"north "), None);
}
