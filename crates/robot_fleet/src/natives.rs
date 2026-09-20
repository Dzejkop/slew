//! Host natives exposed to robot Lua: world actions, channels, lifecycle.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::rc::Rc;

use slew::{Lua, NativeContext, NativeOutcome, Value};

use crate::config::{CHANNEL_CAP, MINE_TICKS, MOVE_TICKS};
use crate::world::{Cell, Channel, Ctx, Facing, Job, JobKind, Msg, Request, WaitKind};

fn int_arg(
    _ctx: &NativeContext<'_, Ctx>,
    args: &[Value],
    idx: usize,
    name: &str,
) -> Result<i64, String> {
    match args.get(idx) {
        Some(Value::Int(n)) => Ok(*n),
        Some(Value::Float(f)) if f.fract() == 0.0 => Ok(*f as i64),
        Some(v) => Err(format!(
            "bad argument #{} to '{name}' (number expected, got {})",
            idx + 1,
            v.type_name()
        )),
        None => Err(format!(
            "bad argument #{} to '{name}' (number expected)",
            idx + 1
        )),
    }
}

fn str_arg(
    ctx: &NativeContext<'_, Ctx>,
    args: &[Value],
    idx: usize,
    name: &str,
) -> Result<Vec<u8>, String> {
    match args.get(idx).and_then(|v| ctx.str_bytes(*v)) {
        Some(b) => Ok(b.to_vec()),
        None => Err(format!(
            "bad argument #{} to '{name}' (string expected)",
            idx + 1
        )),
    }
}

/// `false, reason` — the shape every gated robot action returns when it cannot
/// proceed.
fn refuse(ctx: &mut NativeContext<'_, Ctx>, reason: &[u8]) -> NativeOutcome {
    let s = ctx.new_string(reason);
    NativeOutcome::Return(vec![Value::Bool(false), s])
}

/// Turns the robot using `turn`, returning nothing (like `robot.face`).
fn rotate(
    ctx: &mut NativeContext<'_, Ctx>,
    turn: fn(Facing) -> Facing,
) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let f = turn(world.borrow().robots[rid].facing);
    world.borrow_mut().robots[rid].facing = f;
    Ok(NativeOutcome::Return(Vec::new()))
}

fn value_to_msg(ctx: &NativeContext<'_, Ctx>, v: Value) -> Option<Msg> {
    // `nil` is deliberately not a message: `ch.recv` reads a `nil` result as
    // "woken, so try again", so a nil message could never be delivered.
    match v {
        Value::Bool(b) => Some(Msg::Bool(b)),
        Value::Int(i) => Some(Msg::Int(i)),
        Value::Float(f) => Some(Msg::Float(f)),
        Value::Str(_) => ctx.str_bytes(v).map(|b| Msg::Str(b.to_vec())),
        _ => None,
    }
}

fn msg_to_value(ctx: &mut NativeContext<'_, Ctx>, m: &Msg) -> Value {
    match m {
        Msg::Bool(b) => Value::Bool(*b),
        Msg::Int(i) => Value::Int(*i),
        Msg::Float(f) => Value::Float(*f),
        Msg::Str(s) => ctx.new_string(s),
    }
}

fn native_log(ctx: &mut NativeContext<'_, Ctx>, args: &[Value]) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let text = args
        .iter()
        .map(|v| ctx.display_value(*v))
        .collect::<Vec<_>>()
        .join(" ");
    Rc::clone(&ctx.context().world).borrow().push_log(rid, text);
    Ok(NativeOutcome::Return(Vec::new()))
}

fn native_face(ctx: &mut NativeContext<'_, Ctx>, args: &[Value]) -> Result<NativeOutcome, String> {
    let name = str_arg(ctx, args, 0, "face")?;
    let facing = Facing::from_name(&name)
        .ok_or_else(|| "robot.face: expected 'north'|'east'|'south'|'west'".to_string())?;
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    world.borrow_mut().robots[rid].facing = facing;
    Ok(NativeOutcome::Return(Vec::new()))
}

fn begin_move(ctx: &mut NativeContext<'_, Ctx>, dx: i32, dy: i32) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let mut w = world.borrow_mut();
    if w.robots[rid].job.is_some() {
        return Ok(refuse(ctx, b"busy"));
    }
    let (x, y) = (w.robots[rid].x, w.robots[rid].y);
    let (nx, ny) = (x + dx, y + dy);
    if nx <= 0 || ny <= 0 || nx >= w.w - 1 || ny >= w.h - 1 {
        return Ok(refuse(ctx, b"edge"));
    }
    if w.at(nx, ny) == Some(Cell::Wall) {
        return Ok(refuse(ctx, b"wall"));
    }
    if w.occupied_by_other(rid, nx, ny) {
        return Ok(refuse(ctx, b"occupied"));
    }
    w.robots[rid].job = Some(Job {
        kind: JobKind::Move(nx, ny),
        remaining: MOVE_TICKS,
    });
    Ok(NativeOutcome::Return(vec![Value::Bool(true)]))
}

