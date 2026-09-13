//! Lua 5.4 pattern matching (a port of lstrlib.c's matcher).
//!
//! Pure byte-level matching with no VM involvement; recursion is bounded
//! like PUC's MAXCCALLS so malicious patterns can't blow the host stack.

const MAX_CAPTURES: usize = 32;
const MAX_DEPTH: usize = 200;
const L_ESC: u8 = b'%';

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Capture {
    /// Byte span (0-based, exclusive end).
    Span(usize, usize),
    /// Position capture `()` (0-based position).
    Pos(usize),
}

#[derive(Debug)]
pub struct Match {
    /// 0-based, exclusive end.
    pub start: usize,
    pub end: usize,
    pub captures: Vec<Capture>,
}

struct MatchState<'a> {
    src: &'a [u8],
    pat: &'a [u8],
    caps: Vec<(usize, CapLen)>,
    depth: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum CapLen {
    Unfinished,
    Position,
    Len(usize),
}

/// Finds the first match of `pat` in `src` at or after `init` (0-based).
///
/// # Errors
///
/// Returns an error if the pattern is malformed, e.g. it has an unfinished
/// capture, references an invalid capture index, or nests beyond the
/// matcher's recursion limit.
pub fn find(src: &[u8], pat: &[u8], init: usize) -> Result<Option<Match>, String> {
    if init > src.len() {
        return Ok(None);
    }
    let (anchored, pstart) = match pat.first() {
        Some(b'^') => (true, 1),
        _ => (false, 0),
    };
    let mut ms = MatchState {
        src,
        pat,
        caps: Vec::new(),
        depth: 0,
    };
    let mut s = init;
    loop {
        ms.caps.clear();
        ms.depth = 0;
        if let Some(end) = ms.do_match(s, pstart)? {
            let captures = ms
                .caps
                .iter()
                .map(|&(start, len)| match len {
                    CapLen::Position => Ok(Capture::Pos(start)),
                    CapLen::Len(l) => Ok(Capture::Span(start, start + l)),
                    CapLen::Unfinished => Err("unfinished capture".to_string()),
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Some(Match {
                start: s,
                end,
                captures,
            }));
        }
        if anchored || s >= src.len() {
            return Ok(None);
        }
        s += 1;
    }
}

impl MatchState<'_> {
    /// Attempts to match `pat[p..]` at `src[s..]`; returns the end position.
    fn do_match(&mut self, mut s: usize, mut p: usize) -> Result<Option<usize>, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("pattern too complex".into());
        }
        let r = self.do_match_inner(&mut s, &mut p);
        self.depth -= 1;
        r
    }

    fn do_match_inner(&mut self, s: &mut usize, p: &mut usize) -> Result<Option<usize>, String> {
        loop {
            if *p >= self.pat.len() {
                return Ok(Some(*s));
            }
            match self.pat[*p] {
                b'(' => {
                    return if self.pat.get(*p + 1) == Some(&b')') {
                        self.start_capture(*s, *p + 2, CapLen::Position)
                    } else {
                        self.start_capture(*s, *p + 1, CapLen::Unfinished)
                    };
                }
                b')' => return self.end_capture(*s, *p + 1),
                b'$' if *p + 1 == self.pat.len() => {
                    return Ok(if *s == self.src.len() { Some(*s) } else { None });
                }
                L_ESC => match self.pat.get(*p + 1) {
                    Some(b'b') => match self.match_balance(*s, *p + 2)? {
                        Some(ns) => {
                            *s = ns;
                            *p += 4;
                            continue;
                        }
                        None => return Ok(None),
                    },
                    Some(b'f') => {
                        *p += 2;
                        if self.pat.get(*p) != Some(&b'[') {
                            return Err("missing '[' after '%f' in pattern".into());
                        }
                        let ep = self.class_end(*p)?;
                        let prev = if *s == 0 { 0 } else { self.src[*s - 1] };
                        let cur = self.src.get(*s).copied().unwrap_or(0);
                        if !self.match_bracket_class(prev, *p, ep - 1)
                            && self.match_bracket_class(cur, *p, ep - 1)
                        {
                            *p = ep;
                            continue;
                        }
                        return Ok(None);
                    }
                    Some(c @ b'0'..=b'9') => match self.match_capture(*s, (c - b'0') as usize)? {
                        Some(ns) => {
                            *s = ns;
                            *p += 2;
                            continue;
                        }
                        None => return Ok(None),
                    },
                    _ => {} // fall through to default single-char matching
                },
                _ => {}
            }
            // default: single char class possibly followed by a quantifier
            let ep = self.class_end(*p)?;
            let matches = self.single_match(*s, *p, ep);
            match self.pat.get(ep) {
                Some(b'?') => {
                    if matches && let Some(r) = self.do_match(*s + 1, ep + 1)? {
                        return Ok(Some(r));
                    }
                    *p = ep + 1;
                }
                Some(b'+') => {
                    return if matches {
                        self.max_expand(*s + 1, *p, ep)
                    } else {
                        Ok(None)
                    };
                }
                Some(b'*') => return self.max_expand(*s, *p, ep),
                Some(b'-') => return self.min_expand(*s, *p, ep),
                _ => {
                    if !matches {
                        return Ok(None);
                    }
                    *s += 1;
                    *p = ep;
                }
            }
        }
    }

    fn start_capture(&mut self, s: usize, p: usize, what: CapLen) -> Result<Option<usize>, String> {
        if self.caps.len() >= MAX_CAPTURES {
            return Err("too many captures".into());
        }
        self.caps.push((s, what));
        let r = self.do_match(s, p)?;
        if r.is_none() {
            self.caps.pop();
        }
        Ok(r)
    }

    fn end_capture(&mut self, s: usize, p: usize) -> Result<Option<usize>, String> {
        let l = self
            .caps
            .iter()
            .rposition(|&(_, len)| len == CapLen::Unfinished)
            .ok_or_else(|| "invalid pattern capture".to_string())?;
        self.caps[l].1 = CapLen::Len(s - self.caps[l].0);
        let r = self.do_match(s, p)?;
        if r.is_none() {
            self.caps[l].1 = CapLen::Unfinished;
        }
        Ok(r)
    }

    fn match_capture(&mut self, s: usize, idx: usize) -> Result<Option<usize>, String> {
        let idx = idx
            .checked_sub(1)
            .ok_or_else(|| "invalid capture index %0".to_string())?;
        let Some(&(start, CapLen::Len(len))) = self.caps.get(idx) else {
            return Err(format!("invalid capture index %{}", idx + 1));
        };
        let cap = &self.src[start..start + len];
        if self.src[s..].starts_with(cap) {
            Ok(Some(s + len))
        } else {
            Ok(None)
        }
    }

    fn match_balance(&mut self, s: usize, p: usize) -> Result<Option<usize>, String> {
        if p + 1 >= self.pat.len() {
            return Err("malformed pattern (missing arguments to '%b')".into());
        }
        if s >= self.src.len() || self.src[s] != self.pat[p] {
            return Ok(None);
        }
        let (open, close) = (self.pat[p], self.pat[p + 1]);
        let mut cont = 1;
        let mut i = s + 1;
        while i < self.src.len() {
            if self.src[i] == close {
                cont -= 1;
                if cont == 0 {
                    return Ok(Some(i + 1));
                }
            } else if self.src[i] == open {
                cont += 1;
            }
            i += 1;
        }
        Ok(None)
    }

    fn max_expand(&mut self, s: usize, p: usize, ep: usize) -> Result<Option<usize>, String> {
        let mut i = 0;
        while self.single_match(s + i, p, ep) {
            i += 1;
        }
        loop {
            if let Some(r) = self.do_match(s + i, ep + 1)? {
                return Ok(Some(r));
            }
            if i == 0 {
                return Ok(None);
            }
            i -= 1;
        }
    }

    fn min_expand(&mut self, mut s: usize, p: usize, ep: usize) -> Result<Option<usize>, String> {
        loop {
            if let Some(r) = self.do_match(s, ep + 1)? {
                return Ok(Some(r));
            }
            if self.single_match(s, p, ep) {
                s += 1;
            } else {
                return Ok(None);
            }
        }
    }

    /// End of the single-char class starting at `p` (one past it).
    fn class_end(&self, mut p: usize) -> Result<usize, String> {
        match self.pat[p] {
            L_ESC => {
                if p + 1 >= self.pat.len() {
                    return Err("malformed pattern (ends with '%')".into());
                }
                Ok(p + 2)
            }
            b'[' => {
                p += 1;
                if self.pat.get(p) == Some(&b'^') {
                    p += 1;
                }
                // do-while: consume at least one item, so a leading ']' is
                // a literal member of the set
                loop {
                    if p >= self.pat.len() {
                        return Err("malformed pattern (missing ']')".into());
                    }
                    let c = self.pat[p];
                    p += 1;
                    if c == L_ESC {
                        if p >= self.pat.len() {
                            return Err("malformed pattern (ends with '%')".into());
                        }
                        p += 1;
                    }
                    if self.pat.get(p) == Some(&b']') {
                        return Ok(p + 1);
                    }
                }
            }
            _ => Ok(p + 1),
        }
    }

    fn single_match(&self, s: usize, p: usize, ep: usize) -> bool {
        let Some(&c) = self.src.get(s) else {
            return false;
        };
        match self.pat[p] {
            b'.' => true,
            L_ESC => match_class(c, self.pat[p + 1]),
            b'[' => self.match_bracket_class(c, p, ep - 1),
            pc => pc == c,
        }
    }

    /// Matches `c` against the set `pat[p..=ec]` where `pat[p] == '['` and
    /// `pat[ec] == ']'`.
    fn match_bracket_class(&self, c: u8, mut p: usize, ec: usize) -> bool {
        let mut sig = true;
        p += 1;
        if self.pat.get(p) == Some(&b'^') {
            sig = false;
            p += 1;
        }
        while p < ec {
            if self.pat[p] == L_ESC {
                p += 1;
                if match_class(c, self.pat[p]) {
                    return sig;
                }
                p += 1;
            } else if self.pat.get(p + 1) == Some(&b'-') && p + 2 < ec {
                if self.pat[p] <= c && c <= self.pat[p + 2] {
                    return sig;
                }
                p += 3;
            } else {
                if self.pat[p] == c {
                    return sig;
                }
                p += 1;
            }
        }
        !sig
    }
}

