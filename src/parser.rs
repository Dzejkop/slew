//! Recursive-descent parser for the full Lua 5.4 grammar.

use crate::ast::*;
use crate::lexer::{LexError, Lexer, Token};
use std::fmt;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub line: u32,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at line {}: {}", self.line, self.message)
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError { message: e.message, line: e.line }
    }
}

pub fn parse(src: &[u8]) -> Result<Block, ParseError> {
    let mut p = Parser::new(src)?;
    let block = p.block()?;
    p.expect_token(Token::Eof)?;
    Ok(block)
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    tok: Token,
    line: u32,
}

/// (left, right) binding powers; right < left means right-associative.
fn binop_prec(op: BinOp) -> (u8, u8) {
    use BinOp::*;
    match op {
        Or => (1, 1),
        And => (2, 2),
        Lt | Gt | Le | Ge | Ne | Eq => (3, 3),
        BOr => (4, 4),
        BXor => (5, 5),
        BAnd => (6, 6),
        Shl | Shr => (7, 7),
        Concat => (9, 8),
        Add | Sub => (10, 10),
        Mul | Div | IDiv | Mod => (11, 11),
        Pow => (14, 13),
    }
}

const UNARY_PREC: u8 = 12;

fn token_binop(tok: &Token) -> Option<BinOp> {
    Some(match tok {
        Token::Plus => BinOp::Add,
        Token::Minus => BinOp::Sub,
        Token::Star => BinOp::Mul,
        Token::Slash => BinOp::Div,
        Token::DoubleSlash => BinOp::IDiv,
        Token::Percent => BinOp::Mod,
        Token::Caret => BinOp::Pow,
        Token::Concat => BinOp::Concat,
        Token::Eq => BinOp::Eq,
        Token::Ne => BinOp::Ne,
        Token::Lt => BinOp::Lt,
        Token::Le => BinOp::Le,
        Token::Gt => BinOp::Gt,
        Token::Ge => BinOp::Ge,
        Token::And => BinOp::And,
        Token::Or => BinOp::Or,
        Token::Amp => BinOp::BAnd,
        Token::Pipe => BinOp::BOr,
        Token::Tilde => BinOp::BXor,
        Token::Shl => BinOp::Shl,
        Token::Shr => BinOp::Shr,
        _ => return None,
    })
}

