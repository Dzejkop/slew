-- suslua prelude: stdlib functions that call back into Lua code, written in
-- Lua so they go through the regular (suspendable) VM machinery.

local find, sub, byte = string.find, string.sub, string.byte
local unpack, concat = table.unpack, table.concat

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
