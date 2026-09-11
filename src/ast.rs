//! AST for the full Lua 5.4 grammar.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Len,
    BNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attrib {
    None,
    Const,
    Close,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct FuncBody {
    pub params: Vec<Box<str>>,
    pub is_vararg: bool,
    pub body: Block,
    pub line: u32,
    /// Line of the closing `end`. Feeds `getinfo`'s `lastlinedefined` and the
    /// line attached to the implicit final `RETURN`.
    pub end_line: u32,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `;`
    Empty,
    Assign {
        targets: Vec<Expr>, // Name / Index only
        values: Vec<Expr>,
        line: u32,
    },
    /// Function or method call evaluated for side effects.
    ExprStat(Expr),
    Do(Block),
    While {
        cond: Expr,
        body: Block,
    },
    Repeat {
        body: Block,
        /// May reference locals declared in `body` (Lua scoping rule).
        cond: Expr,
    },
    If {
        /// (condition, block) for `if` and each `elseif`.
        arms: Vec<(Expr, Block)>,
        else_block: Option<Block>,
    },
    NumericFor {
        var: Box<str>,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
        body: Block,
        line: u32,
    },
    GenericFor {
        vars: Vec<Box<str>>,
        exprs: Vec<Expr>,
        body: Block,
        line: u32,
    },
    Local {
        names: Vec<(Box<str>, Attrib)>,
        values: Vec<Expr>,
        line: u32,
    },
    /// `local function name ...` (name is in scope inside the body).
    LocalFunction {
        name: Box<str>,
        body: FuncBody,
    },
    /// `function a.b.c:d() ...` — target is the Name/Index expression,
    /// `is_method` adds the implicit `self` parameter.
    Function {
        target: Expr,
        body: FuncBody,
    },
    Return {
        exprs: Vec<Expr>,
        line: u32,
    },
    Break(u32),
    Goto {
        label: Box<str>,
        line: u32,
    },
    Label(Box<str>),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Nil,
    True,
    False,
    Int(i64),
    Float(f64),
    Str(Box<[u8]>),
    Vararg(u32),
    Function(Box<FuncBody>),
    Name(Box<str>, u32),
    /// `obj[key]` (also `obj.key` with a string key).
    Index {
        obj: Box<Expr>,
        key: Box<Expr>,
        line: u32,
    },
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
        line: u32,
    },
    /// `obj:name(args)`
    MethodCall {
        obj: Box<Expr>,
        name: Box<str>,
        args: Vec<Expr>,
        line: u32,
    },
    BinOp {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        line: u32,
    },
    UnOp {
        op: UnOp,
        operand: Box<Expr>,
        line: u32,
    },
    Table {
        /// Array-part items in order; a trailing multret expression expands.
        items: Vec<Expr>,
        /// `[k] = v` and `name = v` pairs.
        pairs: Vec<(Expr, Expr)>,
        line: u32,
    },
    /// Parenthesized expression: truncates multret to one value.
    Paren(Box<Expr>),
}

impl Expr {
    /// True for expressions that can produce multiple values in tail position.
    pub fn is_multret(&self) -> bool {
        matches!(
            self,
            Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Vararg(_)
        )
    }
}
