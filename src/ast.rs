// AST types for the SQL parser.
//
// The tree only records the structure that decides where lines break: the statements, the blocks
// of procedural code they contain, and the brackets and `CASE` expressions within them. What
// happens between those (the clauses of a query, the items of a list, the operands of an
// operator) is worked out from the tokens when the tree is generated, since that's where the
// dialects differ the most and a mistake should cost no more than a less pleasing layout.
//
// Every token in the tree is borrowed from the lexer's output, and every token of the input is in
// the tree exactly once, in its original order. Comments are tokens like any other, apart from the
// ones on lines of their own between statements, which are items of the statement list.

use crate::lexer::Token;
use crate::lexer::TokenKind;

pub type Tok<'a> = &'a Token<'a>;

/// A list of statements: the file, or the body of a block.
#[derive(Debug, Default)]
pub struct StatementList<'a> {
  pub items: Vec<Item<'a>>,
}

#[derive(Debug)]
pub struct Item<'a> {
  pub kind: ItemKind<'a>,
  /// Whether a blank line separates the item from the one before it.
  pub blank_line_before: bool,
}

#[derive(Debug)]
pub enum ItemKind<'a> {
  Statement(Statement<'a>),
  /// A comment on a line of its own.
  Comment(Tok<'a>),
  /// A batch separator (`GO`) or a command to the client (`DELIMITER //`), along with any
  /// comments after it on its line.
  Command(Tok<'a>, Vec<Tok<'a>>),
}

impl<'a> Item<'a> {
  pub fn first_token(&self) -> Option<Tok<'a>> {
    match &self.kind {
      ItemKind::Statement(statement) => statement.first_token(),
      ItemKind::Comment(token) | ItemKind::Command(token, _) => Some(token),
    }
  }

  pub fn last_token(&self) -> Option<Tok<'a>> {
    match &self.kind {
      ItemKind::Statement(statement) => statement.last_token(),
      ItemKind::Comment(token) => Some(token),
      ItemKind::Command(token, comments) => Some(comments.last().copied().unwrap_or(token)),
    }
  }
}

#[derive(Debug)]
pub struct Statement<'a> {
  pub elements: Vec<Element<'a>>,
  /// The `;` (or `DELIMITER` set terminator) that ends the statement, if any.
  pub terminator: Option<Tok<'a>>,
  /// Comments on the same line as the end of the statement.
  pub trailing_comments: Vec<Tok<'a>>,
  /// Whether a `dprint-ignore` comment precedes the statement, so it's written as it is.
  pub ignored: bool,
}

impl<'a> Statement<'a> {
  pub fn first_token(&self) -> Option<Tok<'a>> {
    self
      .elements
      .first()
      .map(|element| element.first_token())
      .or(self.terminator)
  }

  pub fn last_token(&self) -> Option<Tok<'a>> {
    self
      .trailing_comments
      .last()
      .copied()
      .or(self.terminator)
      .or_else(|| self.elements.last().map(|element| element.last_token()))
  }
}

