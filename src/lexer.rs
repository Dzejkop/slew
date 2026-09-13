//! Lua 5.4 lexer. Operates on bytes: Lua source and strings are byte
//! sequences, not necessarily valid UTF-8.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // literals
    Name(Box<str>),
    Int(i64),
    Float(f64),
    Str(Box<[u8]>),
    // keywords
    And,
    Break,
    Do,
    Else,
    Elseif,
    End,
    False,
    For,
    Function,
    Goto,
    If,
    In,
    Local,
    Nil,
    Not,
    Or,
    Repeat,
    Return,
    Then,
    True,
    Until,
    While,
    // symbols
    Plus,
    Minus,
    Star,
    Slash,
    DoubleSlash,
    Percent,
    Caret,
    Hash,
    Amp,
    Tilde,
    Pipe,
    Shl,
    Shr,
    Eq,
    Ne,
    Le,
    Ge,
    Lt,
    Gt,
    Assign,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    DoubleColon,
    Semi,
    Colon,
    Comma,
    Dot,
    Concat,
    Ellipsis,
    Eof,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Name(n) => write!(f, "{n}"),
            Token::Int(i) => write!(f, "{i}"),
            Token::Float(x) => write!(f, "{x}"),
            Token::Str(_) => write!(f, "string literal"),
            Token::Eof => write!(f, "<eof>"),
            t => {
                let s = match t {
                    Token::And => "and",
                    Token::Break => "break",
                    Token::Do => "do",
                    Token::Else => "else",
                    Token::Elseif => "elseif",
                    Token::End => "end",
                    Token::False => "false",
                    Token::For => "for",
                    Token::Function => "function",
                    Token::Goto => "goto",
                    Token::If => "if",
                    Token::In => "in",
                    Token::Local => "local",
                    Token::Nil => "nil",
                    Token::Not => "not",
                    Token::Or => "or",
                    Token::Repeat => "repeat",
                    Token::Return => "return",
                    Token::Then => "then",
                    Token::True => "true",
                    Token::Until => "until",
                    Token::While => "while",
                    Token::Plus => "+",
                    Token::Minus => "-",
                    Token::Star => "*",
                    Token::Slash => "/",
                    Token::DoubleSlash => "//",
                    Token::Percent => "%",
                    Token::Caret => "^",
                    Token::Hash => "#",
                    Token::Amp => "&",
                    Token::Tilde => "~",
                    Token::Pipe => "|",
                    Token::Shl => "<<",
                    Token::Shr => ">>",
                    Token::Eq => "==",
                    Token::Ne => "~=",
                    Token::Le => "<=",
                    Token::Ge => ">=",
                    Token::Lt => "<",
                    Token::Gt => ">",
                    Token::Assign => "=",
                    Token::LParen => "(",
                    Token::RParen => ")",
                    Token::LBrace => "{",
                    Token::RBrace => "}",
                    Token::LBracket => "[",
                    Token::RBracket => "]",
                    Token::DoubleColon => "::",
                    Token::Semi => ";",
                    Token::Colon => ":",
                    Token::Comma => ",",
                    Token::Dot => ".",
                    Token::Concat => "..",
                    Token::Ellipsis => "...",
                    _ => unreachable!(),
                };
                write!(f, "{s}")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub line: u32,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error at line {}: {}", self.line, self.message)
    }
}

#[derive(Clone)]
pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    pub line: u32,
}

fn keyword(s: &str) -> Option<Token> {
    Some(match s {
        "and" => Token::And,
        "break" => Token::Break,
        "do" => Token::Do,
        "else" => Token::Else,
        "elseif" => Token::Elseif,
        "end" => Token::End,
        "false" => Token::False,
        "for" => Token::For,
        "function" => Token::Function,
        "goto" => Token::Goto,
        "if" => Token::If,
        "in" => Token::In,
        "local" => Token::Local,
        "nil" => Token::Nil,
        "not" => Token::Not,
        "or" => Token::Or,
        "repeat" => Token::Repeat,
        "return" => Token::Return,
        "then" => Token::Then,
        "true" => Token::True,
        "until" => Token::Until,
        "while" => Token::While,
        _ => return None,
    })
}

