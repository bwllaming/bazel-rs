//! The Bazel 9.3 query language: lexer, LL(1) parser and the expression
//! tree with Bazel's `toString` forms.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (Release 9.3.0); paths
//! below are relative to `src/main/java/com/google/devtools/build/lib/`.
//! The lexer is `query2/engine/Lexer.java`, the grammar
//! `query2/engine/QueryParser.java`; evaluation lives in `crate::engine::query`.

use std::fmt;

/// Lexer.TokenKind (query2/engine/Lexer.java:33-58).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Word,
    Eof,
    Comma,
    Equals,
    LParen,
    Minus,
    Plus,
    RParen,
    Caret,
    In,
    Let,
    Set,
    Intersect,
    Except,
    Union,
}

impl TokenKind {
    /// TokenKind.getPrettyName.
    pub fn pretty_name(self) -> &'static str {
        match self {
            Self::Word => "word",
            Self::Eof => "EOF",
            Self::Comma => ",",
            Self::Equals => "=",
            Self::LParen => "(",
            Self::Minus => "-",
            Self::Plus => "+",
            Self::RParen => ")",
            Self::Caret => "^",
            Self::In => "in",
            Self::Let => "let",
            Self::Set => "set",
            Self::Intersect => "intersect",
            Self::Except => "except",
            Self::Union => "union",
        }
    }

    /// Lexer.BINARY_OPERATORS (Lexer.java:60-66).
    fn is_binary_operator(self) -> bool {
        matches!(
            self,
            Self::Intersect | Self::Caret | Self::Union | Self::Plus | Self::Except | Self::Minus
        )
    }
}

/// Lexer.isReservedWord (Lexer.java:77-82): the keywords after
/// `__ALL_IDENTIFIERS_FOLLOW`.
pub fn is_reserved_word(word: &str) -> bool {
    keyword(word).is_some()
}

fn keyword(word: &str) -> Option<TokenKind> {
    Some(match word {
        "in" => TokenKind::In,
        "let" => TokenKind::Let,
        "set" => TokenKind::Set,
        "intersect" => TokenKind::Intersect,
        "except" => TokenKind::Except,
        "union" => TokenKind::Union,
        _ => return None,
    })
}

/// Lexer.Token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub word: Option<String>,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.kind, &self.word) {
            (TokenKind::Word, Some(word)) => f.write_str(word),
            (kind, _) => f.write_str(kind.pretty_name()),
        }
    }
}

/// A QuerySyntaxException message (query2/engine/QuerySyntaxException.java).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxError(pub String);

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lexer.scan (Lexer.java:113-118, tokenize :293-313).
pub fn scan(input: &str) -> Result<Vec<Token>, SyntaxError> {
    let chars = input.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut pos = 0;
    let symbol = |kind| Token { kind, word: None };
    while pos < chars.len() {
        let c = chars[pos];
        pos += 1;
        match c {
            '(' => tokens.push(symbol(TokenKind::LParen)),
            ')' => tokens.push(symbol(TokenKind::RParen)),
            ',' => tokens.push(symbol(TokenKind::Comma)),
            '+' => tokens.push(symbol(TokenKind::Plus)),
            '-' => tokens.push(symbol(TokenKind::Minus)),
            '=' => tokens.push(symbol(TokenKind::Equals)),
            '^' => tokens.push(symbol(TokenKind::Caret)),
            '\n' | ' ' | '\t' | '\r' => {}
            '\'' | '"' => {
                // quotedWord (Lexer.java:136-151): the first matching quote
                // closes; the other quote character is ordinary.
                let start = pos;
                loop {
                    if pos >= chars.len() {
                        return Err(SyntaxError("unclosed quotation".to_owned()));
                    }
                    let next = chars[pos];
                    pos += 1;
                    if next == c {
                        break;
                    }
                }
                tokens.push(Token {
                    kind: TokenKind::Word,
                    word: Some(chars[start..pos - 1].iter().collect()),
                });
            }
            _ => {
                // scanWord (Lexer.java:158-262).
                let start = pos - 1;
                let double_at = c == '@' && chars.get(pos) == Some(&'@');
                while pos < chars.len() {
                    let next = chars[pos];
                    if next.is_ascii_alphanumeric()
                        || matches!(
                            next,
                            '*' | '/' | '@' | '.' | '-' | '_' | ':' | '$' | '~' | '[' | ']'
                        )
                        || (next == '+' && double_at)
                        // Non-ASCII characters continue a word.
                        || !next.is_ascii()
                    {
                        pos += 1;
                    } else {
                        break;
                    }
                }
                let word = chars[start..pos].iter().collect::<String>();
                tokens.push(match keyword(&word) {
                    Some(kind) => symbol(kind),
                    None => Token {
                        kind: TokenKind::Word,
                        word: Some(word),
                    },
                });
            }
        }
    }
    tokens.push(symbol(TokenKind::Eof));
    Ok(tokens)
}