fn native_forward(
    ctx: &mut NativeContext<'_, Ctx>,
    _args: &[Value],
) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let (dx, dy) = ctx.context().world.borrow().robots[rid].facing.delta();
    begin_move(ctx, dx, dy)
}

fn native_back(ctx: &mut NativeContext<'_, Ctx>, _args: &[Value]) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let (dx, dy) = ctx.context().world.borrow().robots[rid].facing.delta();
    begin_move(ctx, -dx, -dy)
}

fn native_left(ctx: &mut NativeContext<'_, Ctx>, _args: &[Value]) -> Result<NativeOutcome, String> {
    rotate(ctx, Facing::left)
}

fn native_right(
    ctx: &mut NativeContext<'_, Ctx>,
    _args: &[Value],
) -> Result<NativeOutcome, String> {
    rotate(ctx, Facing::right)
}

fn native_mine(ctx: &mut NativeContext<'_, Ctx>, _args: &[Value]) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let mut w = world.borrow_mut();
    if w.robots[rid].job.is_some() {
        return Ok(refuse(ctx, b"busy"));
    }
    let (x, y) = (w.robots[rid].x, w.robots[rid].y);
    match w.cells[(y * w.w + x) as usize] {
        Cell::Ore(n) if n > 0 => {
            w.robots[rid].job = Some(Job {
                kind: JobKind::Mine,
                remaining: MINE_TICKS,
            });
            Ok(NativeOutcome::Return(vec![Value::Bool(true)]))
        }
        _ => Ok(refuse(ctx, b"no ore")),
    }
}

fn native_busy(ctx: &mut NativeContext<'_, Ctx>, _args: &[Value]) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let busy = ctx.context().world.borrow().robots[rid].job.is_some();
    Ok(NativeOutcome::Return(vec![Value::Bool(busy)]))
}

fn native_pos(ctx: &mut NativeContext<'_, Ctx>, _args: &[Value]) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let (x, y) = {
        let w = ctx.context().world.borrow();
        (w.robots[rid].x, w.robots[rid].y)
    };
    Ok(NativeOutcome::Return(vec![
        Value::Int(x as i64),
        Value::Int(y as i64),
    ]))
}

fn native_facing(
    ctx: &mut NativeContext<'_, Ctx>,
    _args: &[Value],
) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let name = ctx.context().world.borrow().robots[rid].facing.name();
    Ok(NativeOutcome::Return(vec![ctx.new_string(name.as_bytes())]))
}

fn native_carrying(
    ctx: &mut NativeContext<'_, Ctx>,
    _args: &[Value],
) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let n = ctx.context().world.borrow().robots[rid].carried;
    Ok(NativeOutcome::Return(vec![Value::Int(n as i64)]))
}

fn native_drop(ctx: &mut NativeContext<'_, Ctx>, _args: &[Value]) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let n = {
        let mut w = world.borrow_mut();
        let n = w.robots[rid].carried;
        w.robots[rid].carried = 0;
        n
    };
    Ok(NativeOutcome::Return(vec![Value::Int(n as i64)]))
}

fn native_scan(ctx: &mut NativeContext<'_, Ctx>, _args: &[Value]) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let (x, y, own) = {
        let w = world.borrow();
        (
            w.robots[rid].x,
            w.robots[rid].y,
            w.cells[(w.robots[rid].y * w.w + w.robots[rid].x) as usize],
        )
    };
    let mut out = String::new();
    let _ = write!(out, "here:{own:?}:");
    for (name, (dx, dy)) in [("N", (0, -1)), ("E", (1, 0)), ("S", (0, 1)), ("W", (-1, 0))] {
        let desc = match world.borrow().at(x + dx, y + dy) {
            None => "edge",
            Some(Cell::Wall) => "wall",
            Some(Cell::Ore(n)) if n > 0 => "ore",
            _ => "empty",
        };
        out.push_str(name);
        out.push(':');
        out.push_str(desc);
        out.push(' ');
    }
    Ok(NativeOutcome::Return(vec![ctx.new_string(out.as_bytes())]))
}

/// Suspends until the robot's current job finishes. Parks the calling
/// coroutine; on the execution's root thread, or where the VM cannot park, it
/// blocks the execution instead (see [`NativeOutcome::Wait`]).
fn native_wait(ctx: &mut NativeContext<'_, Ctx>, _args: &[Value]) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let wait = WaitKind::ActionDone(rid);
    if wait.is_ready(&world.borrow()) {
        return Ok(NativeOutcome::Return(Vec::new()));
    }
    Ok(NativeOutcome::Wait(wait.token()))
}

/// Requests that the host shut this robot down after the current step.
fn native_shutdown(
    ctx: &mut NativeContext<'_, Ctx>,
    _args: &[Value],
) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    ctx.context().world.borrow_mut().requests[rid] = Some(Request::Shutdown);
    Ok(NativeOutcome::Return(Vec::new()))
}

/// Requests that the host reboot this robot after the current step.
fn native_reboot(
    ctx: &mut NativeContext<'_, Ctx>,
    _args: &[Value],
) -> Result<NativeOutcome, String> {
    let rid = ctx.context().robot;
    ctx.context().world.borrow_mut().requests[rid] = Some(Request::Reboot);
    Ok(NativeOutcome::Return(Vec::new()))
}

