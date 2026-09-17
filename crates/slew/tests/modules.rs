//! Phase 1: dynamic loading (`load`, `loadfile`, `dofile`), the host
//! file-reader seam, and `package`/`require`.

use slew::{Lua, Step};
use test_case::test_case;

fn eval(lua: &mut Lua, src: &str) -> String {
    let chunk = lua
        .load(src)
        .unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..10_000 {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(vals)) => {
                assert!(!vals.is_empty(), "no result: {src}");
                return lua.display_value(vals[0]);
            }
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("unexpected native wait"),
            Err(e) => panic!("{e}\nsource:\n{src}"),
        }
    }
    panic!("script did not finish: {src}");
}

/// Runs a statement chunk for its side effects.
fn run_ok(lua: &mut Lua, src: &str) {
    let chunk = lua
        .load(src)
        .unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..10_000 {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(_)) => return,
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("unexpected native wait"),
            Err(e) => panic!("{e}\nsource:\n{src}"),
        }
    }
    panic!("script did not finish: {src}");
}

// ---- load ----

#[test]
fn load_string_chunk() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(&mut lua, "local f = load('return 6 * 7') return f()"),
        "42"
    );
}

#[test]
fn load_syntax_error_returns_nil_and_message() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = load('return =') \n\
             return f == nil and type(err) == 'string' and #err > 0"
        ),
        "true"
    );
}

#[test]
fn load_uses_custom_env() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local env = {v = 11, tostring = tostring} \n\
             local f = load('return v + tostring(1)', 'chunk', 't', env) \n\
             return f()"
        ),
        "12"
    );
}

#[test]
fn load_function_reader() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local parts = {'return ', '40 + ', '2'} \n\
             local i = 0 \n\
             local f = load(function() i = i + 1; return parts[i] end, 'reader') \n\
             return f()"
        ),
        "42"
    );
}

#[test]
fn load_function_reader_reports_bad_pieces() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = load(function() return true end) \n\
             return f == nil and type(err) == 'string'"
        ),
        "true"
    );
}

#[test]
fn load_function_reader_accepts_numbers() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local i = 0 \n\
             local f, err = load(function() \n\
               i = i + 1 \n\
               if i == 1 then return 'return ' end \n\
               if i == 2 then return 42 end \n\
             end) \n\
             if not f then error(err) end \n\
             return f()"
        ),
        "42"
    );
}

#[test]
fn load_function_reader_errors_surface_as_message() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = load(function() error('boom') end) \n\
             return f == nil and string.find(err, 'boom', 1, true) ~= nil"
        ),
        "true"
    );
}

#[test]
fn load_explicit_nil_env_is_not_globals() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local f = assert(load('return x', 'chunk', 't', nil)) \n\
             return pcall(f)"
        ),
        "false"
    );
}

#[test]
fn load_mode_rejects_text() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = load('return 1', 'chunk', 'b') \n\
             return f == nil and type(err) == 'string'"
        ),
        "true"
    );
}

// ---- the file-reader seam ----

#[test]
fn loadfile_without_reader_returns_nil_and_message() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = loadfile('mod.lua') \n\
             return f == nil and type(err) == 'string' and #err > 0"
        ),
        "true"
    );
}

#[test]
fn loadfile_uses_reader() {
    let mut lua = Lua::new();
    lua.set_file_reader(|path| match path {
        "mod.lua" => Ok(Some(b"return 'loaded'".to_vec())),
        _ => Ok(None),
    });
    assert_eq!(eval(&mut lua, "return loadfile('mod.lua')()"), "loaded");
}

#[test]
fn loadfile_missing_file_from_reader() {
    let mut lua = Lua::new();
    lua.set_file_reader(|_| Ok(None));
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = loadfile('nope.lua') return f == nil and type(err) == 'string'"
        ),
        "true"
    );
}

#[test]
fn loadfile_reader_error_is_reported() {
    let mut lua = Lua::new();
    lua.set_file_reader(|_| Err("disk on fire".into()));
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = loadfile('x.lua') \n\
             return f == nil and string.find(err, 'disk on fire', 1, true) ~= nil"
        ),
        "true"
    );
}

#[test]
fn dofile_uses_reader() {
    let mut lua = Lua::new();
    lua.set_file_reader(|path| match path {
        "d.lua" => Ok(Some(b"return 7".to_vec())),
        _ => Ok(None),
    });
    assert_eq!(eval(&mut lua, "return dofile('d.lua')"), "7");
}

// ---- loadfile's compiled chunk is callable from the host ----

