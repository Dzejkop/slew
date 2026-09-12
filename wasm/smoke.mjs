// Headless smoke test for the browser bindings. Run after building:
//
//   wasm-pack build --dev --target web --out-dir www/pkg
//   node smoke.mjs
//
// Uses initSync so it does not need an HTTP server or fetch.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { initSync, Session } from "./www/pkg/slew_wasm.js";
import { highlight, tokenize } from "./www/lua-highlight.js";

const here = dirname(fileURLToPath(import.meta.url));
initSync({ module: readFileSync(join(here, "www/pkg/slew_wasm_bg.wasm")) });

let failures = 0;
function check(name, ok, detail = "") {
  if (ok) {
    console.log(`ok   ${name}`);
  } else {
    failures += 1;
    console.error(`FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

// print capture, completion, and return values
{
  const session = new Session();
  const status = session.run('print("hello", 1 + 2)\nreturn 6 * 7', 1_000_000);
  const output = session.take_output();
  check("simple run completes", status === "done", status);
  check("print is captured", output === "hello\t3\n", JSON.stringify(output));
  check("return values rendered", session.result() === "42", session.result());
  check("output drains", session.take_output() === "");
}

// fuel budget suspends, resume makes progress, abort releases
{
  const session = new Session();
  const status = session.run("n = 0\nwhile true do\n  n = n + 1\nend", 1_000);
  check("runaway run suspends", status === "suspended", status);
  const n1 = Number(session.global("n"));
  check("global visible while suspended", Number.isInteger(n1), session.global("n"));
  check("location reported", /^chunk:\d+$/.test(session.location() ?? ""), session.location());
  check("reported as suspended", session.is_suspended());
  const again = session.resume(1_000);
  const n2 = Number(session.global("n"));
  check("resume stays suspended", again === "suspended", again);
  check("resume makes progress", n2 > n1, `${n1} -> ${n2}`);
  session.abort();
  check("abort clears suspension", !session.is_suspended());
}

// errors surface as JS exceptions, not panics
{
  const session = new Session();
  let parseMessage = "";
  try {
    session.run("x = = 1", 1_000);
  } catch (e) {
    parseMessage = String(e);
  }
  check("parse error throws", parseMessage.length > 0, "no exception");

  let runtimeThrew = false;
  try {
    session.run("error('boom')", 1_000);
  } catch (e) {
    runtimeThrew = String(e).includes("boom");
  }
  check("runtime error throws", runtimeThrew);

  check("session usable after errors", session.run("return 1", 1_000) === "done");
}

// coroutines across a suspension boundary
{
  const session = new Session();
  const status = session.run(
    `local sum = 0
local co = coroutine.create(function()
  for i = 1, 5 do coroutine.yield(i) end
end)
while true do
  local ok, v = coroutine.resume(co)
  if not ok or v == nil then break end
  sum = sum + v
end
print("sum", sum)`,
    500,
  );
  check("coroutine chunk completes", status === "done", status);
  check("coroutine output", session.take_output() === "sum\t15\n", JSON.stringify(session.take_output()));
}

// a restart discards output from the abandoned run
{
  const session = new Session();
  session.run("print('old')\nn = 0\nwhile true do n = n + 1 end", 1_000);
  check("restarted run is suspended", session.is_suspended());
  let threw = false;
  try {
    session.run("x = = 1", 1_000);
  } catch {
    threw = true;
  }
  check("restart discards stale output", threw && session.take_output() === "", JSON.stringify(session.take_output()));
}

// Lua syntax highlighter for the editor overlay
{
  const tokens = new Map(tokenize('local x = "hi" -- note').map((t) => [t.text, t.type]));
  check("keyword tokenised", tokens.get("local") === "keyword");
  check("string tokenised", tokens.get('"hi"') === "string");
  check("comment tokenised", tokens.get("-- note") === "comment");
  check("builtin tokenised", tokenize("print(1)")[0].type === "builtin");
  check("number tokenised", tokenize("x = 0x1p4")[4]?.type === "number");

  const long = highlight("local s = [[a\nb]]");
  check("long string spans lines", (long.code.match(/tok-string/g) ?? []).length === 2, long.code);
  check("gutter has one line per row", long.gutter.split("gln").length - 1 === 2, long.gutter);
  const zescape = highlight('s = "a\\z\nb"').code;
  check("\\z continues a string across lines", (zescape.match(/tok-string/g) ?? []).length === 2, zescape);

  const marked = highlight("a = 1\nb = 2", 2);
  check("exec line marked in gutter", marked.gutter.includes('<div class="gln exec">2</div>'), marked.gutter);
  check("exec line marked in code", (marked.code.match(/<div class="line exec">/g) ?? []).length === 1);
  check("html is escaped", highlight("a < b and c > d").code.includes("&lt;"));
}

if (failures > 0) {
  console.error(`\n${failures} failure(s)`);
  process.exit(1);
}
console.log("\nall smoke tests passed");