/// QueryEnvironment.ArgumentType.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentType {
    Expression,
    Word,
    Integer,
}

/// The functions of `bazel query` (QueryEnvironment.DEFAULT_QUERY_FUNCTIONS,
/// query2/engine/QueryEnvironment.java:741-758; Bazel registers no extra
/// query functions). `allrdeps` and `rbuildfiles` are SkyQuery-only and
/// therefore unknown functions here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Function {
    AllPaths,
    Attr,
    BuildFiles,
    Deps,
    Executables,
    Filter,
    Kind,
    Labels,
    LoadFiles,
    Rdeps,
    SamePkgDirectRdeps,
    Siblings,
    Some,
    SomePath,
    Tests,
    Visible,
}

impl Function {
    pub const ALL: [Function; 16] = [
        Self::AllPaths,
        Self::Attr,
        Self::BuildFiles,
        Self::Deps,
        Self::Executables,
        Self::Filter,
        Self::Kind,
        Self::Labels,
        Self::LoadFiles,
        Self::Rdeps,
        Self::SamePkgDirectRdeps,
        Self::Siblings,
        Self::Some,
        Self::SomePath,
        Self::Tests,
        Self::Visible,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::AllPaths => "allpaths",
            Self::Attr => "attr",
            Self::BuildFiles => "buildfiles",
            Self::Deps => "deps",
            Self::Executables => "executables",
            Self::Filter => "filter",
            Self::Kind => "kind",
            Self::Labels => "labels",
            Self::LoadFiles => "loadfiles",
            Self::Rdeps => "rdeps",
            Self::SamePkgDirectRdeps => "same_pkg_direct_rdeps",
            Self::Siblings => "siblings",
            Self::Some => "some",
            Self::SomePath => "somepath",
            Self::Tests => "tests",
            Self::Visible => "visible",
        }
    }

    fn lookup(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|function| function.name() == name)
    }

    /// QueryFunction.getMandatoryArguments of each function class.
    fn mandatory_arguments(self) -> usize {
        match self {
            Self::Attr => 3,
            Self::Rdeps => 2,
            Self::AllPaths
            | Self::SomePath
            | Self::Filter
            | Self::Kind
            | Self::Labels
            | Self::Visible => 2,
            _ => 1,
        }
    }

    /// QueryFunction.getArgumentTypes of each function class.
    fn argument_types(self) -> &'static [ArgumentType] {
        use ArgumentType::{Expression as E, Integer as I, Word as W};
        match self {
            Self::AllPaths | Self::SomePath | Self::Visible => &[E, E],
            Self::Attr => &[W, W, E],
            Self::Deps | Self::Some => &[E, I],
            Self::Rdeps => &[E, E, I],
            Self::Filter | Self::Kind | Self::Labels => &[W, E],
            Self::BuildFiles
            | Self::Executables
            | Self::LoadFiles
            | Self::SamePkgDirectRdeps
            | Self::Siblings
            | Self::Tests => &[E],
        }
    }
}

/// QueryEnvironment.Argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Argument {
    Expression(Expr),
    Word(String),
    Integer(i32),
}