impl<'a> Lexer<'a> {
    #[must_use]
    pub fn new(src: &'a [u8]) -> Self {
        Lexer {
            src,
            pos: 0,
            line: 1,
        }
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, LexError> {
        Err(LexError {
            message: message.into(),
            line: self.line,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    /// Consumes a newline (\n, \r, \r\n or \n\r), incrementing the line count once.
    fn newline(&mut self) {
        let first = self.bump().unwrap();
        if let Some(b) = self.peek()
            && (b == b'\n' || b == b'\r')
            && b != first
        {
            self.pos += 1;
        }
        self.line += 1;
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | 0x0b | 0x0c) => {
                    self.pos += 1;
                }
                Some(b'\n' | b'\r') => self.newline(),
                Some(b'-') if self.peek2() == Some(b'-') => {
                    self.pos += 2;
                    // long comment?
                    if self.peek() == Some(b'[')
                        && let Some(level) = self.long_bracket_level()
                    {
                        self.read_long_string(level)?;
                        continue;
                    }
                    // line comment
                    while let Some(b) = self.peek() {
                        if b == b'\n' || b == b'\r' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// If positioned at `[`, checks for a long-bracket opener `[=*[`.
    /// On match, consumes it and returns the level; otherwise consumes nothing.
    fn long_bracket_level(&mut self) -> Option<usize> {
        debug_assert_eq!(self.peek(), Some(b'['));
        let mut i = self.pos + 1;
        let mut level = 0;
        while self.src.get(i) == Some(&b'=') {
            level += 1;
            i += 1;
        }
        if self.src.get(i) == Some(&b'[') {
            self.pos = i + 1;
            Some(level)
        } else {
            None
        }
    }

    /// Reads a long string body up to and including the closing `]=*]`.
    /// The opening bracket must already be consumed.
    fn read_long_string(&mut self, level: usize) -> Result<Box<[u8]>, LexError> {
        // first newline is skipped
        if let Some(b'\n' | b'\r') = self.peek() {
            self.newline();
        }
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return self.err_eof("unfinished long string/comment"),
                Some(b']') => {
                    let mut i = self.pos + 1;
                    let mut l = 0;
                    while self.src.get(i) == Some(&b'=') {
                        l += 1;
                        i += 1;
                    }
                    if l == level && self.src.get(i) == Some(&b']') {
                        self.pos = i + 1;
                        return Ok(out.into_boxed_slice());
                    }
                    out.push(b']');
                    self.pos += 1;
                }
                Some(b'\n' | b'\r') => {
                    self.newline();
                    out.push(b'\n');
                }
                Some(b) => {
                    out.push(b);
                    self.pos += 1;
                }
            }
        }
    }

    /// Raw source text of the token starting at `start` up to `end`, used for
    /// PUC Lua's `near '<token text>'` error suffix.
    fn near_text(&self, start: usize, end: usize) -> String {
        let end = end.min(self.src.len());
        String::from_utf8_lossy(&self.src[start..end]).into_owned()
    }

    /// Errors with a PUC-style `near '<token text>'` suffix.
    fn err_near<T>(
        &self,
        start: usize,
        end: usize,
        message: impl Into<String>,
    ) -> Result<T, LexError> {
        Err(LexError {
            message: format!("{} near '{}'", message.into(), self.near_text(start, end)),
            line: self.line,
        })
    }

    /// Errors at end of input, where PUC reports the token as `<eof>`.
    fn err_eof<T>(&self, message: impl Into<String>) -> Result<T, LexError> {
        self.err(format!("{} near <eof>", message.into()))
    }

    fn read_short_string(&mut self, quote: u8, start: usize) -> Result<Box<[u8]>, LexError> {
        let mut out = Vec::new();
        loop {
            let Some(b) = self.bump() else {
                return self.err_eof("unfinished string");
            };
            match b {
                b'\n' | b'\r' => return self.err_near(start, self.pos - 1, "unfinished string"),
                b'\\' => {
                    let Some(e) = self.bump() else {
                        return self.err_eof("unfinished string");
                    };
                    match e {
                        b'a' => out.push(7),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'v' => out.push(11),
                        b'\\' => out.push(b'\\'),
                        b'"' => out.push(b'"'),
                        b'\'' => out.push(b'\''),
                        b'\n' | b'\r' => {
                            self.pos -= 1;
                            self.newline();
                            out.push(b'\n');
                        }
                        b'x' => {
                            let mut v: u32 = 0;
                            for _ in 0..2 {
                                match self.bump().and_then(|c| (c as char).to_digit(16)) {
                                    Some(d) => v = v * 16 + d,
                                    None => {
                                        return self.err_near(
                                            start,
                                            self.pos,
                                            "hexadecimal digit expected",
                                        );
                                    }
                                }
                            }
                            out.push(v as u8);
                        }
                        b'0'..=b'9' => {
                            let mut v: u32 = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(c @ b'0'..=b'9') => {
                                        v = v * 10 + (c - b'0') as u32;
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            if v > 255 {
                                // PUC folds the character that ended the digit run
                                // into the `near` text.
                                if self.peek().is_some() {
                                    self.pos += 1;
                                }
                                return self.err_near(start, self.pos, "decimal escape too large");
                            }
                            out.push(v as u8);
                        }
                        b'z' => loop {
                            match self.peek() {
                                Some(b' ' | b'\t' | 0x0b | 0x0c) => self.pos += 1,
                                Some(b'\n' | b'\r') => self.newline(),
                                _ => break,
                            }
                        },
                        b'u' => {
                            if self.bump() != Some(b'{') {
                                return self.err_near(start, self.pos, "missing '{'");
                            }
                            let mut v: u64 = 0;
                            let mut any = false;
                            while let Some(d) = self.peek().and_then(|c| (c as char).to_digit(16)) {
                                v = v * 16 + d as u64;
                                self.pos += 1;
                                if v > 0x7FFF_FFFF {
                                    return self.err_near(start, self.pos, "UTF-8 value too large");
                                }
                                any = true;
                            }
                            if !any {
                                // Include the offending character in `near`.
                                if self.peek().is_some() {
                                    self.pos += 1;
                                }
                                return self.err_near(
                                    start,
                                    self.pos,
                                    "hexadecimal digit expected",
                                );
                            }
                            if self.bump() != Some(b'}') {
                                return self.err_near(start, self.pos, "missing '}'");
                            }
                            push_utf8(&mut out, v as u32);
                        }
                        _ => return self.err_near(start, self.pos, "invalid escape sequence"),
                    }
                }
                _ if b == quote => return Ok(out.into_boxed_slice()),
                _ => out.push(b),
            }
        }
    }

    fn read_number(&mut self) -> Result<Token, LexError> {
        let start = self.pos;
        let hex = self.peek() == Some(b'0')
            && matches!(self.peek2(), Some(b'x' | b'X'))
            && self
                .src
                .get(self.pos + 2)
                .is_some_and(|c| c.is_ascii_hexdigit() || *c == b'.');
        if hex {
            self.pos += 2;
            let mut is_float = false;
            while let Some(b) = self.peek() {
                match b {
                    _ if b.is_ascii_hexdigit() => self.pos += 1,
                    b'.' => {
                        is_float = true;
                        self.pos += 1;
                    }
                    b'p' | b'P' => {
                        is_float = true;
                        self.pos += 1;
                        if matches!(self.peek(), Some(b'+' | b'-')) {
                            self.pos += 1;
                        }
                    }
                    _ => break,
                }
            }
            let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
            if is_float {
                match parse_hex_float(&text[2..]) {
                    Some(f) => Ok(Token::Float(f)),
                    None => self.err("malformed number"),
                }
            } else {
                // hex integer constants wrap around (Lua 5.4)
                let mut v: u64 = 0;
                for c in text[2..].bytes() {
                    v = v
                        .wrapping_mul(16)
                        .wrapping_add((c as char).to_digit(16).unwrap() as u64);
                }
                Ok(Token::Int(v as i64))
            }
        } else {
            let mut is_float = false;
            while let Some(b) = self.peek() {
                match b {
                    b'0'..=b'9' => self.pos += 1,
                    b'.' => {
                        is_float = true;
                        self.pos += 1;
                    }
                    b'e' | b'E' => {
                        is_float = true;
                        self.pos += 1;
                        if matches!(self.peek(), Some(b'+' | b'-')) {
                            self.pos += 1;
                        }
                    }
                    _ => break,
                }
            }
            if self
                .peek()
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return self.err("malformed number");
            }
            let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
            if is_float {
                text.parse::<f64>()
                    .map(Token::Float)
                    .or_else(|_| self.err("malformed number"))
            } else {
                // decimal integer constants that overflow become floats (Lua 5.4)
                match text.parse::<i64>() {
                    Ok(i) => Ok(Token::Int(i)),
                    Err(_) => text
                        .parse::<f64>()
                        .map(Token::Float)
                        .or_else(|_| self.err("malformed number")),
                }
            }
        }
    }

    /// Lexes the next token, returning it with the line it started on.
    ///
    /// # Errors
    ///
    /// Returns a [`LexError`] on malformed input, e.g. an unterminated string
    /// or long bracket, or a malformed number or escape sequence.
    ///
    /// # Panics
    ///
    /// Panics if an identifier byte range is not valid UTF-8; the range is
    /// built only from ASCII alphanumerics and `_`, so it always is.
    pub fn next_token(&mut self) -> Result<(Token, u32), LexError> {
        self.skip_whitespace_and_comments()?;
        let line = self.line;
        let Some(b) = self.peek() else {
            return Ok((Token::Eof, line));
        };
        let tok = match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let start = self.pos;
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
                {
                    self.pos += 1;
                }
                let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
                keyword(s).unwrap_or_else(|| Token::Name(s.into()))
            }
            b'0'..=b'9' => self.read_number()?,
            b'.' if self.peek2().is_some_and(|c| c.is_ascii_digit()) => self.read_number()?,
            b'"' | b'\'' => {
                let start = self.pos;
                self.pos += 1;
                Token::Str(self.read_short_string(b, start)?)
            }
            b'[' => {
                if let Some(level) = self.long_bracket_level() {
                    Token::Str(self.read_long_string(level)?)
                } else {
                    self.pos += 1;
                    Token::LBracket
                }
            }
            _ => {
                self.pos += 1;
                match b {
                    b'+' => Token::Plus,
                    b'-' => Token::Minus,
                    b'*' => Token::Star,
                    b'/' => {
                        if self.peek() == Some(b'/') {
                            self.pos += 1;
                            Token::DoubleSlash
                        } else {
                            Token::Slash
                        }
                    }
                    b'%' => Token::Percent,
                    b'^' => Token::Caret,
                    b'#' => Token::Hash,
                    b'&' => Token::Amp,
                    b'~' => {
                        if self.peek() == Some(b'=') {
                            self.pos += 1;
                            Token::Ne
                        } else {
                            Token::Tilde
                        }
                    }
                    b'|' => Token::Pipe,
                    b'<' => match self.peek() {
                        Some(b'<') => {
                            self.pos += 1;
                            Token::Shl
                        }
                        Some(b'=') => {
                            self.pos += 1;
                            Token::Le
                        }
                        _ => Token::Lt,
                    },
                    b'>' => match self.peek() {
                        Some(b'>') => {
                            self.pos += 1;
                            Token::Shr
                        }
                        Some(b'=') => {
                            self.pos += 1;
                            Token::Ge
                        }
                        _ => Token::Gt,
                    },
                    b'=' => {
                        if self.peek() == Some(b'=') {
                            self.pos += 1;
                            Token::Eq
                        } else {
                            Token::Assign
                        }
                    }
                    b'(' => Token::LParen,
                    b')' => Token::RParen,
                    b'{' => Token::LBrace,
                    b'}' => Token::RBrace,
                    b']' => Token::RBracket,
                    b';' => Token::Semi,
                    b':' => {
                        if self.peek() == Some(b':') {
                            self.pos += 1;
                            Token::DoubleColon
                        } else {
                            Token::Colon
                        }
                    }
                    b',' => Token::Comma,
                    b'.' => {
                        if self.peek() == Some(b'.') {
                            self.pos += 1;
                            if self.peek() == Some(b'.') {
                                self.pos += 1;
                                Token::Ellipsis
                            } else {
                                Token::Concat
                            }
                        } else {
                            Token::Dot
                        }
                    }
                    _ => return self.err(format!("unexpected symbol near '{}'", b as char)),
                }
            }
        };
        Ok((tok, line))
    }
}

/// Lua's extended UTF-8: encodes values up to 0x7FFFFFFF, up to 6 bytes.
fn push_utf8(out: &mut Vec<u8>, v: u32) {
    if v < 0x80 {
        out.push(v as u8);
        return;
    }
    let mut buf = [0u8; 6];
    let mut n = 0; // continuation bytes written (reversed)
    let mut mfs: u32 = 0x3F; // max value that fits in the first byte for n continuations
    let mut v = v;
    loop {
        buf[n] = 0x80 | (v & 0x3F) as u8;
        n += 1;
        v >>= 6;
        mfs >>= 1;
        if v <= mfs {
            break;
        }
    }
    // first byte: n+1 leading ones
    let first = (!mfs << 1) as u8 | v as u8;
    out.push(first);
    for i in (0..n).rev() {
        out.push(buf[i]);
    }
}

/// Parses a hex float (without the `0x` prefix): hex digits, optional `.`,
/// optional binary exponent `p±d`.
fn parse_hex_float(s: &str) -> Option<f64> {
    let mut mantissa: f64 = 0.0;
    let mut exp: i32 = 0;
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut chars = s.bytes().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            b'.' => {
                if seen_dot {
                    return None;
                }
                seen_dot = true;
                chars.next();
            }
            _ if c.is_ascii_hexdigit() => {
                mantissa = mantissa * 16.0 + (c as char).to_digit(16).unwrap() as f64;
                if seen_dot {
                    exp -= 4;
                }
                seen_digit = true;
                chars.next();
            }
            _ => break,
        }
    }
    if !seen_digit {
        return None;
    }
    match chars.next() {
        None => {}
        Some(b'p' | b'P') => {
            let mut sign = 1i32;
            match chars.peek() {
                Some(b'+') => {
                    chars.next();
                }
                Some(b'-') => {
                    sign = -1;
                    chars.next();
                }
                _ => {}
            }
            let mut e: i32 = 0;
            let mut any = false;
            for c in chars {
                if !c.is_ascii_digit() {
                    return None;
                }
                e = e.saturating_mul(10).saturating_add((c - b'0') as i32);
                any = true;
            }
            if !any {
                return None;
            }
            exp += sign * e;
        }
        Some(_) => return None,
    }
    Some(mantissa * (exp as f64).exp2())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<Token> {
        let mut l = Lexer::new(src.as_bytes());
        let mut out = Vec::new();
        loop {
            let (t, _) = l.next_token().unwrap();
            if t == Token::Eof {
                return out;
            }
            out.push(t);
        }
    }