fn match_class(c: u8, cl: u8) -> bool {
    let res = match cl.to_ascii_lowercase() {
        b'a' => c.is_ascii_alphabetic(),
        b'c' => c.is_ascii_control(),
        b'd' => c.is_ascii_digit(),
        b'g' => c.is_ascii_graphic(),
        b'l' => c.is_ascii_lowercase(),
        b'p' => c.is_ascii_punctuation(),
        b's' => c == b' ' || (0x09..=0x0d).contains(&c),
        b'u' => c.is_ascii_uppercase(),
        b'w' => c.is_ascii_alphanumeric(),
        b'x' => c.is_ascii_hexdigit(),
        // deprecated `%z`: matches only the zero byte (`%Z` its complement)
        b'z' => c == 0,
        _ => return cl == c, // escaped literal (%%, %., ...)
    };
    if cl.is_ascii_uppercase() { !res } else { res }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str, p: &str) -> Option<(usize, usize)> {
        find(s.as_bytes(), p.as_bytes(), 0)
            .unwrap()
            .map(|m| (m.start, m.end))
    }

    fn cap(s: &str, p: &str, i: usize) -> String {
        let m = find(s.as_bytes(), p.as_bytes(), 0).unwrap().unwrap();
        match m.captures[i] {
            Capture::Span(a, b) => s[a..b].to_string(),
            Capture::Pos(p) => format!("@{p}"),
        }
    }

    #[test]
    fn literals_and_classes() {
        assert_eq!(f("hello world", "world"), Some((6, 11)));
        assert_eq!(f("hello", "xyz"), None);
        assert_eq!(f("abc123", "%d+"), Some((3, 6)));
        assert_eq!(f("abc123", "%a+"), Some((0, 3)));
        assert_eq!(f("  hi", "%s+"), Some((0, 2)));
        assert_eq!(f("foo.bar", "%."), Some((3, 4)));
        assert_eq!(f("ABCdef", "%u+"), Some((0, 3)));
        assert_eq!(f("ABCdef", "%U+"), Some((3, 6))); // complement
    }

    #[test]
    fn anchors_and_quantifiers() {
        assert_eq!(f("aaa", "^a+$"), Some((0, 3)));
        assert_eq!(f("baaa", "^a"), None);
        assert_eq!(f("abc", "x*"), Some((0, 0))); // empty match
        assert_eq!(f("aaab", "a-b"), Some((0, 4))); // lazy
        assert_eq!(f("aaab", "a*b"), Some((0, 4)));
        assert_eq!(f("ab", "a?b"), Some((0, 2)));
        assert_eq!(f("b", "a?b"), Some((0, 1)));
        assert_eq!(f("<<x>>", "<+"), Some((0, 2)));
    }

    #[test]
    fn sets() {
        assert_eq!(f("hello42", "[0-9]+"), Some((5, 7)));
        assert_eq!(f("hello42", "[^0-9]+"), Some((0, 5)));
        assert_eq!(f("a-b", "[%-]"), Some((1, 2)));
        assert_eq!(f("x]y", "[%]]"), Some((1, 2)));
        assert_eq!(f("abc", "[abc]+"), Some((0, 3)));
    }

    #[test]
    fn captures() {
        assert_eq!(cap("key=value", "(%w+)=(%w+)", 0), "key");
        assert_eq!(cap("key=value", "(%w+)=(%w+)", 1), "value");
        assert_eq!(cap("hello", "()ll", 0), "@2"); // position capture
        // backreference
        assert_eq!(f("abcabc", "(abc)%1"), Some((0, 6)));
        assert_eq!(f("abcabd", "(abc)%1"), None);
        // nested
        assert_eq!(cap("(foo)", "%((%w+)%)", 0), "foo");
    }

    #[test]
    fn balance_and_frontier() {
        assert_eq!(f("(nested (parens)) after", "%b()"), Some((0, 17)));
        assert_eq!(f("no parens", "%b()"), None);
        assert_eq!(f("THE (quick) fox", "%f[%a]%a+"), Some((0, 3)));
    }

    #[test]
    fn date_pattern() {
        let m = find(b"today is 2026-06-10!", b"(%d+)-(%d+)-(%d+)", 0)
            .unwrap()
            .unwrap();
        assert_eq!((m.start, m.end), (9, 19));
        assert_eq!(m.captures.len(), 3);
    }

    #[test]
    fn malformed() {
        assert!(find(b"x", b"[abc", 0).is_err());
        assert!(find(b"x", b"%", 0).is_err());
        assert!(find(b"a", b"(a", 0).is_err()); // unfinished capture
    }
}