#[derive(Debug)]
pub enum Element<'a> {
  Token(Tok<'a>),
  /// Brackets of any kind (`( )`, `[ ]` or `{ }`) and what's within them.
  Group(Box<Group<'a>>),
  Case(Box<Case<'a>>),
  /// `BEGIN .. END`, including T-SQL's `BEGIN TRY .. END TRY` and `BEGIN CATCH .. END CATCH`.
  Block(Box<Block<'a>>),
  /// The declarations before a block's `BEGIN` (ex. after `DECLARE`, or after the `IS` of an
  /// Oracle procedure).
  DeclareSection(Box<DeclareSection<'a>>),
  /// `IF cond THEN .. END IF`.
  If(Box<IfBlock<'a>>),
  /// T-SQL's `IF cond statement ELSE statement` and `WHILE cond statement`.
  Conditional(Box<Conditional<'a>>),
  /// `LOOP`, `WHILE`, `FOR`, `FOREACH` and `REPEAT` loops with a body ended by `END ..`.
  Loop(Box<Loop<'a>>),
  /// A procedural `CASE .. END CASE` whose branches hold statements.
  CaseStatement(Box<CaseStatement<'a>>),
  /// A label for a block or loop (`<<name>>` or `name:`).
  Label(Box<Label<'a>>),
}

impl<'a> Element<'a> {
  pub fn first_token(&self) -> Tok<'a> {
    match self {
      Element::Token(token) => token,
      Element::Group(group) => group.open,
      Element::Case(case) => case.case_keyword,
      Element::Block(block) => block.begin[0],
      Element::DeclareSection(section) => section.first_token(),
      Element::If(block) => block.branches[0].keyword,
      Element::Conditional(conditional) => conditional.keyword,
      Element::Loop(block) => block.header[0].first_token(),
      Element::CaseStatement(statement) => statement.case_keyword,
      Element::Label(label) => label.tokens[0],
    }
  }

  pub fn last_token(&self) -> Tok<'a> {
    match self {
      Element::Token(token) => token,
      Element::Group(group) => group.close,
      Element::Case(case) => case.end,
      Element::Block(block) => block.end.last().unwrap(),
      Element::DeclareSection(section) => section.last_token(),
      Element::If(block) => block.end.last().unwrap(),
      Element::Conditional(conditional) => conditional.last_token(),
      Element::Loop(block) => block.end.last().unwrap(),
      Element::CaseStatement(statement) => statement.end.last().unwrap(),
      Element::Label(label) => label.tokens.last().unwrap(),
    }
  }

  pub fn as_token(&self) -> Option<Tok<'a>> {
    match self {
      Element::Token(token) => Some(token),
      _ => None,
    }
  }

  pub fn is_word(&self, word: &str) -> bool {
    self.as_token().is_some_and(|token| token.is_word(word))
  }

  pub fn is_kind(&self, kind: TokenKind) -> bool {
    self.as_token().is_some_and(|token| token.kind == kind)
  }

  pub fn is_comment(&self) -> bool {
    self.as_token().is_some_and(|token| token.is_comment())
  }

  pub fn is_line_comment(&self) -> bool {
    self.as_token().is_some_and(|token| token.is_line_comment())
  }

  /// Whether the element is procedural code that is written over lines of its own.
  pub fn is_procedural(&self) -> bool {
    matches!(
      self,
      Element::Block(_)
        | Element::DeclareSection(_)
        | Element::If(_)
        | Element::Conditional(_)
        | Element::Loop(_)
        | Element::CaseStatement(_)
    )
  }
}

#[derive(Debug)]
pub struct Group<'a> {
  pub open: Tok<'a>,
  pub elements: Vec<Element<'a>>,
  pub close: Tok<'a>,
}

impl Group<'_> {
  pub fn is_paren(&self) -> bool {
    self.open.kind == TokenKind::OpenParen
  }
}

/// `CASE [operand] WHEN .. THEN .. [ELSE ..] END`.
#[derive(Debug)]
pub struct Case<'a> {
  pub case_keyword: Tok<'a>,
  pub operand: Vec<Element<'a>>,
  pub branches: Vec<CaseBranch<'a>>,
  pub else_branch: Option<CaseElse<'a>>,
  /// Comments on lines of their own before the `END`.
  pub end_comments: Vec<Tok<'a>>,
  pub end: Tok<'a>,
}

#[derive(Debug)]
pub struct CaseBranch<'a> {
  /// Comments on lines of their own before the `WHEN`.
  pub leading_comments: Vec<Tok<'a>>,
  pub when: Tok<'a>,
  pub condition: Vec<Element<'a>>,
  pub then: Tok<'a>,
  pub result: Vec<Element<'a>>,
}

#[derive(Debug)]
pub struct CaseElse<'a> {
  pub leading_comments: Vec<Tok<'a>>,
  pub keyword: Tok<'a>,
  pub result: Vec<Element<'a>>,
}

#[derive(Debug)]
pub struct Block<'a> {
  /// `BEGIN` along with any words after it that say what kind of block it is (ex. `TRY`).
  pub begin: Vec<Tok<'a>>,
  pub body: StatementList<'a>,
  pub exception: Option<ExceptionSection<'a>>,
  /// `END` along with any words after it (ex. `TRY` or the name of the procedure).
  pub end: Vec<Tok<'a>>,
}