#[test]
fn make_function_binds_env() {
    let mut lua = Lua::new();
    let env = lua.new_table();
    lua.set_global("env", env);
    run_ok(&mut lua, "env.v = 7");
    let chunk = lua.load("return v").unwrap();
    let f = lua.make_function(&chunk, Some(env));
    lua.set_global("f", f);
    assert_eq!(eval(&mut lua, "return f()"), "7");
}

// ---- package.searchpath ----

#[test]
fn searchpath_reports_tried_files() {
    let mut lua = Lua::new();
    lua.set_file_reader(|_| Ok(None));
    assert_eq!(
        eval(
            &mut lua,
            "local s, err = package.searchpath('a.b', 'x/?.lua;y/?', '.', '/') \n\
             return tostring(s) .. '|' .. err"
        ),
        "nil|no file 'x/a/b.lua'\n\tno file 'y/a/b'"
    );
}

#[test]
fn searchpath_finds_first_match() {
    let mut lua = Lua::new();
    lua.set_file_reader(|path| match path {
        "a/b" | "z/a/b.lua" => Ok(Some(Vec::new())),
        _ => Ok(None),
    });
    assert_eq!(
        eval(
            &mut lua,
            "return package.searchpath('a.b', 'x/?.lua;?;z/?.lua')"
        ),
        "a/b"
    );
}

// ---- require ----

#[test]
fn require_builtin_modules() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "return require('string') == string and require('math') == math \n\
             and require('table') == table and require('coroutine') == coroutine"
        ),
        "true"
    );
}

#[test]
fn require_preload_module() {
    let mut lua = Lua::new();
    run_ok(
        &mut lua,
        "package.preload['greet'] = function(name) return 'module:' .. name end",
    );
    assert_eq!(eval(&mut lua, "return require('greet')"), "module:greet");
}

#[test]
fn require_file_executes_once() {
    let mut lua = Lua::new();
    lua.set_file_reader(move |path| {
        if path == "m.lua" {
            Ok(Some(
                b"_G.loads = (_G.loads or 0) + 1\nreturn _G.loads".to_vec(),
            ))
        } else {
            Ok(None)
        }
    });
    assert_eq!(
        eval(
            &mut lua,
            "local a = require('m') \n\
             local b = require('m') \n\
             return a == b and a"
        ),
        "1"
    );
}

#[test]
fn require_missing_module_message_matches_puc() {
    let mut lua = Lua::new();
    run_ok(
        &mut lua,
        "package.path = '?.lua;?/?' package.cpath = '?.so;?/init'",
    );
    assert_eq!(
        eval(&mut lua, "local ok, err = pcall(require, 'XXX') return err"),
        "module 'XXX' not found:\n\
         \tno field package.preload['XXX']\n\
         \tno file 'XXX.lua'\n\
         \tno file 'XXX/XXX'\n\
         \tno file 'XXX.so'\n\
         \tno file 'XXX/init'"
    );
}

#[test]
fn require_reports_non_string_package_path() {
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            "package.path = {} \n\
             local ok, err = pcall(require, 'x') \n\
             return not ok and string.find(err, 'package.path', 1, true) ~= nil"
        ),
        "true"
    );
}

#[test]
fn require_error_loading_module_names_file() {
    let mut lua = Lua::new();
    lua.set_file_reader(|path| match path {
        "broken.lua" => Ok(Some(b"this is not lua".to_vec())),
        _ => Ok(None),
    });
    assert_eq!(
        eval(
            &mut lua,
            "local ok, err = pcall(require, 'broken') \n\
             return not ok and string.find(err, \"error loading module 'broken'\", 1, true) ~= nil"
        ),
        "true"
    );
}

// ---- the fs adapter (default-on `fs` feature) ----

