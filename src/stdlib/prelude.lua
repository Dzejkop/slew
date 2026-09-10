-- slew prelude: stdlib functions that call back into Lua code, written in
-- Lua so they go through the regular (suspendable) VM machinery.

local find, sub, byte = string.find, string.sub, string.byte
local unpack, concat = table.unpack, table.concat

-- table.insert lives here rather than as a native so that it can honor
-- __len and __newindex metamethods through the regular VM machinery.
local function to_integer(v)
  if type(v) == 'string' then v = tonumber(v) end
  return math.tointeger(v)
end

local function check_insert_pos(pos)
  local i = to_integer(pos)
  if i == nil then
    if type(pos) == 'number' then
      error("bad argument #2 to 'insert' (number has no integer representation)", 3)
    end
    error("bad argument #2 to 'insert' (number expected, got " .. type(pos) .. ")", 3)
  end
  return i
end

-- luaL_len: `#t` must produce an integer (non-integral results are errors).
local function table_len(t)
  local i = to_integer(#t)
  if i == nil then
    error("object length is not an integer", 3)
  end
  return i
end

function table.insert(t, ...)
  if type(t) ~= 'table' then
    error("bad argument #1 to 'insert' (table expected, got " .. type(t) .. ")", 2)
  end
  local n = select('#', ...)
  if n == 1 then
    local value = ...
    t[table_len(t) + 1] = value
  elseif n == 2 then
    local pos, value = ...
    pos = check_insert_pos(pos)
    local e = table_len(t) + 1
    -- PUC checks `(unsigned)(pos - 1) < (unsigned)e`, i.e. pos in [1, e]
    if not math.ult(pos - 1, e) then
      error("bad argument #2 to 'insert' (position out of bounds)", 2)
    end
    local i = e
    while i > pos do
      t[i] = t[i - 1]
      i = i - 1
    end
    t[pos] = value
  else
    error("wrong number of arguments to 'insert'", 2)
  end
end

function string.gmatch(s, p)
  if type(s) == 'number' then s = tostring(s) end
  local pos = 1
  local len = #s
  return function()
    if pos > len + 1 then return nil end
    local r = {find(s, p, pos)}
    local st, en = r[1], r[2]
    if not st then
      pos = len + 2
      return nil
    end
    if en < st then pos = st + 1 else pos = en + 1 end
    if r[3] ~= nil then return unpack(r, 3) else return sub(s, st, en) end
  end
end

local function expand_repl(repl, whole, caps)
  local out = {}
  local i = 1
  local n = #repl
  while i <= n do
    local c = sub(repl, i, i)
    if c == '%' then
      i = i + 1
      local d = sub(repl, i, i)
      if d == '%' then
        out[#out+1] = '%'
      elseif d == '0' then
        out[#out+1] = whole
      elseif d >= '1' and d <= '9' then
        local v = caps[tonumber(d)]
        if v == nil then error("invalid capture index %" .. d .. " in replacement string") end
        out[#out+1] = tostring(v)
      else
        error("invalid use of '%' in replacement string")
      end
    else
      out[#out+1] = c
    end
    i = i + 1
  end
  return concat(out)
end

function string.gsub(s, pat, repl, maxn)
  if type(s) == 'number' then s = tostring(s) end
  local tr = type(repl)
  if tr == 'number' then
    repl = tostring(repl)
    tr = 'string'
  end
  local anchored = sub(pat, 1, 1) == '^'
  local out, pos, count = {}, 1, 0
  local len = #s
  while pos <= len + 1 do
    if maxn and count >= maxn then break end
    local r = {find(s, pat, pos)}
    local st = r[1]
    if not st then break end
    local en = r[2]
    out[#out+1] = sub(s, pos, st - 1)
    local whole = sub(s, st, en)
    local caps = {}
    for i = 3, #r do caps[i-2] = r[i] end
    if caps[1] == nil then caps[1] = whole end
    local value
    if tr == 'string' then
      value = expand_repl(repl, whole, caps)
    elseif tr == 'table' then
      value = repl[caps[1]]
    elseif tr == 'function' then
      value = repl(unpack(caps))
    else
      error("bad argument #3 to 'gsub' (string/function/table expected)")
    end
    if value == nil or value == false then
      value = whole
    elseif type(value) == 'number' then
      value = tostring(value)
    elseif type(value) ~= 'string' then
      error("invalid replacement value (a " .. type(value) .. ")")
    end
    out[#out+1] = value
    count = count + 1
    if en < st then
      -- empty match: copy one char and advance
      if st <= len then out[#out+1] = sub(s, st, st) end
      pos = st + 1
    else
      pos = en + 1
    end
    if anchored then break end
  end
  out[#out+1] = sub(s, pos)
  return concat(out), count
end

function table.sort(t, cmp)
  cmp = cmp or function(a, b) return a < b end
  local function qs(lo, hi)
    while lo < hi do
      if hi - lo < 12 then
        -- insertion sort for small ranges
        for i = lo + 1, hi do
          local v = t[i]
          local j = i - 1
          while j >= lo and cmp(v, t[j]) do
            t[j+1] = t[j]
            j = j - 1
          end
          t[j+1] = v
        end
        return
      end
      -- median-of-three pivot
      local mid = (lo + hi) // 2
      if cmp(t[mid], t[lo]) then t[lo], t[mid] = t[mid], t[lo] end
      if cmp(t[hi], t[lo]) then t[lo], t[hi] = t[hi], t[lo] end
      if cmp(t[hi], t[mid]) then t[mid], t[hi] = t[hi], t[mid] end
      local p = t[mid]
      local i, j = lo, hi
      while true do
        while cmp(t[i], p) do
          i = i + 1
          if i > hi then error("invalid order function for sorting") end
        end
        while cmp(p, t[j]) do
          j = j - 1
          if j < lo then error("invalid order function for sorting") end
        end
        if i >= j then break end
        t[i], t[j] = t[j], t[i]
        i = i + 1
        j = j - 1
      end
      -- recurse into the smaller half, loop on the bigger one
      if j - lo < hi - j then
        qs(lo, j)
        lo = j + 1
      else
        qs(j + 1, hi)
        hi = j
      end
    end
  end
  qs(1, #t)
end

-- ---- package and require ------------------------------------------------
-- Module bytes come from `loadfile`, which is backed by the host reader (or
-- by nothing at all): the interpreter has no filesystem authority of its
-- own. `package.searchers` is the Lua-level seam, so an embedder can replace
-- or extend it to serve modules from anywhere.

local raw_load = load

function load(chunk, ...)
  if type(chunk) == 'function' then
    -- Reader chunks run under pcall: a failing reader surfaces as
    -- `nil, message` from load, exactly as PUC's protected parser does.
    local parts, n = {}, 0
    local ok, err = pcall(function()
      while true do
        local piece = chunk()
        if piece == nil then break end
        if type(piece) == 'number' then
          piece = tostring(piece)
        elseif type(piece) ~= 'string' then
          error("reader function must return a string", 0)
        end
        if #piece == 0 then break end
        n = n + 1
        parts[n] = piece
      end
    end)
    if not ok then return nil, err end
    local chunkname, mode, env = ...
    if chunkname == nil then chunkname = "=(load)" end
    local text = table.concat(parts)
    local nargs = select('#', ...)
    if nargs >= 3 then return raw_load(text, chunkname, mode, env) end
    if nargs == 2 then return raw_load(text, chunkname, mode) end
    return raw_load(text, chunkname)
  end
  -- forward the argument tail verbatim so an absent `env` stays absent
  return raw_load(chunk, ...)
end

local function preload_searcher(name)
  local v = package.preload[name]
  if v == nil then
    return "no field package.preload['" .. name .. "']"
  end
  return v, ":preload:"
end

local function load_error(name, filename, msg)
  return "error loading module '" .. name .. "' from file '" ..
         filename .. "':\n\t" .. msg
end

local function lua_searcher(name)
  local path = package.path
  if type(path) ~= 'string' then
    error("'package.path' must be a string", 0)
  end
  local filename, err = package.searchpath(name, path, ".", "/")
  if not filename then return err end
  local f, msg = loadfile(filename, "t")
  if not f then
    error(load_error(name, filename, msg), 0)
  end
  return f, filename
end

local function c_searcher(name)
  local path = package.cpath
  if type(path) ~= 'string' then
    error("'package.cpath' must be a string", 0)
  end
  local filename, err = package.searchpath(name, path, ".", "/")
  if not filename then return err end
  error(load_error(name, filename, "dynamic libraries are not supported"), 0)
end

local function croot_searcher(name)
  local p = find(name, ".", 1, true)
  if not p then return nil end
  local path = package.cpath
  if type(path) ~= 'string' then
    error("'package.cpath' must be a string", 0)
  end
  local filename, err = package.searchpath(sub(name, 1, p - 1), path, ".", "/")
  if not filename then return err end
  error(load_error(name, filename, "dynamic libraries are not supported"), 0)
end

package.searchers = {preload_searcher, lua_searcher, c_searcher, croot_searcher}

function require(name)
  local loaded = package.loaded
  local v = loaded[name]
  if v then return v end
  local searchers = package.searchers
  if type(searchers) ~= 'table' then
    error("'package.searchers' must be a table", 0)
  end
  local msgs = {}
  local loader, extra
  local i = 1
  while true do
    local s = searchers[i]
    if s == nil then
      error("module '" .. name .. "' not found:" .. table.concat(msgs), 0)
    end
    local a, b = s(name)
    if type(a) == 'function' then
      loader, extra = a, b
      break
    elseif type(a) == 'string' then
      msgs[#msgs + 1] = "\n\t" .. a
    end
    i = i + 1
  end
  local res = loader(name, extra)
  if res ~= nil then loaded[name] = res end
  if loaded[name] == nil then loaded[name] = true end
  return loaded[name], extra
end

function dofile(filename)
  local f, err = loadfile(filename)
  if f == nil then error(err, 0) end
  return f()
end