/// Senses the tile one step in `dir`: `"edge"|"wall"|"ore"|"robot"|"empty"`.
fn native_probe(ctx: &mut NativeContext<'_, Ctx>, args: &[Value]) -> Result<NativeOutcome, String> {
    let name = str_arg(ctx, args, 0, "probe")?;
    let facing = Facing::from_name(&name)
        .ok_or_else(|| "robot.probe: expected 'north'|'east'|'south'|'west'".to_string())?;
    let (dx, dy) = facing.delta();
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let (x, y) = {
        let w = world.borrow();
        (w.robots[rid].x, w.robots[rid].y)
    };
    let (tx, ty) = (x + dx, y + dy);
    let desc = {
        let w = world.borrow();
        match w.at(tx, ty) {
            None => "edge",
            Some(Cell::Wall) => "wall",
            Some(Cell::Ore(n)) if n > 0 => "ore",
            _ if w.occupied_by_other(rid, tx, ty) => "robot",
            _ => "empty",
        }
    };
    Ok(NativeOutcome::Return(vec![ctx.new_string(desc.as_bytes())]))
}

/// Sends `msg` on `chan`. If the channel is full, returns
/// [`NativeOutcome::Wait`]; when the host completes the wait the call resumes
/// with no values, so the `ch.send` prelude wrapper retries. Returns `true` on
/// success, and `nil, "denied"` at once if the robot lacks a grant.
fn native_send(ctx: &mut NativeContext<'_, Ctx>, args: &[Value]) -> Result<NativeOutcome, String> {
    let chan = int_arg(ctx, args, 0, "send")?;
    let Some(value) = args.get(1) else {
        return Err("bad argument #2 to 'send' (value expected)".into());
    };
    let msg = value_to_msg(ctx, *value).ok_or_else(|| {
        "bad argument #2 to 'send' (message must be bool/number/string)".to_string()
    })?;
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let mut w = world.borrow_mut();
    if !w.grants[rid].contains(&chan) {
        let s = ctx.new_string(b"denied");
        return Ok(NativeOutcome::Return(vec![Value::Nil, s]));
    }
    let ch = w.channels.entry(chan).or_insert_with(|| Channel {
        cap: CHANNEL_CAP,
        items: VecDeque::new(),
    });
    if ch.items.len() >= ch.cap {
        return Ok(NativeOutcome::Wait(WaitKind::ChannelRoom(chan).token()));
    }
    ch.items.push_back(msg);
    Ok(NativeOutcome::Return(vec![Value::Bool(true)]))
}

/// Receives a message from `chan`. If the channel is empty, returns
/// [`NativeOutcome::Wait`]; when the host completes the wait the call resumes
/// with no values, so the `ch.recv` prelude wrapper retries. Returns the
/// received message, and `nil, "denied"` at once if the robot lacks a grant.
fn native_recv(ctx: &mut NativeContext<'_, Ctx>, args: &[Value]) -> Result<NativeOutcome, String> {
    let chan = int_arg(ctx, args, 0, "recv")?;
    let rid = ctx.context().robot;
    let world = Rc::clone(&ctx.context().world);
    let (allowed, msg) = {
        let mut w = world.borrow_mut();
        if w.grants[rid].contains(&chan) {
            let ch = w.channels.entry(chan).or_insert_with(|| Channel {
                cap: CHANNEL_CAP,
                items: VecDeque::new(),
            });
            (true, ch.items.pop_front())
        } else {
            (false, None)
        }
    };
    if !allowed {
        let s = ctx.new_string(b"denied");
        return Ok(NativeOutcome::Return(vec![Value::Nil, s]));
    }
    match msg {
        None => Ok(NativeOutcome::Wait(WaitKind::ChannelNonEmpty(chan).token())),
        Some(m) => Ok(NativeOutcome::Return(vec![msg_to_value(ctx, &m)])),
    }
}

pub(crate) fn install_natives(lua: &mut Lua<Ctx>) {
    lua.register_suspendable_native("__log", native_log);
    lua.register_suspendable_native("__face", native_face);
    lua.register_suspendable_native("__forward", native_forward);
    lua.register_suspendable_native("__back", native_back);
    lua.register_suspendable_native("__left", native_left);
    lua.register_suspendable_native("__right", native_right);
    lua.register_suspendable_native("__mine", native_mine);
    lua.register_suspendable_native("__busy", native_busy);
    lua.register_suspendable_native("__pos", native_pos);
    lua.register_suspendable_native("__facing", native_facing);
    lua.register_suspendable_native("__carrying", native_carrying);
    lua.register_suspendable_native("__drop", native_drop);
    lua.register_suspendable_native("__scan", native_scan);
    lua.register_suspendable_native("__wait", native_wait);
    lua.register_suspendable_native("__send", native_send);
    lua.register_suspendable_native("__recv", native_recv);
    lua.register_suspendable_native("__shutdown", native_shutdown);
    lua.register_suspendable_native("__reboot", native_reboot);
    lua.register_suspendable_native("__probe", native_probe);
}