#[cfg(feature = "fs")]
#[test]
fn fs_reader_is_rooted_and_confined() {
    let root = std::env::temp_dir().join(format!("slew-fs-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("slew-out-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("m.lua"), "return 'fs'").unwrap();
    std::fs::write(outside.join("evil.lua"), "return 'escaped'").unwrap();

    let mut lua = Lua::new();
    lua.set_fs_file_reader(&root);
    assert_eq!(eval(&mut lua, "return require('m')"), "fs");

    let src = format!(
        "local f, err = loadfile([[{}]]) \n\
         return f == nil and string.find(err, 'denied', 1, true) ~= nil",
        outside.join("evil.lua").display()
    );
    assert_eq!(eval(&mut lua, &src), "true");

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
}

// ---- string.dump / binary chunks ----

#[test]
fn string_dump_round_trips_and_loads_binary() {
    let mut lua = Lua::new();
    // A dumped function round-trips through binary `load` with an env.
    assert_eq!(
        eval(
            &mut lua,
            "local f = assert(load(string.dump(function() return 1 end), nil, 'b', {})) \
             return type(f) == 'function' and f() == 1"
        ),
        "true"
    );
    // The header starts with PUC 5.4's signature, version 0x54, format 0.
    assert_eq!(
        eval(
            &mut lua,
            "local c = string.dump(function() return 1 end) \
             return string.sub(c, 1, 4) == '\\27Lua' \
                and string.byte(c, 5) == 0x54 and string.byte(c, 6) == 0"
        ),
        "true"
    );
    // Upvalue names survive the round-trip and be re-bound afterwards.
    assert_eq!(
        eval(
            &mut lua,
            "local a = 7 \
             local f = assert(load(string.dump(function() return a end), '', 'b')) \
             local name = debug.getupvalue(f, 1) \
             debug.setupvalue(f, 1, a) \
             return name .. ':' .. f()"
        ),
        "a:7"
    );
    // Both mode mismatches are rejected with PUC-style messages.
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = load(string.dump(function() end), nil, 't') \
             return f == nil and string.find(err, 'binary chunk') ~= nil"
        ),
        "true"
    );
    assert_eq!(
        eval(
            &mut lua,
            "local f, err = load('return 1', nil, 'b') \
             return f == nil and string.find(err, 'text chunk') ~= nil"
        ),
        "true"
    );
    // Truncation (even mid-signature) reports "truncated".
    assert_eq!(
        eval(
            &mut lua,
            "local c = string.dump(function() return 1 end) \
             local function bad(s) \
               local f, err = load(s) \
               return f == nil and string.find(err, 'truncated') ~= nil \
             end \
             return bad(string.sub(c, 1, 1)) and bad(string.sub(c, 1, #c - 1))"
        ),
        "true"
    );
    // Loading a binary long string interrupted by GC cycles (calls.lua:335).
    assert_eq!(
        eval(
            &mut lua,
            "local function read1(x) local i = 0; \
               return function() collectgarbage(); i = i + 1; return string.sub(x, i, i) end end \
             local c = string.dump(function() return '0123456789' end) \
             return assert(load(read1(c)))()"
        ),
        "0123456789"
    );
    // C functions cannot be dumped.
    assert_eq!(
        eval(&mut lua, "return not pcall(string.dump, print)"),
        "true"
    );
}

#[test]
fn undump_rejects_absurd_element_counts() {
    // A crafted chunk that claims a `u32::MAX`-long `code` array must be
    // rejected as truncated, not abort the host by reserving gigabytes.
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            r"
            local bytes = {
            -- PUC header (signature/version/format/sizes) == \27Lua\84\0\25\147\r\n\26\n\4\8\8
              27, 76, 117, 97, 0x54, 0, 0x19, 0x93, 13, 10, 26, 10, 4, 8, 8,
            -- LUAC_INT sentinel 0x5678 (i64 LE)
              0x78, 0x56, 0, 0, 0, 0, 0, 0,
            -- LUAC_NUM sentinel 370.5 (f64 LE)
              0, 0, 0, 0, 0, 0x28, 0x77, 0x40,
            -- slew payload magic SLW1 + version 1
              83, 76, 87, 49, 1,
            -- proto: empty source, empty name, nparams/is_vararg/max_regs
              0, 0, 0, 0,
              0, 0, 0, 0,
              0, 0, 0,
            -- linedefined, lastlinedefined
              0, 0, 0, 0,
              0, 0, 0, 0,
            -- code length = u32::MAX (the absurd count under test)
              0xff, 0xff, 0xff, 0xff
            }
            local c = string.char(table.unpack(bytes))
            local f, err = load(c)
            return f == nil and string.find(err, 'truncated') ~= nil
            "
        ),
        "true"
    );
}

