-- io prelude: `lines` iterators are real Lua closures, so they run through
-- the regular suspendable VM machinery. Installed only when a host exists.

local methods = __slew_file_methods
__slew_file_methods = nil

local unpack = table.unpack

function methods:lines(...)
  local fmts = table.pack(...)
  return function()
    return self:read(unpack(fmts, 1, fmts.n))
  end
end

function io.lines(fname, ...)
  local f, closeit
  if fname == nil then
    f = io.input()
  elseif type(fname) == "string" then
    f = io.open(fname, "r")
    if f == nil then error("cannot open '" .. fname .. "'", 2) end
    closeit = true
  else
    f = fname
  end
  local fmts = table.pack(...)
  local function iter()
    local r = f:read(unpack(fmts, 1, fmts.n))
    if r == nil and closeit then f:close() end
    return r
  end
  return iter, nil, nil, f
end