impl fmt::Display for Argument {
    /// Argument.toString (QueryEnvironment.java:98-104): words are always
    /// single-quoted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expression(expr) => expr.fmt(f),
            Self::Word(word) => write!(f, "'{word}'"),
            Self::Integer(value) => write!(f, "{value}"),
        }
    }
}

/// TargetLiteral.toString quoting (TargetLiteral.java:90-115).
fn quote_word(word: &str) -> String {
    let needs = is_reserved_word(word)
        || word.is_empty()
        || word.starts_with('-')
        || word.starts_with('*')
        || word.chars().any(|c| " \"#&'()+,;<=>?[]{|}".contains(c));
    if !needs {
        return word.to_owned();
    }
    let quote = if word.contains('"') { '\'' } else { '"' };
    format!("{quote}{word}{quote}")
}

/// Binary operators (BinaryOperatorExpression).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Intersect,
    Caret,
    Union,
    Plus,
    Except,
    Minus,
}

impl Operator {
    fn of(kind: TokenKind) -> Self {
        match kind {
            TokenKind::Intersect => Self::Intersect,
            TokenKind::Caret => Self::Caret,
            TokenKind::Union => Self::Union,
            TokenKind::Plus => Self::Plus,
            TokenKind::Except => Self::Except,
            _ => Self::Minus,
        }
    }

    fn pretty_name(self) -> &'static str {
        match self {
            Self::Intersect => "intersect",
            Self::Caret => "^",
            Self::Union => "union",
            Self::Plus => "+",
            Self::Except => "except",
            Self::Minus => "-",
        }
    }
}

/// A QueryExpression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// TargetLiteral: a target pattern or a `$name` variable reference.
    Target(String),
    Binary(Operator, Vec<Expr>),
    Function(Function, Vec<Argument>),
    Let(String, Box<Expr>, Box<Expr>),
    /// SetExpression of target literals.
    Set(Vec<String>),
}

/// LetExpression.isValidVarReference (LetExpression.java:35-46).
pub fn is_variable_reference(pattern: &str) -> bool {
    pattern
        .strip_prefix('$')
        .is_some_and(is_valid_variable_name)
}

/// LetExpression.NAME_PATTERN `^[a-zA-Z_][a-zA-Z0-9_]*$`.
pub fn is_valid_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Expr {
    /// QueryExpression.collectTargetPatterns: target literals that are not
    /// variable references, left to right.
    pub fn collect_target_patterns(&self, patterns: &mut Vec<String>) {
        match self {
            Self::Target(pattern) => {
                if !is_variable_reference(pattern) && !patterns.contains(pattern) {
                    patterns.push(pattern.clone());
                }
            }
            Self::Set(words) => {
                for word in words {
                    if !is_variable_reference(word) && !patterns.contains(word) {
                        patterns.push(word.clone());
                    }
                }
            }
            Self::Binary(_, operands) => {
                for operand in operands {
                    operand.collect_target_patterns(patterns);
                }
            }
            Self::Function(_, args) => {
                for arg in args {
                    if let Argument::Expression(expr) = arg {
                        expr.collect_target_patterns(patterns);
                    }
                }
            }
            Self::Let(_, value, body) => {
                value.collect_target_patterns(patterns);
                body.collect_target_patterns(patterns);
            }
        }
    }

    /// Whether `function` occurs anywhere (ContainsFunctionQueryExpressionVisitor).
    pub fn contains_function(&self, names: &[Function]) -> bool {
        match self {
            Self::Target(_) | Self::Set(_) => false,
            Self::Binary(_, operands) => operands.iter().any(|e| e.contains_function(names)),
            Self::Function(function, args) => {
                names.contains(function)
                    || args.iter().any(|arg| match arg {
                        Argument::Expression(expr) => expr.contains_function(names),
                        _ => false,
                    })
            }
            Self::Let(_, value, body) => {
                value.contains_function(names) || body.contains_function(names)
            }
        }
    }

    /// QueryExpression.toTrunctatedString (QueryExpression.java:93-108):
    /// at most 1000 characters, with a `[truncated]` suffix.
    pub fn truncated(&self) -> String {
        truncate(&self.to_string())
    }
}