#[test]
fn crafted_chunks_error_instead_of_panicking() {
    // Chunks that pass validation but drive the VM into states the compiler
    // never emits must raise Lua errors at call time; a chunk with an
    // out-of-range operand is rejected by `load` up front. Neither may abort
    // the host process.
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            r"
            local function chunk(code_bytes, max_regs)
              local head = string.char(27,76,117,97,0x54,0,0x19,0x93,13,10,26,10,4,8,8)
              local sent = string.char(0x78,0x56,0,0,0,0,0,0)
                .. string.char(0,0,0,0,0,0x28,0x77,0x40) .. 'SLW1' .. string.char(1)
              local function u32(n)
                return string.char(n%256, math.floor(n/256)%256,
                                    math.floor(n/65536)%256, math.floor(n/16777216)%256)
              end
              return head..sent
                .. u32(0)..u32(0)..string.char(0,0,max_regs)..u32(0)..u32(0)
                .. u32(1)..code_bytes..u32(1)..u32(0)
                .. u32(0)..u32(0)..u32(0)..u32(0)
                .. u32(1)..string.char(0)..u32(1)..string.char(max_regs)
            end

            -- SetList{obj:1,base:0,n:1,start:1}: 11 = SetList tag, then
            -- obj/base/n as u8 and start as u32.
            local a = assert(load(chunk(string.char(11,1,0,1,1,0,0,0), 2)))
            local ok1, e1 = pcall(a)
            -- ForLoop{base:0,off:-1} with no preceding ForPrep: 26 = ForLoop
            -- tag, base u8, off i32.
            local b = assert(load(chunk(string.char(26,0,255,255,255,255), 4)))
            local ok2, e2 = pcall(b)
            -- Call{base:0,nargs:255,nres:0}: 18 = Call tag, base/nargs/nres.
            local c, e3 = load(chunk(string.char(18,0,255,0), 2))

            return ok1 == false and string.find(e1, 'attempt to index') ~= nil
               and ok2 == false and string.find(e2, 'must be a number') ~= nil
               and c == nil and string.find(e3, 'register operand out of range') ~= nil
            "
        ),
        "true"
    );
}

// `string.dump` output must load back: the validator's register bounds must
// accept everything the compiler emits (table constructors and zero-value
// returns are the tight cases).
#[test_case(
    "local f = function() return {1, 2, 3} end local g = assert(load(string.dump(f))) return #g()"
    => "3"; "table_constructor")]
#[test_case(
    "local f = function() local a, b return a, b end local g = assert(load(string.dump(f))) return tostring(g())"
    => "nil"; "two_value_return")]
// A zero-value `return` after locals puts `base` at the register watermark.
#[test_case(
    "local f = function() local a, b return end local g = assert(load(string.dump(f))) return select('#', g())"
    => "0"; "zero_value_return_with_locals")]
#[test_case(
    "local f = assert(load('local a, b return')) local g = assert(load(string.dump(f))) return select('#', g())"
    => "0"; "zero_value_return_main_chunk")]
#[test_case(
    "local f = function(...) local s = 0 for i = 1, select('#', ...) do s = s + 1 end return s end \
     local g = assert(load(string.dump(f))) return g(1, 2, 3)"
    => "3"; "vararg")]
// A generic `for` with more variables than the iterator call's 3-slot staging
// window exercises the result-register accounting.
#[test_case(
    "local f = function() for a, b, c, d in pairs({x = 1}) do return a, b, c, d end end \
     local g = assert(load(string.dump(f))) return g()"
    => "x"; "generic_for_four_vars")]
fn dumped_protos_round_trip(src: &str) -> String {
    let mut lua = Lua::new();
    eval(&mut lua, src)
}

#[test]
fn crafted_empty_code_chunk_errors_at_call_time() {
    // A proto with no instructions must not make the VM fetch `code[0]`.
    let mut lua = Lua::new();
    assert_eq!(
        eval(
            &mut lua,
            r"
            local bytes = {
              27, 76, 117, 97, 0x54, 0, 0x19, 0x93, 13, 10, 26, 10, 4, 8, 8,
              0x78, 0x56, 0, 0, 0, 0, 0, 0,
              0, 0, 0, 0, 0, 0x28, 0x77, 0x40,
              83, 76, 87, 49, 1,
              0, 0, 0, 0,  -- source
              0, 0, 0, 0,  -- name
              0, 0, 1,     -- nparams / is_vararg / max_regs
              0, 0, 0, 0,  -- linedefined
              0, 0, 0, 0,  -- lastlinedefined
              0, 0, 0, 0,  -- code length 0
              0, 0, 0, 0,  -- lines length 0
              0, 0, 0, 0,  -- consts
              0, 0, 0, 0,  -- upvals
              0, 0, 0, 0,  -- upval names
              0, 0, 0, 0,  -- sub-protos
              0, 0, 0, 0,  -- call names
              0, 0, 0, 0   -- reg extent
            }
            local f = assert(load(string.char(table.unpack(bytes))))
            local ok, err = pcall(f)
            return ok == false and string.find(err, 'fell off the end') ~= nil
            "
        ),
        "true"
    );
}