#[derive(Debug)]
pub struct ExceptionSection<'a> {
  pub keyword: Tok<'a>,
  /// Comments before the first handler.
  pub leading_comments: Vec<Tok<'a>>,
  pub handlers: Vec<Handler<'a>>,
}

/// `WHEN condition THEN statements` in a list of exception handlers or a procedural `CASE`.
#[derive(Debug)]
pub struct Handler<'a> {
  pub when: Tok<'a>,
  pub condition: Vec<Element<'a>>,
  pub then: Tok<'a>,
  pub body: StatementList<'a>,
}

#[derive(Debug)]
pub struct DeclareSection<'a> {
  pub keyword: Option<Tok<'a>>,
  pub declarations: StatementList<'a>,
}

impl<'a> DeclareSection<'a> {
  pub fn first_token(&self) -> Tok<'a> {
    self
      .keyword
      .or_else(|| self.declarations.items.first().and_then(|item| item.first_token()))
      .unwrap()
  }

  pub fn last_token(&self) -> Tok<'a> {
    self
      .declarations
      .items
      .last()
      .and_then(|item| item.last_token())
      .or(self.keyword)
      .unwrap()
  }
}

#[derive(Debug)]
pub struct IfBlock<'a> {
  pub branches: Vec<IfBranch<'a>>,
  pub else_branch: Option<ElseBranch<'a>>,
  /// `END IF`.
  pub end: Vec<Tok<'a>>,
}

#[derive(Debug)]
pub struct IfBranch<'a> {
  /// `IF`, `ELSIF` or `ELSEIF`.
  pub keyword: Tok<'a>,
  pub condition: Vec<Element<'a>>,
  pub then: Tok<'a>,
  pub body: StatementList<'a>,
}

#[derive(Debug)]
pub struct ElseBranch<'a> {
  pub keyword: Tok<'a>,
  pub body: StatementList<'a>,
}

/// T-SQL's `IF condition statement [ELSE statement]` or `WHILE condition statement`, where each
/// statement may be a block.
#[derive(Debug)]
pub struct Conditional<'a> {
  pub keyword: Tok<'a>,
  pub condition: Vec<Element<'a>>,
  pub body: Box<Statement<'a>>,
  /// Comments on lines of their own between the body and the `ELSE`.
  pub else_comments: Vec<Tok<'a>>,
  pub else_branch: Option<(Tok<'a>, Box<Statement<'a>>)>,
}

impl<'a> Conditional<'a> {
  pub fn last_token(&self) -> Tok<'a> {
    match &self.else_branch {
      Some((keyword, statement)) => statement.last_token().unwrap_or(keyword),
      None => self.body.last_token().unwrap_or(self.keyword),
    }
  }
}

#[derive(Debug)]
pub struct Loop<'a> {
  /// What comes before the body: `WHILE condition` or `FOR .. IN ..`, followed by the `LOOP` or
  /// `DO`. Just `LOOP` or `REPEAT` for loops without a header.
  pub header: Vec<Element<'a>>,
  pub body: StatementList<'a>,
  /// The `UNTIL` of a `REPEAT` loop and its condition.
  pub until: Option<(Tok<'a>, Vec<Element<'a>>)>,
  /// `END LOOP`, `END WHILE`, `END FOR` or `END REPEAT`, followed by any label.
  pub end: Vec<Tok<'a>>,
}

#[derive(Debug)]
pub struct CaseStatement<'a> {
  pub case_keyword: Tok<'a>,
  pub operand: Vec<Element<'a>>,
  pub branches: Vec<Handler<'a>>,
  pub else_branch: Option<ElseBranch<'a>>,
  /// `END CASE`.
  pub end: Vec<Tok<'a>>,
}

#[derive(Debug)]
pub struct Label<'a> {
  pub tokens: Vec<Tok<'a>>,
  /// Whether the label is written on a line of its own (ex. `<<name>>`) rather than before what
  /// it labels (ex. `name: LOOP`).
  pub own_line: bool,
}
