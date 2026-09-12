//! Phase 1: dynamic loading (`load`, `loadfile`, `dofile`), the host
//! file-reader seam, and `package`/`require`.

use slew::{Lua, Step};

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