    /// Lexes `src` expecting a failure and returns the error message.
    fn lex_err(src: &str) -> String {
        let mut l = Lexer::new(src.as_bytes());
        loop {
            match l.next_token() {
                Ok((Token::Eof, _)) => panic!("expected lex error: {src:?}"),
                Ok(_) => {}
                Err(e) => return e.message,
            }
        }
    }

    #[test]
    fn names_keywords_symbols() {
        assert_eq!(
            lex("local x = y0 // 2"),
            vec![
                Token::Local,
                Token::Name("x".into()),
                Token::Assign,
                Token::Name("y0".into()),
                Token::DoubleSlash,
                Token::Int(2),
            ]
        );
        assert_eq!(
            lex(".. ... . :: : ~= ~ << <= <"),
            vec![
                Token::Concat,
                Token::Ellipsis,
                Token::Dot,
                Token::DoubleColon,
                Token::Colon,
                Token::Ne,
                Token::Tilde,
                Token::Shl,
                Token::Le,
                Token::Lt,
            ]
        );
    }

    #[test]
    #[expect(clippy::approx_constant)] // 3.1416 is the Lua manual's example
    fn numbers() {
        assert_eq!(lex("3"), vec![Token::Int(3)]);
        assert_eq!(lex("345"), vec![Token::Int(345)]);
        assert_eq!(lex("0xff"), vec![Token::Int(255)]);
        assert_eq!(lex("0xBEBADA"), vec![Token::Int(0x00BE_BADA)]);
        assert_eq!(lex("3.0"), vec![Token::Float(3.0)]);
        assert_eq!(lex("3.1416"), vec![Token::Float(3.1416)]);
        assert_eq!(lex("314.16e-2"), vec![Token::Float(3.1416)]);
        assert_eq!(lex("0.31416E1"), vec![Token::Float(3.1416)]);
        assert_eq!(lex("34e1"), vec![Token::Float(340.0)]);
        assert_eq!(lex("0x0.1E"), vec![Token::Float(0.117_187_5)]);
        assert_eq!(lex("0xA23p-4"), vec![Token::Float(162.1875)]);
        assert_eq!(
            lex("0X1.921FB54442D18P+1"),
            vec![Token::Float(std::f64::consts::PI)]
        );
        // decimal overflow -> float; hex overflow -> wraps
        assert_eq!(
            lex("9223372036854775808"),
            vec![Token::Float(9.223_372_036_854_776e18)]
        );
        assert_eq!(lex("0xFFFFFFFFFFFFFFFF"), vec![Token::Int(-1)]);
        assert_eq!(lex(".5"), vec![Token::Float(0.5)]);
    }