/// QueryExpression.truncate: Ascii.truncate(expr, 1000, "[truncated]").
pub fn truncate(text: &str) -> String {
    const MAX: usize = 1000;
    const SUFFIX: &str = "[truncated]";
    // Java counts UTF-16 code units; ASCII-dominant query text is assumed.
    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    let keep = MAX - SUFFIX.len();
    let mut result = text.chars().take(keep).collect::<String>();
    result.push_str(SUFFIX);
    result
}

/// TargetLiteral.toString (TargetLiteral.java:90-115).
fn literal(pattern: &str) -> String {
    quote_word(pattern)
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target(pattern) => f.write_str(&literal(pattern)),
            Self::Binary(operator, operands) => {
                write!(f, "({}", operands[0])?;
                for operand in &operands[1..] {
                    write!(f, " {} {operand}", operator.pretty_name())?;
                }
                f.write_str(")")
            }
            Self::Function(function, args) => {
                write!(f, "{}(", function.name())?;
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    arg.fmt(f)?;
                }
                f.write_str(")")
            }
            Self::Let(name, value, body) => write!(f, "let {name} = {value} in {body}"),
            Self::Set(words) => {
                f.write_str("set(")?;
                for (index, word) in words.iter().enumerate() {
                    if index > 0 {
                        f.write_str(" ")?;
                    }
                    f.write_str(&literal(word))?;
                }
                f.write_str(")")
            }
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

/// QueryParser.parse (QueryParser.java:68-80).
pub fn parse(query: &str) -> Result<Expr, SyntaxError> {
    let mut parser = Parser {
        tokens: scan(query)?,
        index: 0,
    };
    let expr = parser.expression()?;
    if parser.kind() != TokenKind::Eof {
        return Err(SyntaxError(format!(
            "unexpected token '{}' after query expression '{}'",
            parser.tokens[parser.index],
            expr.truncated()
        )));
    }
    Ok(expr)
}

impl Parser {
    fn kind(&self) -> TokenKind {
        self.tokens[self.index].kind
    }

    fn next(&mut self) {
        if self.kind() != TokenKind::Eof {
            self.index += 1;
        }
    }

