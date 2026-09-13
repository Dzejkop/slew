//! Embedded Lua sources: the scheduler/channel prelude, boot helpers, and the
//! default robot programs.

pub(crate) const PRELUDE: &str = r#"
package.path = "?.lua;?/init.lua"

local raw = {
  log = __log,
  face = __face, forward = __forward, back = __back, left = __left, right = __right,
  mine = __mine, busy = __busy, pos = __pos, facing = __facing,
  carrying = __carrying, drop = __drop, scan = __scan, wait = __wait,
  probe = __probe, shutdown = __shutdown, reboot = __reboot,
  send = __try_send, recv = __try_recv,
}
__log = nil __face = nil __forward = nil __back = nil __left = nil __right = nil
__mine = nil __busy = nil __pos = nil __facing = nil __carrying = nil __drop = nil
__scan = nil __wait = nil __probe = nil __shutdown = nil __reboot = nil
__try_send = nil __try_recv = nil

log = raw.log
robot = {
  face = raw.face, forward = raw.forward, back = raw.back,
  left = raw.left, right = raw.right, mine = raw.mine,
  busy = raw.busy, pos = raw.pos, facing = raw.facing,
  carrying = raw.carrying, drop = raw.drop, scan = raw.scan, wait = raw.wait,
  probe = raw.probe, shutdown = raw.shutdown, reboot = raw.reboot,
}
ch = { try_send = raw.send, try_recv = raw.recv }

sched = {}
local tasks = {}

function sched.spawn(f)
  local co = coroutine.create(f)
  tasks[#tasks + 1] = co
  return co
end

function sched.yield() coroutine.yield() end
function sched.sleep(n) for _ = 1, n do coroutine.yield() end end
function sched.await(pred) while not pred() do coroutine.yield() end end

function sched.recv(chan)
  while true do
    local m = ch.try_recv(chan)
    if m ~= nil then return m end
    coroutine.yield()
  end
end

function sched.send(chan, msg)
  while true do
    local ok = ch.try_send(chan, msg)
    if ok then return true end
    coroutine.yield()
  end
end

function sched.loop()
  while true do
    local next_tasks = {}
    for i = 1, #tasks do
      local co = tasks[i]
      if co ~= nil and coroutine.status(co) ~= 'dead' then
        local ok, err = coroutine.resume(co)
        if not ok then
          log('[sched] error: ' .. tostring(err))
        elseif coroutine.status(co) ~= 'dead' then
          next_tasks[#next_tasks + 1] = co
        end
      end
    end
    if #next_tasks == 0 then return end
    tasks = next_tasks
  end
end

function sched.run(fns)
  for _, f in ipairs(fns) do sched.spawn(f) end
  return sched.loop()
end
"#;

pub(crate) const DEFAULT_INIT: &str = r"-- Robot program. Runs forever via the cooperative scheduler.
-- Add modules next to this file and `require` them.
local nav = require 'nav'

local function miner()
  while true do
    if not robot.mine() then
      if not robot.forward() then
        robot.face(nav.turn(robot.facing()))
      end
    end
    sched.await(function() return not robot.busy() end)
    if robot.carrying() >= 5 then
      local n = robot.drop()
      log('dropped ' .. n .. ' ore')
    end
  end
end

local function reporter()
  while true do
    sched.sleep(20)
    local x, y = robot.pos()
    log('at ' .. x .. ',' .. y .. ' facing ' .. robot.facing())
    sched.send(100, 'R@' .. x .. ',' .. y)
  end
end

local function listener()
  while true do
    local msg = sched.recv(100)
    log('heard ' .. tostring(msg))
  end
end

sched.run({ miner, reporter, listener })
";

pub(crate) const DEFAULT_EXPLORER: &str = r"-- Explorer program: wanders the world building a mental map.
local order = { 'north', 'east', 'south', 'west' }
local delta = { north = { 0, -1 }, east = { 1, 0 }, south = { 0, 1 }, west = { -1, 0 } }
local map = {}
local seen = {}

local function key(x, y) return x .. ',' .. y end

local function sense()
  local x, y = robot.pos()
  seen[key(x, y)] = true
  map[key(x, y)] = 'empty'
  for _, d in ipairs(order) do
    local c = robot.probe(d)
    if c ~= 'edge' then
      local dd = delta[d]
      map[key(x + dd[1], y + dd[2])] = c
    end
  end
end

local function render_lines()
  local minx, maxx, miny, maxy = math.huge, -math.huge, math.huge, -math.huge
  for k in pairs(map) do
    local x, y = k:match('(-?%d+),(-?%d+)')
    x, y = tonumber(x), tonumber(y)
    if x < minx then minx = x end
    if x > maxx then maxx = x end
    if y < miny then miny = y end
    if y > maxy then maxy = y end
  end
  local rx, ry = robot.pos()
  local lines = {}
  for y = miny, maxy do
    local row = {}
    for x = minx, maxx do
      local sym = ' '
      if x == rx and y == ry then
        sym = '@'
      else
        local c = map[key(x, y)]
        if c == 'wall' then sym = '#'
        elseif c == 'ore' then sym = '*'
        elseif c == 'empty' then sym = '.'
        elseif c == 'robot' then sym = 'o' end
      end
      row[#row + 1] = sym
    end
    lines[#lines + 1] = table.concat(row)
  end
  return lines
end

local function choose()
  local x, y = robot.pos()
  for _, d in ipairs(order) do
    local dd = delta[d]
    local k = key(x + dd[1], y + dd[2])
    if map[k] ~= 'wall' and not seen[k] then
      return d
    end
  end
  return order[math.random(4)]
end

local function explorer()
  sense()
  local steps = 0
  while true do
    robot.face(choose())
    robot.forward()
    sched.await(function() return not robot.busy() end)
    sense()
    steps = steps + 1
    if steps % 30 == 0 then
      log('mental map after ' .. steps .. ' steps:')
      for _, row in ipairs(render_lines()) do
        log('  ' .. row)
      end
    end
  end
end

sched.run({ explorer })
";

pub(crate) const DEFAULT_NAV: &str = r"-- Small helper module, loaded via `require 'nav'`.
local M = {}

local turns = { north = 'east', east = 'south', south = 'west', west = 'north' }

function M.turn(facing)
  return turns[facing] or 'north'
end

return M
";

pub(crate) fn default_program_for(id: usize) -> &'static str {
    if id % 4 == 1 {
        DEFAULT_EXPLORER
    } else {
        DEFAULT_INIT
    }
}