    #[test]
    fn strings() {
        assert_eq!(
            lex(r#""hello""#),
            vec![Token::Str(b"hello".to_vec().into())]
        );
        assert_eq!(
            lex(r"'a\n\t\\\'b'"),
            vec![Token::Str(b"a\n\t\\'b".to_vec().into())]
        );
        assert_eq!(
            lex(r#""\x41\65\66""#),
            vec![Token::Str(b"AAB".to_vec().into())]
        );
        assert_eq!(
            lex(r#""\u{48}\u{65}""#),
            vec![Token::Str(b"He".to_vec().into())]
        );
        assert_eq!(
            lex(r#""\u{20AC}""#),
            vec![Token::Str("€".as_bytes().to_vec().into())]
        );
        assert_eq!(
            lex("\"a\\z  \n  b\""),
            vec![Token::Str(b"ab".to_vec().into())]
        );
        assert_eq!(
            lex("[[long\nstring]]"),
            vec![Token::Str(b"long\nstring".to_vec().into())]
        );
        assert_eq!(
            lex("[==[a]=]b]==]"),
            vec![Token::Str(b"a]=]b".to_vec().into())]
        );
        assert_eq!(
            lex("[[\nskipped]]"),
            vec![Token::Str(b"skipped".to_vec().into())]
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            lex("a -- comment\nb"),
            vec![Token::Name("a".into()), Token::Name("b".into())]
        );
        assert_eq!(
            lex("a --[==[ long\ncomment ]==] b"),
            vec![Token::Name("a".into()), Token::Name("b".into())]
        );
    }

    #[test]
    fn line_tracking() {
        let mut l = Lexer::new(b"a\nb\r\nc");
        assert_eq!(l.next_token().unwrap().1, 1);
        assert_eq!(l.next_token().unwrap().1, 2);
        assert_eq!(l.next_token().unwrap().1, 3);
    }

    #[test]
    fn errors() {
        let mut l = Lexer::new(b"\"unfinished");
        assert!(l.next_token().is_err());
        let mut l = Lexer::new(b"[[unfinished");
        assert!(l.next_token().is_err());
        let mut l = Lexer::new(b"3a");
        assert!(l.next_token().is_err());
    }

    #[test]
    fn malformed_escapes_report_puc_near_text() {
        // PUC appends `near '<source text>'` naming the malformed token.
        assert_eq!(
            lex_err(r#""\x""#),
            r#"hexadecimal digit expected near '"\x"'"#
        );
        assert_eq!(
            lex_err(r#""\xG""#),
            r#"hexadecimal digit expected near '"\xG'"#
        );
        // \u{...} diagnostics
        assert_eq!(lex_err(r#""\u""#), r#"missing '{' near '"\u"'"#);
        assert_eq!(
            lex_err(r#""\u{}""#),
            r#"hexadecimal digit expected near '"\u{}'"#
        );
        assert_eq!(lex_err(r#""\u{48""#), r#"missing '}' near '"\u{48"'"#);
        assert_eq!(
            lex_err(r#""\u{110000000}""#),
            r#"UTF-8 value too large near '"\u{110000000'"#
        );
        // oversized decimal escapes
        assert_eq!(
            lex_err(r#""\256""#),
            r#"decimal escape too large near '"\256"'"#
        );
        assert_eq!(
            lex_err(r#""\999""#),
            r#"decimal escape too large near '"\999"'"#
        );
        // an unknown escape names the offending character
        assert_eq!(lex_err(r#""\q""#), r#"invalid escape sequence near '"\q'"#);
    }

    #[test]
    fn unfinished_strings_report_near_text_or_eof() {
        // Hitting a newline reports the text consumed so far.
        assert_eq!(lex_err("\"a\nb\""), "unfinished string near '\"a'");
        assert_eq!(lex_err("'x\rY'"), "unfinished string near ''x'");
        // Running off the end of input reports `<eof>`.
        assert_eq!(lex_err("\"abc"), "unfinished string near <eof>");
        assert_eq!(lex_err("\"abc\\"), "unfinished string near <eof>");
        assert_eq!(
            lex_err("[[abc"),
            "unfinished long string/comment near <eof>"
        );
    }
}