    /// Three tokens of context starting at `index` (appendInputContext,
    /// QueryParser.java:148-157; the EOF token is excluded).
    fn context(&self, index: usize) -> String {
        let max = (self.tokens.len() - 1).min(index + 3);
        (index..max)
            .map(|i| self.tokens[i].to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// syntaxError (QueryParser.java:93-112).
    fn syntax_error(&self, index: usize) -> SyntaxError {
        if self.tokens[index].kind == TokenKind::Eof {
            return SyntaxError("premature end of input".to_owned());
        }
        SyntaxError(format!("syntax error at '{}'", self.context(index)))
    }

    fn consume(&mut self, kind: TokenKind) -> Result<Option<String>, SyntaxError> {
        if self.kind() != kind {
            return Err(self.syntax_error(self.index));
        }
        let word = self.tokens[self.index].word.clone();
        self.next();
        Ok(word)
    }

    fn consume_word(&mut self) -> Result<String, SyntaxError> {
        Ok(self.consume(TokenKind::Word)?.unwrap_or_default())
    }

    /// consumeIntLiteral (QueryParser.java:176-186): Integer.parseInt.
    fn consume_int(&mut self) -> Result<i32, SyntaxError> {
        let text = self.consume_word()?;
        parse_java_int(&text)
            .ok_or_else(|| SyntaxError(format!("expected an integer literal: '{text}'")))
    }

    fn expression(&mut self) -> Result<Expr, SyntaxError> {
        let lhs = self.primary()?;
        self.binary_tail(lhs)
    }

    /// parseBinaryOperatorTail (QueryParser.java:210-232): equal precedence,
    /// left associative, runs of one operator collected into one node.
    fn binary_tail(&mut self, lhs: Expr) -> Result<Expr, SyntaxError> {
        if !self.kind().is_binary_operator() {
            return Ok(lhs);
        }
        let mut lhs = lhs;
        let mut operands = vec![];
        let mut last = self.kind();
        while self.kind().is_binary_operator() {
            let operator = self.kind();
            self.next();
            if operator != last {
                let previous = std::mem::take(&mut operands);
                lhs = Expr::Binary(Operator::of(last), prepend(lhs, previous));
                last = operator;
            }
            let rhs = self.primary()?;
            operands.push(rhs);
        }
        Ok(Expr::Binary(Operator::of(last), prepend(lhs, operands)))
    }

    /// parsePrimary (QueryParser.java:248-338).
    fn primary(&mut self) -> Result<Expr, SyntaxError> {
        match self.kind() {
            TokenKind::Word => {
                let word_index = self.index;
                let word = self.consume_word()?;
                if self.kind() != TokenKind::LParen {
                    return validate_target_literal(word).map(Expr::Target);
                }
                let Some(function) = Function::lookup(&word) else {
                    let mut names = Function::ALL.map(Function::name).to_vec();
                    names.sort_unstable();
                    return Err(SyntaxError(format!(
                        "unknown function '{word}' at '{}'; expected one of ['{}']",
                        self.context(word_index),
                        names.join("', '")
                    )));
                };
                let mut args = Vec::new();
                let mut separator = TokenKind::LParen;
                let mut seen = 0;
                for &ty in function.argument_types() {
                    if self.kind() == TokenKind::RParen {
                        if seen >= function.mandatory_arguments() {
                            break;
                        }
                        return Err(self.argument_count_error(function, "too few"));
                    }
                    self.consume(separator)?;
                    separator = TokenKind::Comma;
                    if seen == 0 && self.kind() == TokenKind::RParen {
                        return Err(self.argument_count_error(function, "too few"));
                    }
                    args.push(match ty {
                        ArgumentType::Expression => Argument::Expression(self.expression()?),
                        ArgumentType::Word => Argument::Word(self.consume_word()?),
                        ArgumentType::Integer => Argument::Integer(self.consume_int()?),
                    });
                    seen += 1;
                }
                if self.kind() == TokenKind::Comma && seen > 0 {
                    return Err(self.argument_count_error(function, "too many"));
                }
                self.consume(TokenKind::RParen)?;
                Ok(Expr::Function(function, args))
            }
            TokenKind::Let => {
                self.next();
                let name = self.consume_word()?;
                self.consume(TokenKind::Equals)?;
                let value = self.expression()?;
                self.consume(TokenKind::In)?;
                let body = self.expression()?;
                Ok(Expr::Let(name, Box::new(value), Box::new(body)))
            }
            TokenKind::LParen => {
                self.next();
                let expr = self.expression()?;
                self.consume(TokenKind::RParen)?;
                Ok(expr)
            }
            TokenKind::Set => {
                self.next();
                self.consume(TokenKind::LParen)?;
                let mut words = Vec::new();
                while self.kind() == TokenKind::Word {
                    words.push(validate_target_literal(self.consume_word()?)?);
                }
                self.consume(TokenKind::RParen)?;
                Ok(Expr::Set(words))
            }
            _ => Err(self.syntax_error(self.index)),
        }
    }

    /// functionArgumentCountError (QueryParser.java:118-130): the context is
    /// the current token.
    fn argument_count_error(&self, function: Function, description: &str) -> SyntaxError {
        SyntaxError(format!(
            "{description} arguments to function '{}' at '{}'",
            function.name(),
            self.context(self.index)
        ))
    }
}

fn prepend(first: Expr, rest: Vec<Expr>) -> Vec<Expr> {
    let mut operands = Vec::with_capacity(rest.len() + 1);
    operands.push(first);
    operands.extend(rest);
    operands
}

/// validateTargetLiteral (QueryParser.java:344-351).
fn validate_target_literal(word: String) -> Result<String, SyntaxError> {
    if let Some(first) = word.chars().next().filter(|c| matches!(c, '-' | '*')) {
        return Err(SyntaxError(format!(
            "target literal must not begin with ({first}): {word}"
        )));
    }
    Ok(word)
}

/// Integer.parseInt: optional sign, decimal digits, 32-bit range.
fn parse_java_int(text: &str) -> Option<i32> {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<i64>()
        .ok()
        .and_then(|value| i32::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(query: &str) -> String {
        parse(query).unwrap().to_string()
    }

    fn error(query: &str) -> String {
        parse(query).unwrap_err().0
    }

    #[test]
    fn operators_are_left_associative_with_equal_precedence() {
        assert_eq!(round_trip("a + b - c"), "((a + b) - c)");
        assert_eq!(round_trip("a + b + c"), "(a + b + c)");
        assert_eq!(round_trip("a ^ b intersect c"), "((a ^ b) intersect c)");
        assert_eq!(round_trip("a union (b except c)"), "(a union (b except c))");
        assert_eq!(round_trip("a + b - c + d"), "(((a + b) - c) + d)");
    }

    #[test]
    fn functions_lets_sets_and_literals_print_like_bazel() {
        assert_eq!(round_trip("deps(//a:b, 2)"), "deps(//a:b, 2)");
        assert_eq!(
            round_trip("let x = //a in $x + kind(\"go_.*\", $x)"),
            "let x = //a in ($x + kind('go_.*', $x))"
        );
        assert_eq!(round_trip("set(//a //b:c)"), "set(//a //b:c)");
        assert_eq!(
            round_trip("attr(tags, '[\\[ ]x', //...)"),
            "attr('tags', '[\\[ ]x', //...)"
        );
        assert_eq!(round_trip("'//a b'"), "\"//a b\"");
        assert_eq!(round_trip("\"in\""), "\"in\"");
        assert_eq!(round_trip("'\"x\"'"), "'\"x\"'");
        // `+` requires quoting (TargetLiteral.java:93-98).
        assert_eq!(
            round_trip("@@rules_go+//go:def.bzl"),
            "\"@@rules_go+//go:def.bzl\""
        );
        assert_eq!(round_trip("@foo+@bar"), "(@foo + @bar)");
    }

    #[test]
    fn syntax_errors_follow_query_parser() {
        assert_eq!(error("deps("), "premature end of input");
        assert_eq!(error("deps(//a"), "premature end of input");
        assert_eq!(
            error("deps()"),
            "too few arguments to function 'deps' at ')'"
        );
        assert_eq!(
            error("deps(//a, 1, 2)"),
            "too many arguments to function 'deps' at ', 2 )'"
        );
        assert_eq!(
            error("rdeps(//a)"),
            "too few arguments to function 'rdeps' at ')'"
        );
        assert_eq!(error("deps(//a, x)"), "expected an integer literal: 'x'");
        assert_eq!(
            error("foo(//a)"),
            "unknown function 'foo' at 'foo ( //a'; expected one of ['allpaths', 'attr', \
             'buildfiles', 'deps', 'executables', 'filter', 'kind', 'labels', 'loadfiles', \
             'rdeps', 'same_pkg_direct_rdeps', 'siblings', 'some', 'somepath', 'tests', \
             'visible']"
        );
        assert_eq!(
            error("//a )"),
            "unexpected token ')' after query expression '//a'"
        );
        assert_eq!(error("'abc"), "unclosed quotation");
        assert_eq!(error("*x"), "target literal must not begin with (*): *x");
        assert_eq!(error("+ //a"), "syntax error at '+ //a'");
        assert_eq!(error("let x = //a"), "premature end of input");
    }

    #[test]
    fn collected_patterns_skip_variables_and_duplicates() {
        let mut patterns = Vec::new();
        parse("let v = //a in $v + //b + //a + set(//c $v)")
            .unwrap()
            .collect_target_patterns(&mut patterns);
        assert_eq!(patterns, ["//a", "//b", "//c"]);
    }
}
