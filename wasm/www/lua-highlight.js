// A small Lua 5.4 tokenizer and HTML renderer for the editor overlay in
// index.html. No dependencies: the textarea holds the real text, and this
// module paints a coloured, line-addressable copy behind it.

const KEYWORDS = new Set([
  "and", "break", "do", "else", "elseif", "end", "false", "for", "function",
  "goto", "if", "in", "local", "nil", "not", "or", "repeat", "return",
  "then", "true", "until", "while",
]);

const BUILTINS = new Set([
  "assert", "collectgarbage", "coroutine", "debug", "dofile", "error",
  "getmetatable", "io", "ipairs", "load", "loadstring", "math", "next",
  "os", "pairs", "pcall", "print", "rawequal", "rawget", "rawlen", "rawset",
  "require", "select", "setmetatable", "string", "table", "tonumber",
  "tostring", "type", "utf8", "xpcall", "_G", "_VERSION",
]);

const WHITESPACE = /\s/;
const IDENT_START = /[A-Za-z_]/;
const IDENT_PART = /[A-Za-z0-9_]/;
const NUMBER = /(?:0[xX][0-9a-fA-F]+(?:\.[0-9a-fA-F]*)?(?:[pP][+-]?\d+)?|\d+(?:\.\d*)?(?:[eE][+-]?\d+)?|\.\d+(?:[eE][+-]?\d+)?)/y;

/// If `[` at `index` opens a long bracket (`[[`, `[=[`, ...), returns the
/// end offset just past its closing bracket, else null.
function longBracketEnd(src, index) {
  if (src[index] !== "[") return null;
  let level = 0;
  while (src[index + 1 + level] === "=") level += 1;
  if (src[index + 1 + level] !== "[") return null;
  const close = "]" + "=".repeat(level) + "]";
  const end = src.indexOf(close, index + level + 2);
  return end === -1 ? src.length : end + close.length;
}

/// Splits `src` into tokens of type "ws", "comment", "string", "number",
/// "keyword", "builtin", "ident", or "op". A token may span newlines (long
/// strings and comments); callers split it when rendering lines.
export function tokenize(src) {
  const tokens = [];
  const push = (type, text) => {
    if (text) tokens.push({ type, text });
  };

  let i = 0;
  while (i < src.length) {
    const c = src[i];

    if (WHITESPACE.test(c)) {
      let end = i + 1;
      while (end < src.length && WHITESPACE.test(src[end])) end += 1;
      push("ws", src.slice(i, end));
      i = end;
      continue;
    }

    if (c === "-" && src[i + 1] === "-") {
      const end = longBracketEnd(src, i + 2) ?? src.indexOf("\n", i);
      const stop = end === -1 ? src.length : end;
      push("comment", src.slice(i, stop));
      i = stop;
      continue;
    }

    const longEnd = longBracketEnd(src, i);
    if (longEnd !== null) {
      push("string", src.slice(i, longEnd));
      i = longEnd;
      continue;
    }

    if (c === '"' || c === "'") {
      let end = i + 1;
      while (end < src.length) {
        if (src[end] === "\\") {
          end += 2;
          // `\z` swallows following whitespace, including newlines
          if (src[end - 1] === "z") {
            while (end < src.length && WHITESPACE.test(src[end])) end += 1;
          }
        } else if (src[end] === c || src[end] === "\n") {
          end += 1;
          break;
        } else {
          end += 1;
        }
      }
      push("string", src.slice(i, end));
      i = end;
      continue;
    }

    NUMBER.lastIndex = i;
    const number = NUMBER.exec(src);
    if (number) {
      push("number", number[0]);
      i = NUMBER.lastIndex;
      continue;
    }

    if (IDENT_START.test(c)) {
      let end = i + 1;
      while (end < src.length && IDENT_PART.test(src[end])) end += 1;
      const word = src.slice(i, end);
      push(KEYWORDS.has(word) ? "keyword" : BUILTINS.has(word) ? "builtin" : "ident", word);
      i = end;
      continue;
    }

    push("op", c);
    i += 1;
  }

  return tokens;
}

function escapeHtml(text) {
  return text.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
}

/// Renders `src` as two HTML fragments — `code` for the highlight layer and
/// `gutter` for the line numbers — so the caller can swap them in wholesale.
/// Line `execLine` (1-based) gets the `exec` class in both.
export function highlight(src, execLine = null) {
  const lines = [[]];
  for (const token of tokenize(src)) {
    const parts = token.text.split("\n");
    for (let i = 0; i < parts.length; i++) {
      if (i > 0) lines.push([]);
      if (parts[i] !== "") lines[lines.length - 1].push([token.type, parts[i]]);
    }
  }

  const code = lines
    .map((segments, index) => {
      const body = segments
        .map(([type, text]) => `<span class="tok-${type}">${escapeHtml(text)}</span>`)
        .join("");
      const suffix = index + 1 === execLine ? " exec" : "";
      return `<div class="line${suffix}">${body}</div>`;
    })
    .join("");

  const gutter = lines
    .map((_, index) => {
      const suffix = index + 1 === execLine ? " exec" : "";
      return `<div class="gln${suffix}">${index + 1}</div>`;
    })
    .join("");

  return { code, gutter, lineCount: lines.length };
}