impl<'a> Parser<'a> {
    fn new(src: &'a [u8]) -> Result<Self, ParseError> {
        let mut lexer = Lexer::new(src);
        let (tok, line) = lexer.next_token()?;
        Ok(Parser { lexer, tok, line })
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError { message: message.into(), line: self.line })
    }

    fn advance(&mut self) -> Result<Token, ParseError> {
        let (tok, line) = self.lexer.next_token()?;
        self.line = line;
        Ok(std::mem::replace(&mut self.tok, tok))
    }

    fn check(&mut self, tok: &Token) -> Result<bool, ParseError> {
        if self.tok == *tok {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect_token(&mut self, tok: Token) -> Result<(), ParseError> {
        if self.tok == tok {
            self.advance()?;
            Ok(())
        } else {
            self.err(format!("'{}' expected near '{}'", tok, self.tok))
        }
    }

    fn expect_name(&mut self) -> Result<Box<str>, ParseError> {
        match self.tok {
            Token::Name(_) => {
                let Token::Name(n) = self.advance()? else { unreachable!() };
                Ok(n)
            }
            _ => self.err(format!("<name> expected near '{}'", self.tok)),
        }
    }

    fn block_ends(&self) -> bool {
        matches!(
            self.tok,
            Token::Eof | Token::End | Token::Else | Token::Elseif | Token::Until
        )
    }

    fn block(&mut self) -> Result<Block, ParseError> {
        let mut stmts = Vec::new();
        loop {
            if self.block_ends() {
                return Ok(Block { stmts });
            }
            if self.tok == Token::Return {
                stmts.push(self.return_stat()?);
                return Ok(Block { stmts });
            }
            let s = self.statement()?;
            if !matches!(s, Stmt::Empty) {
                stmts.push(s);
            }
        }
    }

    fn return_stat(&mut self) -> Result<Stmt, ParseError> {
        let line = self.line;
        self.advance()?; // 'return'
        let exprs = if self.block_ends() || self.tok == Token::Semi {
            Vec::new()
        } else {
            self.expr_list()?
        };
        self.check(&Token::Semi)?;
        if !self.block_ends() {
            return self.err(format!("'end' expected near '{}'", self.tok));
        }
        Ok(Stmt::Return { exprs, line })
    }

    fn statement(&mut self) -> Result<Stmt, ParseError> {
        let line = self.line;
        match &self.tok {
            Token::Semi => {
                self.advance()?;
                Ok(Stmt::Empty)
            }
            Token::Break => {
                self.advance()?;
                Ok(Stmt::Break(line))
            }
            Token::Goto => {
                self.advance()?;
                Ok(Stmt::Goto { label: self.expect_name()?, line })
            }
            Token::DoubleColon => {
                self.advance()?;
                let name = self.expect_name()?;
                self.expect_token(Token::DoubleColon)?;
                Ok(Stmt::Label(name))
            }
            Token::Do => {
                self.advance()?;
                let body = self.block()?;
                self.expect_token(Token::End)?;
                Ok(Stmt::Do(body))
            }
            Token::While => {
                self.advance()?;
                let cond = self.expr()?;
                self.expect_token(Token::Do)?;
                let body = self.block()?;
                self.expect_token(Token::End)?;
                Ok(Stmt::While { cond, body })
            }
            Token::Repeat => {
                self.advance()?;
                let body = self.block()?;
                self.expect_token(Token::Until)?;
                let cond = self.expr()?;
                Ok(Stmt::Repeat { body, cond })
            }
            Token::If => self.if_stat(),
            Token::For => self.for_stat(),
            Token::Function => self.function_stat(),
            Token::Local => self.local_stat(),
            _ => self.expr_stat(),
        }
    }

    fn if_stat(&mut self) -> Result<Stmt, ParseError> {
        let mut arms = Vec::new();
        loop {
            self.advance()?; // 'if' / 'elseif'
            let cond = self.expr()?;
            self.expect_token(Token::Then)?;
            let body = self.block()?;
            arms.push((cond, body));
            if self.tok != Token::Elseif {
                break;
            }
        }
        let else_block = if self.check(&Token::Else)? {
            Some(self.block()?)
        } else {
            None
        };
        self.expect_token(Token::End)?;
        Ok(Stmt::If { arms, else_block })
    }

    fn for_stat(&mut self) -> Result<Stmt, ParseError> {
        let line = self.line;
        self.advance()?; // 'for'
        let first = self.expect_name()?;
        if self.check(&Token::Assign)? {
            let start = self.expr()?;
            self.expect_token(Token::Comma)?;
            let end = self.expr()?;
            let step = if self.check(&Token::Comma)? {
                Some(self.expr()?)
            } else {
                None
            };
            self.expect_token(Token::Do)?;
            let body = self.block()?;
            self.expect_token(Token::End)?;
            Ok(Stmt::NumericFor { var: first, start, end, step, body, line })
        } else {
            let mut vars = vec![first];
            while self.check(&Token::Comma)? {
                vars.push(self.expect_name()?);
            }
            self.expect_token(Token::In)?;
            let exprs = self.expr_list()?;
            self.expect_token(Token::Do)?;
            let body = self.block()?;
            self.expect_token(Token::End)?;
            Ok(Stmt::GenericFor { vars, exprs, body, line })
        }
    }

    fn function_stat(&mut self) -> Result<Stmt, ParseError> {
        let line = self.line;
        self.advance()?; // 'function'
        let mut target = Expr::Name(self.expect_name()?, line);
        let mut is_method = false;
        loop {
            if self.check(&Token::Dot)? {
                let key = self.expect_name()?;
                target = Expr::Index {
                    obj: Box::new(target),
                    key: Box::new(Expr::Str(key.as_bytes().into())),
                    line,
                };
            } else if self.check(&Token::Colon)? {
                let key = self.expect_name()?;
                target = Expr::Index {
                    obj: Box::new(target),
                    key: Box::new(Expr::Str(key.as_bytes().into())),
                    line,
                };
                is_method = true;
                break;
            } else {
                break;
            }
        }
        let mut body = self.func_body(line)?;
        if is_method {
            body.params.insert(0, "self".into());
        }
        Ok(Stmt::Function { target, body })
    }

    fn local_stat(&mut self) -> Result<Stmt, ParseError> {
        let line = self.line;
        self.advance()?; // 'local'
        if self.check(&Token::Function)? {
            let name = self.expect_name()?;
            let body = self.func_body(line)?;
            return Ok(Stmt::LocalFunction { name, body });
        }
        let mut names = Vec::new();
        loop {
            let name = self.expect_name()?;
            let attrib = if self.check(&Token::Lt)? {
                let a = self.expect_name()?;
                self.expect_token(Token::Gt)?;
                match &*a {
                    "const" => Attrib::Const,
                    "close" => Attrib::Close,
                    _ => return self.err(format!("unknown attribute '{a}'")),
                }
            } else {
                Attrib::None
            };
            names.push((name, attrib));
            if !self.check(&Token::Comma)? {
                break;
            }
        }
        let values = if self.check(&Token::Assign)? {
            self.expr_list()?
        } else {
            Vec::new()
        };
        Ok(Stmt::Local { names, values, line })
    }

    /// Expression statement: assignment or call.
    fn expr_stat(&mut self) -> Result<Stmt, ParseError> {
        let line = self.line;
        let first = self.suffixed_expr()?;
        if self.tok == Token::Assign || self.tok == Token::Comma {
            let mut targets = vec![first];
            while self.check(&Token::Comma)? {
                targets.push(self.suffixed_expr()?);
            }
            for t in &targets {
                if !matches!(t, Expr::Name(..) | Expr::Index { .. }) {
                    return self.err("syntax error: cannot assign to this expression");
                }
            }
            self.expect_token(Token::Assign)?;
            let values = self.expr_list()?;
            Ok(Stmt::Assign { targets, values, line })
        } else {
            if !matches!(first, Expr::Call { .. } | Expr::MethodCall { .. }) {
                return self.err("syntax error: unexpected expression statement");
            }
            Ok(Stmt::ExprStat(first))
        }
    }

    fn func_body(&mut self, line: u32) -> Result<FuncBody, ParseError> {
        self.expect_token(Token::LParen)?;
        let mut params = Vec::new();
        let mut is_vararg = false;
        if self.tok != Token::RParen {
            loop {
                match &self.tok {
                    Token::Ellipsis => {
                        self.advance()?;
                        is_vararg = true;
                        break;
                    }
                    Token::Name(_) => params.push(self.expect_name()?),
                    _ => return self.err(format!("<name> expected near '{}'", self.tok)),
                }
                if !self.check(&Token::Comma)? {
                    break;
                }
            }
        }
        self.expect_token(Token::RParen)?;
        let body = self.block()?;
        self.expect_token(Token::End)?;
        Ok(FuncBody { params, is_vararg, body, line })
    }

    fn expr_list(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut exprs = vec![self.expr()?];
        while self.check(&Token::Comma)? {
            exprs.push(self.expr()?);
        }
        Ok(exprs)
    }

    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.sub_expr(0)
    }

    /// Precedence climbing.
    fn sub_expr(&mut self, limit: u8) -> Result<Expr, ParseError> {
        let line = self.line;
        let mut lhs = match &self.tok {
            Token::Not => {
                self.advance()?;
                let operand = self.sub_expr(UNARY_PREC)?;
                Expr::UnOp { op: UnOp::Not, operand: Box::new(operand), line }
            }
            Token::Minus => {
                self.advance()?;
                let operand = self.sub_expr(UNARY_PREC)?;
                Expr::UnOp { op: UnOp::Neg, operand: Box::new(operand), line }
            }
            Token::Hash => {
                self.advance()?;
                let operand = self.sub_expr(UNARY_PREC)?;
                Expr::UnOp { op: UnOp::Len, operand: Box::new(operand), line }
            }
            Token::Tilde => {
                self.advance()?;
                let operand = self.sub_expr(UNARY_PREC)?;
                Expr::UnOp { op: UnOp::BNot, operand: Box::new(operand), line }
            }
            _ => self.simple_expr()?,
        };
        while let Some(op) = token_binop(&self.tok) {
            let (left, right) = binop_prec(op);
            if left <= limit {
                break;
            }
            let line = self.line;
            self.advance()?;
            let rhs = self.sub_expr(right)?;
            lhs = Expr::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs), line };
        }
        Ok(lhs)
    }

    fn simple_expr(&mut self) -> Result<Expr, ParseError> {
        let line = self.line;
        match &self.tok {
            Token::Nil => {
                self.advance()?;
                Ok(Expr::Nil)
            }
            Token::True => {
                self.advance()?;
                Ok(Expr::True)
            }
            Token::False => {
                self.advance()?;
                Ok(Expr::False)
            }
            Token::Int(_) => {
                let Token::Int(i) = self.advance()? else { unreachable!() };
                Ok(Expr::Int(i))
            }
            Token::Float(_) => {
                let Token::Float(f) = self.advance()? else { unreachable!() };
                Ok(Expr::Float(f))
            }
            Token::Str(_) => {
                let Token::Str(s) = self.advance()? else { unreachable!() };
                Ok(Expr::Str(s))
            }
            Token::Ellipsis => {
                self.advance()?;
                Ok(Expr::Vararg(line))
            }
            Token::Function => {
                self.advance()?;
                Ok(Expr::Function(Box::new(self.func_body(line)?)))
            }
            Token::LBrace => self.table_constructor(),
            _ => self.suffixed_expr(),
        }
    }

    /// primaryexp { '.' Name | '[' exp ']' | ':' Name args | args }
    fn suffixed_expr(&mut self) -> Result<Expr, ParseError> {
        let line = self.line;
        let mut e = match &self.tok {
            Token::Name(_) => Expr::Name(self.expect_name()?, line),
            Token::LParen => {
                self.advance()?;
                let inner = self.expr()?;
                self.expect_token(Token::RParen)?;
                Expr::Paren(Box::new(inner))
            }
            t => return self.err(format!("unexpected symbol near '{t}'")),
        };
        loop {
            let line = self.line;
            match &self.tok {
                Token::Dot => {
                    self.advance()?;
                    let key = self.expect_name()?;
                    e = Expr::Index {
                        obj: Box::new(e),
                        key: Box::new(Expr::Str(key.as_bytes().into())),
                        line,
                    };
                }
                Token::LBracket => {
                    self.advance()?;
                    let key = self.expr()?;
                    self.expect_token(Token::RBracket)?;
                    e = Expr::Index { obj: Box::new(e), key: Box::new(key), line };
                }
                Token::Colon => {
                    self.advance()?;
                    let name = self.expect_name()?;
                    let args = self.call_args()?;
                    e = Expr::MethodCall { obj: Box::new(e), name, args, line };
                }
                Token::LParen | Token::Str(_) | Token::LBrace => {
                    let args = self.call_args()?;
                    e = Expr::Call { func: Box::new(e), args, line };
                }
                _ => return Ok(e),
            }
        }
    }

    fn call_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        match &self.tok {
            Token::LParen => {
                self.advance()?;
                let args = if self.tok == Token::RParen {
                    Vec::new()
                } else {
                    self.expr_list()?
                };
                self.expect_token(Token::RParen)?;
                Ok(args)
            }
            Token::Str(_) => {
                let Token::Str(s) = self.advance()? else { unreachable!() };
                Ok(vec![Expr::Str(s)])
            }
            Token::LBrace => Ok(vec![self.table_constructor()?]),
            t => self.err(format!("function arguments expected near '{t}'")),
        }
    }

    fn table_constructor(&mut self) -> Result<Expr, ParseError> {
        let line = self.line;
        self.expect_token(Token::LBrace)?;
        let mut items = Vec::new();
        let mut pairs = Vec::new();
        while self.tok != Token::RBrace {
            match &self.tok {
                Token::LBracket => {
                    self.advance()?;
                    let k = self.expr()?;
                    self.expect_token(Token::RBracket)?;
                    self.expect_token(Token::Assign)?;
                    let v = self.expr()?;
                    pairs.push((k, v));
                }
                Token::Name(_) if self.peek_is_assign()? => {
                    let Token::Name(n) = self.advance()? else { unreachable!() };
                    let k = Expr::Str(n.as_bytes().into());
                    self.advance()?; // '='
                    let v = self.expr()?;
                    pairs.push((k, v));
                }
                _ => items.push(self.expr()?),
            }
            if !(self.check(&Token::Comma)? || self.check(&Token::Semi)?) {
                break;
            }
        }
        self.expect_token(Token::RBrace)?;
        Ok(Expr::Table { items, pairs, line })
    }

    /// Looks ahead one token past the current Name to detect `Name =` in a
    /// table constructor (vs. `Name` as the start of an expression).
    fn peek_is_assign(&self) -> Result<bool, ParseError> {
        let mut probe = self.lexer.clone();
        let (next, _) = probe.next_token()?;
        Ok(next == Token::Assign)
    }
}
