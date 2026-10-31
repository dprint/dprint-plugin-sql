// Builds the statement and block structure from the tokens.
//
// The parser is tolerant: it understands enough to know where statements, blocks and brackets
// begin and end, and leaves everything else to the generator as a run of tokens. Brackets have
// to balance, since nothing sensible can be done otherwise, but a construct it can't make sense
// of (ex. a `CASE` without an `END`) is read as plain tokens rather than reported, because the
// dialect it's written in may just use those words differently.

use std::collections::HashSet;
use std::ops::Range;

use dprint_core::formatting::bumpalo;
use dprint_core::formatting::bumpalo::Bump;
use dprint_core::formatting::bumpalo::collections::Vec as BumpVec;

use crate::ast::*;
use crate::configuration::Dialect;
use crate::keywords;
use crate::lexer::Token;
use crate::lexer::TokenKind;

#[derive(Debug)]
pub struct SyntaxError {
  pub message: String,
  pub span: Range<usize>,
}

/// Parses the tokens into a tree, which is allocated in `bump`.
pub fn parse<'a>(tokens: &'a [Token<'a>], dialect: Dialect, bump: &'a Bump) -> Result<StatementList<'a>, SyntaxError> {
  let mut parser = Parser {
    tokens,
    bump,
    pos: 0,
    dialect,
    depth: 0,
    not_declarations: false,
    failed_constructs: HashSet::new(),
    failed_declarations: HashSet::new(),
  };
  let list = parser.parse_list(Stops::default())?;
  if let Some(token) = parser.peek() {
    // only a closing bracket stops the top level list
    return Err(parser.error(token, "unexpected closing bracket"));
  }
  Ok(list)
}

/// How deeply brackets and blocks may nest before the input is refused. Each level is a few stack
/// frames, and running out of stack aborts the process rather than raising an error.
const MAX_DEPTH: usize = 128;

/// Which words end the statement list or the statement being parsed, which depends on what
/// encloses them.
#[derive(Clone, Copy, Default)]
struct Stops {
  /// Within a `BEGIN .. END` block or another construct ended by `END`.
  end: bool,
  /// Within the body of an `IF` or a procedural `CASE`, ended by `ELSE`.
  else_: bool,
  /// Within the body of a procedural `IF`, ended by `ELSIF` or `ELSEIF`.
  elsif: bool,
  /// Within an exception handler or a branch of a procedural `CASE`, ended by the next `WHEN`.
  when: bool,
  /// Within a block that may have an `EXCEPTION` section.
  exception: bool,
  /// Within the body of a `REPEAT` loop, ended by `UNTIL`.
  until: bool,
  /// Within a declaration section, ended by `BEGIN`.
  begin: bool,
  /// Within a declaration section, where `PROCEDURE` and `FUNCTION` start a routine, and which a
  /// `DECLARE` ends, since one is never within another.
  declarations: bool,
}

struct Parser<'a> {
  tokens: &'a [Token<'a>],
  /// Where the tree is allocated.
  bump: &'a Bump,
  pos: usize,
  dialect: Dialect,
  depth: usize,
  /// Set when a list of declarations reaches something that isn't one.
  not_declarations: bool,
  /// Where a block, loop or other construct was tried and turned out not to be one (ex. a `BEGIN`
  /// without an `END`), which is the same wherever it's tried from. Trying again would read the
  /// rest of the input again, for every construct that encloses it.
  failed_constructs: HashSet<usize>,
  /// Where a section of declarations was tried and turned out not to be one, for the same reason,
  /// whether what's there aren't declarations or they aren't followed by a block.
  failed_declarations: HashSet<usize>,
}

impl<'a> Parser<'a> {
  // ---- statement lists ----

  fn parse_list(&mut self, stops: Stops) -> Result<StatementList<'a>, SyntaxError> {
    let mut items = BumpVec::new_in(self.bump);
    let mut ignore_next = false;
    while let Some(token) = self.peek() {
      if self.is_list_stop(token, stops) || is_closing(token) {
        break;
      }
      let blank_line_before = !items.is_empty() && token.newlines_before >= 2;
      let kind = match token.kind {
        // a comment that's a statement of its own (ex. MySQL's `/*!40101 SET NAMES utf8 */;`)
        TokenKind::BlockComment
          if self.tokens.get(self.pos + 1).is_some_and(|next| {
            matches!(next.kind, TokenKind::Semicolon | TokenKind::Delimiter) && next.newlines_before == 0
          }) =>
        {
          self.pos += 1;
          ItemKind::Statement(self.finish_statement(bumpalo::vec![in self.bump; Element::Token(token)], None))
        }
        TokenKind::LineComment | TokenKind::BlockComment | TokenKind::TemplateLine => {
          self.pos += 1;
          if dprint_core::formatting::ir_helpers::text_has_dprint_ignore(token.text, "dprint-ignore") {
            ignore_next = true;
          }
          ItemKind::Comment(token)
        }
        TokenKind::BatchSeparator | TokenKind::Directive => {
          self.pos += 1;
          ignore_next = false;
          ItemKind::Command(token, self.take_same_line_comments())
        }
        _ => {
          if stops.declarations && !self.looks_like_declaration(self.pos) {
            self.not_declarations = true;
            break;
          }
          let mut statement = self.parse_statement(stops)?;
          statement.ignored = std::mem::take(&mut ignore_next);
          if statement.elements.is_empty() && statement.terminator.is_none() {
            // nothing could be parsed here, which only a stop within a statement causes
            break;
          }
          ItemKind::Statement(statement)
        }
      };
      items.push(Item {
        kind,
        blank_line_before,
      });
    }
    Ok(StatementList {
      items: items.into_bump_slice(),
    })
  }

  fn is_list_stop(&self, token: &Token, stops: Stops) -> bool {
    token.kind == TokenKind::Word
      && ((stops.end && token.is_word("END"))
        || (stops.else_ && token.is_word("ELSE"))
        || (stops.elsif && (token.is_word("ELSIF") || token.is_word("ELSEIF")))
        || (stops.when && token.is_word("WHEN"))
        || (stops.exception && token.is_word("EXCEPTION"))
        || (stops.until && token.is_word("UNTIL"))
        || (stops.begin && token.is_word("BEGIN"))
        || (stops.declarations && token.is_word("DECLARE")))
  }

  // ---- statements ----

  fn parse_statement(&mut self, stops: Stops) -> Result<Statement<'a>, SyntaxError> {
    let mut elements = BumpVec::new_in(self.bump);
    // comments before a statement that isn't in a list, such as the body of a T-SQL `IF`
    while let Some(token) = self.peek().filter(|token| token.is_comment()) {
      elements.push(Element::Token(token));
      self.pos += 1;
    }

    if let Some(label) = self.parse_label() {
      let standalone = !label.own_line
        && self
          .peek()
          .is_none_or(|token| token.starts_line() || !self.is_labelable(token));
      elements.push(Element::Label(self.bump.alloc(label)));
      if standalone {
        // a T-SQL label for `GOTO`, which is a statement of its own
        return Ok(self.finish_statement(elements, None));
      }
    }

    if let Some(element) = self.parse_statement_construct(stops)? {
      elements.push(element);
      return Ok(self.finish_statement(elements, None));
    }

    let mut state = StatementState::default();
    if stops.declarations {
      state.declarations = true;
    }
    if let Some(section) = self.parse_declare_section_at_start()? {
      let element = Element::DeclareSection(self.bump.alloc(section));
      state.update(&element);
      elements.push(element);
    }

    let mut terminator = None;
    while let Some(token) = self.peek() {
      if is_terminator(token) {
        terminator = Some(token);
        self.pos += 1;
        break;
      }
      match token.kind {
        TokenKind::BatchSeparator | TokenKind::Directive => break,
        _ if is_closing(token) => break,
        _ => {}
      }
      if token.is_comment() {
        // a comment at the end of a line belongs to what's before it, but one on a line of its
        // own belongs to whatever follows it, which may be the next statement
        if token.newlines_before > 0 && !elements.is_empty() {
          let next = self.peek_past_comments();
          if next.is_none_or(|next| self.ends_statement(next, stops, &state, true)) {
            break;
          }
        }
        elements.push(Element::Token(token));
        self.pos += 1;
        continue;
      }
      if !elements.is_empty() && self.ends_statement(token, stops, &state, false) {
        break;
      }
      let element = self.parse_element(&state)?;
      let ends = matches!(
        element,
        Element::Block(_) | Element::If(_) | Element::Conditional(_) | Element::Loop(_) | Element::CaseStatement(_)
      );
      state.update(&element);
      let is_routine_body_start = state.routine
        && element
          .as_token()
          .is_some_and(|token| token.is_word("IS") || token.is_word("AS"));
      elements.push(element);
      if ends {
        break;
      }
      if is_routine_body_start && let Some(section) = self.parse_routine_declarations()? {
        let element = Element::DeclareSection(self.bump.alloc(section));
        state.update(&element);
        elements.push(element);
      }
    }
    Ok(self.finish_statement(elements, terminator))
  }

  fn finish_statement(&mut self, elements: BumpVec<'a, Element<'a>>, mut terminator: Option<Tok<'a>>) -> Statement<'a> {
    if terminator.is_none()
      && let Some(token) = self.peek().filter(|token| is_terminator(token))
    {
      terminator = Some(token);
      self.pos += 1;
    }
    let trailing_comments = if terminator.is_some() || elements.last().is_some_and(|element| element.is_procedural()) {
      self.take_same_line_comments()
    } else {
      &[]
    };
    Statement {
      elements: elements.into_bump_slice(),
      terminator,
      trailing_comments,
      ignored: false,
    }
  }

  fn take_same_line_comments(&mut self) -> &'a [Tok<'a>] {
    let mut comments = BumpVec::new_in(self.bump);
    while let Some(token) = self
      .peek()
      .filter(|token| token.is_comment() && token.newlines_before == 0)
    {
      comments.push(token);
      self.pos += 1;
    }
    comments.into_bump_slice()
  }

  /// Whether `token` ends the statement being parsed rather than continuing it.
  fn ends_statement(&self, token: &Token, stops: Stops, state: &StatementState, is_after_comment: bool) -> bool {
    match token.kind {
      // the terminator ends the statement the comment before it is in
      TokenKind::Semicolon | TokenKind::Delimiter => return false,
      TokenKind::BatchSeparator | TokenKind::Directive => return true,
      _ if is_closing(token) => return true,
      _ => {}
    }
    if token.kind != TokenKind::Word {
      return false;
    }
    if (stops.end && token.is_word("END"))
      || (stops.else_ && token.is_word("ELSE"))
      || (stops.elsif && (token.is_word("ELSIF") || token.is_word("ELSEIF")))
    {
      return true;
    }
    if is_after_comment && self.is_list_stop(token, stops) {
      return true;
    }
    self.allows_statements_without_semicolons() && self.starts_new_statement(token, state)
  }

  /// T-SQL doesn't need a `;` between statements, so a statement there may also end where the
  /// next one begins.
  fn allows_statements_without_semicolons(&self) -> bool {
    matches!(self.dialect, Dialect::SqlServer | Dialect::Generic)
  }

  /// Whether `token`, a word, starts a new statement in place of continuing the one before it,
  /// which is only asked in the dialects that don't need a `;` between them.
  fn starts_new_statement(&self, token: &Token, state: &StatementState) -> bool {
    // the generic dialect only splits where a new line starts, since a mistake there is more
    // likely, and the formatter writes everything that does start a line on one of its own
    if self.dialect == Dialect::Generic && !token.starts_line() {
      return false;
    }
    let next = self.tokens.get(self.pos + 1);
    let next_is_call = next.is_some_and(|next| next.kind == TokenKind::OpenParen && !next.space_before);
    let next_is = |word: &str| next.is_some_and(|next| next.is_word(word));
    let prev = state.prev_word.unwrap_or("");
    let prev_is = |words: &[&str]| words.iter().any(|word| word.eq_ignore_ascii_case(prev));
    let head = state.head.unwrap_or("");
    let head_is = |words: &[&str]| words.iter().any(|word| word.eq_ignore_ascii_case(head));
    if state.prev_is_comma_or_operator {
      return false;
    }
    // the privileges of a `GRANT` are words that start statements (ex. `GRANT SELECT, UPDATE`)
    if head_is(&["GRANT", "REVOKE", "DENY"]) && (!token.starts_line() || prev_is(&["GRANT", "REVOKE", "DENY"])) {
      return false;
    }
    let word = token.text;
    let is = |candidate: &str| word.eq_ignore_ascii_case(candidate);

    // the body of a T-SQL routine without a `BEGIN` is the rest of the batch, so the statements
    // in it are written as statements of their own
    if state.routine
      && prev_is(&["AS"])
      && !is("BEGIN")
      && !is("RETURN")
      && !is("EXTERNAL")
      && keywords::contains(keywords::TSQL_STATEMENT_KEYWORDS, word)
    {
      return true;
    }

    if is("SELECT") || (is("WITH") && self.is_cte_start(self.pos)) {
      // PostgreSQL's `ON CONFLICT .. DO SELECT`
      if prev_is(&["UNION", "ALL", "DISTINCT", "INTERSECT", "EXCEPT", "MINUS", "DO"]) {
        return false;
      }
      if state.query_started {
        return true;
      }
      // a T-SQL `RETURN` that returns nothing (ex. `IF @x IS NULL RETURN`), unlike the one of
      // `AS RETURN SELECT ..`
      if head_is(&["RETURN"]) && prev_is(&["RETURN"]) && token.starts_line() {
        return true;
      }
      if prev_is(&[
        "AS", "FOR", "IN", "QUERY", "EXPLAIN", "ANALYZE", "VERBOSE", "IS", "RETURN",
      ]) {
        return false;
      }
      // a `CREATE` or `ALTER` only has a query after its `AS`, which the check above covers
      return !head_is(&["INSERT", "REPLACE", "WITH", "EXPLAIN", "DESCRIBE", "DESC", "UPSERT"]);
    }
    if is("INSERT") || is("UPDATE") || is("DELETE") || is("MERGE") || is("REPLACE") {
      // a join hint (ex. `LEFT MERGE JOIN`)
      if next_is_call || next_is("JOIN") {
        return false;
      }
      if prev_is(&[
        "ON", "FOR", "KEY", "DO", "THEN", "BEFORE", "AFTER", "OF", "OR", "GRANT", "REVOKE", "DENY", "INSTEAD", "NO",
        "ALSO",
      ]) {
        return false;
      }
      // the body of a trigger without a `BEGIN` (ex. `FOR EACH ROW INSERT ..`), or an action of
      // an `ALTER` (ex. ClickHouse's `ALTER TABLE t UPDATE a = 1`)
      if (head_is(&["CREATE"]) && state.routine) || head_is(&["ALTER"]) {
        return false;
      }
      return !(head_is(&["WITH"]) && state.main_dml.is_none());
    }
    if is("SET") {
      if head_is(&["ALTER", "CREATE", "REPLACE", "GRANT", "REVOKE"]) || state.handler {
        return false;
      }
      if prev_is(&["CHARACTER", "CHAR", "DELETE", "UPDATE", "NULL", "DEFAULT"]) {
        return false;
      }
      // an `UPDATE` has one `SET`, a `MERGE` has one after each of its `UPDATE`s, and a MySQL
      // `INSERT INTO t SET a = 1` has one before any values, so a `SET` after them starts the
      // next statement (ex. `UPDATE t SET a = 1 SET @done = 1`)
      let dml_is = |word: &str| state.main_dml.is_some_and(|dml| dml.eq_ignore_ascii_case(word));
      if dml_is("UPDATE") {
        return state.update_set_seen;
      }
      if dml_is("INSERT") {
        return state.query_started;
      }
      return !(head_is(&["WITH"]) && state.main_dml.is_none());
    }
    if is("EXEC") || is("EXECUTE") {
      if prev_is(&["WITH", "GRANT", "REVOKE", "DENY", "AS"]) || head_is(&["CREATE", "ALTER"]) {
        return false;
      }
      return !head_is(&["INSERT"]) || state.query_started;
    }
    if is("IF") {
      return !next_is_call && !keywords::contains(keywords::OBJECT_KINDS, prev) && !prev_is(&["EXISTS", "NOT"]);
    }
    if is("BEGIN") {
      return !state.allows_block() && !head_is(&["CREATE", "ALTER"]);
    }
    if is("RETURN") {
      // `RETURN` is the body of a SQL function in PostgreSQL and Databricks
      return !prev_is(&["AS"]) && !state.routine;
    }
    if is("CREATE") || is("ALTER") || is("DROP") {
      if prev_is(&["GRANT", "REVOKE", "DENY", "WITH", "ON", "OR"]) {
        return false;
      }
      if head_is(&["ALTER"]) && (is("ALTER") || is("DROP")) {
        // `ALTER TABLE t DROP COLUMN c` and `ALTER TABLE t ALTER COLUMN c ..` continue it, while
        // a statement altering or dropping something else doesn't
        return next.is_some_and(|next| {
          next.kind == TokenKind::Word
            && keywords::contains(keywords::OBJECT_KINDS, next.text)
            && !next.is_word("COLUMN")
            && !next.is_word("CONSTRAINT")
            && !next.is_word("INDEX")
            && !next.is_word("PARTITION")
            && !next.is_word("KEY")
        });
      }
      return !next_is_call;
    }
    if is("FETCH") {
      return !(state.query_started || prev_is(&["ROW", "ROWS", "ONLY"]));
    }
    if is("USE") {
      return !(next_is("INDEX") || next_is("KEY") || next_is("NONE"));
    }
    if is("COMMIT") || is("ROLLBACK") {
      return !prev_is(&["ON", "WITH", "PRESERVE", "DELETE"]);
    }
    if is("GRANT") || is("REVOKE") || is("DENY") {
      return !prev_is(&["WITH"]);
    }
    if is("WHILE")
      || is("DECLARE")
      || is("PRINT")
      || is("RAISERROR")
      || is("THROW")
      || is("WAITFOR")
      || is("GOTO")
      || is("DEALLOCATE")
      || is("TRUNCATE")
      || is("OPEN")
      || is("CLOSE")
      || is("BREAK")
      || is("CONTINUE")
      || is("SAVE")
      || is("DBCC")
      || is("BACKUP")
      || is("RESTORE")
      || is("CHECKPOINT")
      || is("KILL")
      || is("RECONFIGURE")
    {
      // an action of an `ALTER TABLE` (ex. MySQL's `TRUNCATE PARTITION`)
      if head_is(&["ALTER"]) {
        return false;
      }
      return !next_is_call && !prev_is(&["GRANT", "REVOKE", "DENY"]);
    }
    false
  }

  /// Whether the `WITH` at `index` begins common table expressions (`WITH name AS (` or
  /// `WITH RECURSIVE`) rather than a table hint or an option.
  fn is_cte_start(&self, index: usize) -> bool {
    let next = |offset: usize| {
      self.tokens[index + 1..]
        .iter()
        .filter(|token| !token.is_comment())
        .nth(offset)
    };
    match next(0) {
      Some(token) if token.is_word("RECURSIVE") => true,
      Some(token) if matches!(token.kind, TokenKind::Word | TokenKind::QuotedIdent) => match next(1) {
        // not an option of a view or a routine (ex. `WITH SCHEMABINDING AS SELECT ..`)
        Some(token) if token.is_word("AS") => next(2).is_some_and(|token| {
          token.kind == TokenKind::OpenParen || token.is_word("MATERIALIZED") || token.is_word("NOT")
        }),
        // `WITH name (columns) AS (`
        Some(token) if token.kind == TokenKind::OpenParen => {
          let mut depth = 0;
          for (i, token) in self.tokens[index + 1..].iter().enumerate() {
            match token.kind {
              TokenKind::OpenParen => depth += 1,
              TokenKind::CloseParen => {
                depth -= 1;
                if depth == 0 {
                  return self
                    .tokens
                    .get(index + 1 + i + 1)
                    .is_some_and(|token| token.is_word("AS"));
                }
              }
              _ => {}
            }
          }
          false
        }
        _ => false,
      },
      _ => false,
    }
  }

  // ---- statement level constructs ----

  /// Parses procedural code that makes up a whole statement (ex. a block or an `IF`), if that's
  /// what starts here.
  fn parse_statement_construct(&mut self, stops: Stops) -> Result<Option<Element<'a>>, SyntaxError> {
    let Some(token) = self.peek() else {
      return Ok(None);
    };
    if token.kind != TokenKind::Word {
      return Ok(None);
    }
    let start = self.pos;
    if self.failed_constructs.contains(&start) {
      return Ok(None);
    }
    let result = if token.is_word("BEGIN") && self.is_block_begin(self.pos) {
      self.parse_block()?.map(|block| Element::Block(self.bump.alloc(block)))
    } else if token.is_word("IF") {
      if self.dialect != Dialect::SqlServer && self.has_procedural_keyword_ahead(&["THEN"]) {
        self.parse_if()?.map(|block| Element::If(self.bump.alloc(block)))
      } else if self.allows_statements_without_semicolons() {
        self
          .parse_conditional(stops)?
          .map(|conditional| Element::Conditional(self.bump.alloc(conditional)))
      } else {
        None
      }
    } else if token.is_word("WHILE") {
      if self.has_procedural_keyword_ahead(&["LOOP", "DO"]) {
        self.parse_loop()?.map(|block| Element::Loop(self.bump.alloc(block)))
      } else if self.allows_statements_without_semicolons() {
        self
          .parse_conditional(stops)?
          .map(|conditional| Element::Conditional(self.bump.alloc(conditional)))
      } else {
        None
      }
    } else if (token.is_word("FOR") || token.is_word("FOREACH"))
      && self.dialect != Dialect::SqlServer
      && self.has_procedural_keyword_ahead(&["LOOP", "DO"])
    {
      self.parse_loop()?.map(|block| Element::Loop(self.bump.alloc(block)))
    } else if (token.is_word("LOOP") || token.is_word("REPEAT")) && self.dialect != Dialect::SqlServer {
      self.parse_loop()?.map(|block| Element::Loop(self.bump.alloc(block)))
    } else if token.is_word("CASE") && self.dialect != Dialect::SqlServer {
      self
        .parse_case_statement()?
        .map(|statement| Element::CaseStatement(self.bump.alloc(statement)))
    } else {
      None
    };
    if result.is_none() {
      self.pos = start;
      self.failed_constructs.insert(start);
    }
    Ok(result)
  }

  /// Whether the `BEGIN` at `index` starts a block rather than a transaction.
  fn is_block_begin(&self, index: usize) -> bool {
    match self.tokens.get(index + 1) {
      None => false,
      Some(next) => {
        // a transaction's `BEGIN` may also be ended by a command to the client (ex. psql's `\g`)
        !matches!(
          next.kind,
          TokenKind::Semicolon | TokenKind::Delimiter | TokenKind::BatchSeparator | TokenKind::Directive
        ) && !is_terminator(next)
          && ![
            "TRAN",
            "TRANSACTION",
            "WORK",
            "ISOLATION",
            "READ",
            "DEFERRED",
            "IMMEDIATE",
            "EXCLUSIVE",
            "DISTRIBUTED",
            "DIALOG",
            "CONVERSATION",
            "NAME",
          ]
          .iter()
          .any(|word| next.is_word(word))
      }
    }
  }

  fn parse_block(&mut self) -> Result<Option<Block<'a>>, SyntaxError> {
    self.enter()?;
    let mut begin = bumpalo::vec![in self.bump; self.advance()];
    while let Some(token) = self.peek().filter(|token| {
      !token.starts_line()
        && (token.is_word("TRY") || token.is_word("CATCH") || token.is_word("ATOMIC") || token.is_word("NOT"))
    }) {
      begin.push(token);
      self.pos += 1;
    }
    let has_exception_section = !matches!(self.dialect, Dialect::SqlServer | Dialect::MySql | Dialect::Sqlite);
    let body = self.parse_list(Stops {
      end: true,
      exception: has_exception_section,
      ..Default::default()
    })?;
    let exception = if has_exception_section && self.peek().is_some_and(|token| token.is_word("EXCEPTION")) {
      Some(self.parse_exception_section()?)
    } else {
      None
    };
    let Some(end) = self.peek().filter(|token| token.is_word("END")) else {
      self.leave();
      return Ok(None);
    };
    self.pos += 1;
    let mut end = bumpalo::vec![in self.bump; end];
    if let Some(suffix) = self.peek().filter(|token| {
      token.newlines_before == 0
        && (token.is_word("TRY")
          || token.is_word("CATCH")
          || (matches!(token.kind, TokenKind::Word | TokenKind::QuotedIdent)
            && !keywords::contains(keywords::RESERVED, token.text)))
    }) {
      end.push(suffix);
      self.pos += 1;
    }
    self.leave();
    Ok(Some(Block {
      begin: begin.into_bump_slice(),
      body,
      exception,
      end: end.into_bump_slice(),
    }))
  }

  fn parse_exception_section(&mut self) -> Result<ExceptionSection<'a>, SyntaxError> {
    let keyword = self.advance();
    let leading_comments = self.take_comments();
    let mut handlers = BumpVec::new_in(self.bump);
    while self.peek().is_some_and(|token| token.is_word("WHEN")) {
      let Some(handler) = self.parse_handler(Stops {
        when: true,
        end: true,
        ..Default::default()
      })?
      else {
        break;
      };
      handlers.push(handler);
    }
    Ok(ExceptionSection {
      keyword,
      leading_comments,
      handlers: handlers.into_bump_slice(),
    })
  }

  /// `WHEN condition THEN statements`, starting at the `WHEN`.
  fn parse_handler(&mut self, body_stops: Stops) -> Result<Option<Handler<'a>>, SyntaxError> {
    let start = self.pos;
    let when = self.advance();
    let Some(condition) = self.parse_elements_until(&["THEN"])? else {
      self.pos = start;
      return Ok(None);
    };
    let then = self.advance();
    let body = self.parse_list(body_stops)?;
    Ok(Some(Handler {
      when,
      condition: condition.into_bump_slice(),
      then,
      body,
    }))
  }

  fn parse_if(&mut self) -> Result<Option<IfBlock<'a>>, SyntaxError> {
    self.enter()?;
    let body_stops = Stops {
      end: true,
      else_: true,
      elsif: true,
      ..Default::default()
    };
    let mut branches = BumpVec::new_in(self.bump);
    loop {
      let keyword = self.advance();
      let Some(condition) = self.parse_elements_until(&["THEN"])? else {
        self.leave();
        return Ok(None);
      };
      let then = self.advance();
      let body = self.parse_list(body_stops)?;
      branches.push(IfBranch {
        keyword,
        condition: condition.into_bump_slice(),
        then,
        body,
      });
      if !self
        .peek()
        .is_some_and(|token| token.is_word("ELSIF") || token.is_word("ELSEIF"))
      {
        break;
      }
    }
    let else_branch = if self.peek().is_some_and(|token| token.is_word("ELSE")) {
      let keyword = self.advance();
      let body = self.parse_list(Stops {
        end: true,
        ..Default::default()
      })?;
      Some(ElseBranch { keyword, body })
    } else {
      None
    };
    let end = self.parse_end(&["IF"]);
    self.leave();
    Ok(end.map(|end| IfBlock {
      branches: branches.into_bump_slice(),
      else_branch,
      end,
    }))
  }

  /// T-SQL's `IF condition statement [ELSE statement]` or `WHILE condition statement`.
  fn parse_conditional(&mut self, stops: Stops) -> Result<Option<Conditional<'a>>, SyntaxError> {
    self.enter()?;
    let keyword = self.advance();
    let is_if = keyword.is_word("IF");
    let mut condition = BumpVec::new_in(self.bump);
    loop {
      let Some(token) = self.peek() else {
        self.leave();
        return Ok(None);
      };
      if matches!(
        token.kind,
        TokenKind::Semicolon | TokenKind::Delimiter | TokenKind::BatchSeparator | TokenKind::Directive
      ) || is_closing(token)
      {
        self.leave();
        return Ok(None);
      }
      if !condition.is_empty()
        && token.kind == TokenKind::Word
        && keywords::contains(keywords::TSQL_STATEMENT_KEYWORDS, token.text)
      {
        let is_call = self
          .tokens
          .get(self.pos + 1)
          .is_some_and(|next| next.kind == TokenKind::OpenParen && !next.space_before);
        if !is_call {
          break;
        }
      }
      condition.push(self.parse_element(&StatementState::default())?);
    }
    let body = self.parse_statement(Stops { else_: is_if, ..stops })?;
    let mut else_comments: &'a [Tok<'a>] = &[];
    let mut else_branch = None;
    if is_if {
      let comments_start = self.pos;
      let comments = self.take_comments();
      if self.peek().is_some_and(|token| token.is_word("ELSE")) {
        let else_keyword = self.advance();
        else_comments = comments;
        let statement = self.parse_statement(Stops { else_: true, ..stops })?;
        else_branch = Some((else_keyword, &*self.bump.alloc(statement)));
      } else {
        self.pos = comments_start;
      }
    }
    self.leave();
    Ok(Some(Conditional {
      keyword,
      condition: condition.into_bump_slice(),
      body: self.bump.alloc(body),
      else_comments,
      else_branch,
    }))
  }

  fn parse_loop(&mut self) -> Result<Option<Loop<'a>>, SyntaxError> {
    self.enter()?;
    let first = self.peek().unwrap();
    let is_repeat = first.is_word("REPEAT");
    let header = if first.is_word("LOOP") || is_repeat {
      bumpalo::vec![in self.bump; Element::Token(self.advance())]
    } else {
      let Some(mut header) = self.parse_elements_until(&["LOOP", "DO"])? else {
        self.leave();
        return Ok(None);
      };
      header.push(Element::Token(self.advance()));
      header
    };
    let body = self.parse_list(Stops {
      end: true,
      until: is_repeat,
      ..Default::default()
    })?;
    let until = if is_repeat {
      if !self.peek().is_some_and(|token| token.is_word("UNTIL")) {
        self.leave();
        return Ok(None);
      }
      let keyword = self.advance();
      let Some(condition) = self.parse_elements_until(&["END"])? else {
        self.leave();
        return Ok(None);
      };
      Some((keyword, condition.into_bump_slice()))
    } else {
      None
    };
    let end = self.parse_end(&["LOOP", "WHILE", "FOR", "REPEAT", "FOREACH"]);
    self.leave();
    Ok(end.map(|end| Loop {
      header: header.into_bump_slice(),
      body,
      until,
      end,
    }))
  }

  fn parse_case_statement(&mut self) -> Result<Option<CaseStatement<'a>>, SyntaxError> {
    self.enter()?;
    let case_keyword = self.advance();
    let Some(operand) = self.parse_elements_until(&["WHEN"])? else {
      self.leave();
      return Ok(None);
    };
    let mut branches = BumpVec::new_in(self.bump);
    while self.peek().is_some_and(|token| token.is_word("WHEN")) {
      let Some(branch) = self.parse_handler(Stops {
        when: true,
        else_: true,
        end: true,
        ..Default::default()
      })?
      else {
        self.leave();
        return Ok(None);
      };
      branches.push(branch);
    }
    let else_branch = if self.peek().is_some_and(|token| token.is_word("ELSE")) {
      let keyword = self.advance();
      let body = self.parse_list(Stops {
        end: true,
        ..Default::default()
      })?;
      Some(ElseBranch { keyword, body })
    } else {
      None
    };
    let end = self.parse_end(&["CASE"]);
    self.leave();
    Ok(end.filter(|_| !branches.is_empty()).map(|end| CaseStatement {
      case_keyword,
      operand: operand.into_bump_slice(),
      branches: branches.into_bump_slice(),
      else_branch,
      end,
    }))
  }

  /// `END` followed by one of `kinds` and optionally a label, or `None` when that isn't what's
  /// next.
  fn parse_end(&mut self, kinds: &[&str]) -> Option<&'a [Tok<'a>]> {
    let end = self.peek().filter(|token| token.is_word("END"))?;
    let kind = self
      .tokens
      .get(self.pos + 1)
      .filter(|token| kinds.iter().any(|kind| token.is_word(kind)))?;
    self.pos += 2;
    let mut tokens = bumpalo::vec![in self.bump; end, kind];
    if let Some(label) = self.peek().filter(|token| {
      token.newlines_before == 0
        && matches!(token.kind, TokenKind::Word | TokenKind::QuotedIdent)
        && !keywords::contains(keywords::RESERVED, token.text)
    }) {
      tokens.push(label);
      self.pos += 1;
    }
    Some(tokens.into_bump_slice())
  }

  /// A label before a block or loop: `<<name>>`, or `name:` in MySQL and BigQuery.
  fn parse_label(&mut self) -> Option<Label<'a>> {
    let first = self.peek()?;
    if first.is_operator("<<") {
      let name = self.tokens.get(self.pos + 1)?;
      let close = self.tokens.get(self.pos + 2)?;
      if matches!(name.kind, TokenKind::Word | TokenKind::QuotedIdent) && close.is_operator(">>") {
        self.pos += 3;
        return Some(Label {
          tokens: self.bump.alloc_slice_copy(&[first, name, close]),
          own_line: true,
        });
      }
      return None;
    }
    if matches!(first.kind, TokenKind::Word | TokenKind::QuotedIdent)
      && !keywords::contains(keywords::RESERVED, first.text)
    {
      let colon = self.tokens.get(self.pos + 1)?;
      if colon.is_operator(":") && !colon.space_before {
        let after = self.tokens.get(self.pos + 2);
        let is_label = after.is_none_or(|after| after.starts_line() || self.is_labelable(after));
        if is_label {
          self.pos += 2;
          return Some(Label {
            tokens: self.bump.alloc_slice_copy(&[first, colon]),
            own_line: false,
          });
        }
      }
    }
    None
  }

  fn is_labelable(&self, token: &Token) -> bool {
    ["BEGIN", "LOOP", "WHILE", "REPEAT", "FOR"]
      .iter()
      .any(|word| token.is_word(word))
  }

  // ---- declarations ----

  /// A `DECLARE` that starts a section of declarations before a block's `BEGIN`, as opposed to a
  /// statement declaring a variable (ex. T-SQL's `DECLARE @x INT`).
  fn parse_declare_section_at_start(&mut self) -> Result<Option<DeclareSection<'a>>, SyntaxError> {
    let Some(token) = self.peek().filter(|token| token.is_word("DECLARE")) else {
      return Ok(None);
    };
    let tentative = match self.dialect {
      Dialect::SqlServer | Dialect::MySql | Dialect::BigQuery | Dialect::Sqlite => return Ok(None),
      Dialect::Oracle | Dialect::Snowflake => false,
      Dialect::PostgreSql => false,
      Dialect::Generic => true,
    };
    if !self.starts_declarations(self.pos + 1) {
      return Ok(None);
    }
    let start = self.pos;
    self.pos += 1;
    let Some(declarations) = self.parse_declarations()? else {
      self.pos = start;
      return Ok(None);
    };
    let followed_by_block = self.peek().is_some_and(|token| token.is_word("BEGIN"));
    if !followed_by_block || (tentative && declarations.items.is_empty()) {
      if !followed_by_block {
        self.failed_declarations.insert(start + 1);
      }
      self.pos = start;
      return Ok(None);
    }
    Ok(Some(DeclareSection {
      keyword: Some(token),
      declarations,
    }))
  }

  /// Whether the tokens at `index` are a declaration in a declaration section rather than the
  /// rest of a `DECLARE` statement.
  fn starts_declarations(&self, index: usize) -> bool {
    let Some(first) = self.tokens.get(index) else {
      return false;
    };
    if first.is_word("BEGIN") {
      // an empty section
      return true;
    }
    if !matches!(first.kind, TokenKind::Word | TokenKind::QuotedIdent) || first.kind == TokenKind::Variable {
      return false;
    }
    // a PostgreSQL cursor (`DECLARE name [BINARY] [INSENSITIVE] [NO] [SCROLL] CURSOR`)
    let is_cursor = self.tokens[index + 1..]
      .iter()
      .take_while(|token| token.kind == TokenKind::Word)
      .take(5)
      .take_while(|token| {
        ["BINARY", "INSENSITIVE", "ASENSITIVE", "NO", "SCROLL", "CURSOR"]
          .iter()
          .any(|w| token.is_word(w))
      })
      .any(|token| token.is_word("CURSOR"));
    if is_cursor && self.dialect != Dialect::Oracle {
      return false;
    }
    // a MySQL handler or condition (`DECLARE CONTINUE HANDLER ..`)
    if ["CONTINUE", "EXIT", "UNDO"].iter().any(|w| first.is_word(w)) {
      return false;
    }
    true
  }

  /// The declarations of a declaration section, up to the `BEGIN` or `END` after them, or `None`
  /// when something other than a declaration comes first, in which case it wasn't a declaration
  /// section after all.
  ///
  /// Each declaration is checked as it's reached rather than all of them once the `BEGIN` is, which
  /// keeps a mistaken guess cheap: otherwise the rest of the file would be read as declarations to
  /// find out, again for every later statement that looked like the start of a section.
  fn parse_declarations(&mut self) -> Result<Option<StatementList<'a>>, SyntaxError> {
    let start = self.pos;
    if self.failed_declarations.contains(&start) {
      return Ok(None);
    }
    self.enter()?;
    let outer = std::mem::replace(&mut self.not_declarations, false);
    let list = self.parse_list(Stops {
      begin: true,
      end: true,
      declarations: true,
      ..Default::default()
    })?;
    let failed = std::mem::replace(&mut self.not_declarations, outer);
    if failed {
      self.failed_declarations.insert(start);
    }
    self.leave();
    Ok(if failed { None } else { Some(list) })
  }

  /// The declarations after the `IS` or `AS` of an Oracle routine, if that's what follows.
  fn parse_routine_declarations(&mut self) -> Result<Option<DeclareSection<'a>>, SyntaxError> {
    let Some(next) = self.peek() else {
      return Ok(None);
    };
    let is_keyword = self.tokens[self.pos - 1].is_word("IS");
    let follows = match self.dialect {
      Dialect::Oracle => {
        !next.is_word("BEGIN")
          && !next.is_word("LANGUAGE")
          && !next.is_word("EXTERNAL")
          && matches!(next.kind, TokenKind::Word | TokenKind::QuotedIdent)
      }
      Dialect::Generic => {
        !next.is_word("BEGIN")
          && (if is_keyword {
            matches!(next.kind, TokenKind::Word | TokenKind::QuotedIdent)
              && !next.is_word("LANGUAGE")
              && !next.is_word("EXTERNAL")
          } else {
            self.looks_like_declaration(self.pos)
          })
      }
      _ => false,
    };
    if !follows {
      return Ok(None);
    }
    let start = self.pos;
    let declarations = self.parse_declarations()?;
    // the declarations are followed by the routine's block, or the end of a package's
    // specification, and without either they weren't declarations after all
    let Some(declarations) = declarations.filter(|_| {
      self
        .peek()
        .is_some_and(|token| token.is_word("BEGIN") || token.is_word("END"))
    }) else {
      self.failed_declarations.insert(start);
      self.pos = start;
      return Ok(None);
    };
    Ok(Some(DeclareSection {
      keyword: None,
      declarations,
    }))
  }

  /// Whether the tokens at `index` look like an Oracle declaration (`name type`).
  fn looks_like_declaration(&self, index: usize) -> bool {
    let Some(first) = self.tokens.get(index) else {
      return false;
    };
    if ["CURSOR", "TYPE", "SUBTYPE", "PRAGMA", "PROCEDURE", "FUNCTION"]
      .iter()
      .any(|word| first.is_word(word))
    {
      return true;
    }
    let is_name = (first.kind == TokenKind::Word && !keywords::contains(keywords::RESERVED, first.text))
      || first.kind == TokenKind::QuotedIdent;
    is_name
      && self
        .tokens
        .get(index + 1)
        .is_some_and(|token| token.kind == TokenKind::Word)
  }

  // ---- elements ----

  fn parse_element(&mut self, state: &StatementState) -> Result<Element<'a>, SyntaxError> {
    let token = self.peek().unwrap();
    match token.kind {
      TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => {
        Ok(Element::Group(self.bump.alloc(self.parse_group()?)))
      }
      TokenKind::Word if token.is_word("CASE") && !self.failed_constructs.contains(&self.pos) => {
        let start = self.pos;
        match self.parse_case()? {
          Some(case) => Ok(Element::Case(self.bump.alloc(case))),
          None => {
            self.failed_constructs.insert(start);
            self.pos = start + 1;
            Ok(Element::Token(token))
          }
        }
      }
      TokenKind::Word
        if token.is_word("BEGIN")
          && state.allows_block()
          && self.is_block_begin(self.pos)
          && !self.failed_constructs.contains(&self.pos) =>
      {
        let start = self.pos;
        match self.parse_block()? {
          Some(block) => Ok(Element::Block(self.bump.alloc(block))),
          None => {
            self.failed_constructs.insert(start);
            self.pos = start + 1;
            Ok(Element::Token(token))
          }
        }
      }
      TokenKind::Word
        if token.is_word("DECLARE")
          && state.routine
          && matches!(
            self.dialect,
            Dialect::Oracle | Dialect::Generic | Dialect::PostgreSql | Dialect::Snowflake
          )
          && self.starts_declarations(self.pos + 1)
          && !self.failed_declarations.contains(&(self.pos + 1)) =>
      {
        let start = self.pos;
        self.pos += 1;
        let declarations = self.parse_declarations()?;
        let Some(declarations) = declarations.filter(|_| self.peek().is_some_and(|token| token.is_word("BEGIN")))
        else {
          self.failed_declarations.insert(start + 1);
          self.pos = start + 1;
          return Ok(Element::Token(token));
        };
        Ok(Element::DeclareSection(self.bump.alloc(DeclareSection {
          keyword: Some(token),
          declarations,
        })))
      }
      _ => {
        self.pos += 1;
        Ok(Element::Token(token))
      }
    }
  }

  fn parse_group(&mut self) -> Result<Group<'a>, SyntaxError> {
    self.enter()?;
    let open = self.advance();
    let close_kind = match open.kind {
      TokenKind::OpenParen => TokenKind::CloseParen,
      TokenKind::OpenBracket => TokenKind::CloseBracket,
      _ => TokenKind::CloseBrace,
    };
    let mut elements = BumpVec::new_in(self.bump);
    loop {
      let Some(token) = self.peek() else {
        return Err(self.error(open, &format!("unclosed '{}'", open.text)));
      };
      if token.kind == close_kind {
        break;
      }
      if is_closing(token) {
        return Err(self.error(
          token,
          &format!("expected '{}' to close the '{}'", closing_text(close_kind), open.text),
        ));
      }
      elements.push(self.parse_element(&StatementState::default())?);
    }
    let close = self.advance();
    self.leave();
    Ok(Group {
      open,
      elements: elements.into_bump_slice(),
      close,
    })
  }

  fn parse_case(&mut self) -> Result<Option<Case<'a>>, SyntaxError> {
    self.enter()?;
    let case_keyword = self.advance();
    let Some(mut operand) = self.parse_elements_until(&["WHEN"])? else {
      self.leave();
      return Ok(None);
    };
    let mut branches = BumpVec::new_in(self.bump);
    let mut pending_comments = take_trailing_own_line_comments(&mut operand);
    while self.peek().is_some_and(|token| token.is_word("WHEN")) {
      let when = self.advance();
      let Some(condition) = self.parse_elements_until(&["THEN"])? else {
        self.leave();
        return Ok(None);
      };
      let then = self.advance();
      let Some(mut result) = self.parse_elements_until(&["WHEN", "ELSE", "END"])? else {
        self.leave();
        return Ok(None);
      };
      let next_comments = take_trailing_own_line_comments(&mut result);
      branches.push(CaseBranch {
        leading_comments: std::mem::replace(&mut pending_comments, next_comments),
        when,
        condition: condition.into_bump_slice(),
        then,
        result: result.into_bump_slice(),
      });
    }
    if branches.is_empty() {
      self.leave();
      return Ok(None);
    }
    let else_branch = if self.peek().is_some_and(|token| token.is_word("ELSE")) {
      let keyword = self.advance();
      let Some(mut result) = self.parse_elements_until(&["END"])? else {
        self.leave();
        return Ok(None);
      };
      let next_comments = take_trailing_own_line_comments(&mut result);
      Some(CaseElse {
        leading_comments: std::mem::replace(&mut pending_comments, next_comments),
        keyword,
        result: result.into_bump_slice(),
      })
    } else {
      None
    };
    let Some(end) = self.peek().filter(|token| token.is_word("END")) else {
      self.leave();
      return Ok(None);
    };
    self.pos += 1;
    self.leave();
    Ok(Some(Case {
      case_keyword,
      operand: operand.into_bump_slice(),
      branches: branches.into_bump_slice(),
      else_branch,
      end_comments: pending_comments,
      end,
    }))
  }

  /// The elements up to one of the words `stops`, which is left as the next token, or `None` when
  /// the statement or the enclosing brackets end first.
  fn parse_elements_until(&mut self, stops: &[&str]) -> Result<Option<BumpVec<'a, Element<'a>>>, SyntaxError> {
    let mut elements = BumpVec::new_in(self.bump);
    loop {
      let Some(token) = self.peek() else {
        return Ok(None);
      };
      if stops.iter().any(|stop| token.is_word(stop)) {
        return Ok(Some(elements));
      }
      if matches!(
        token.kind,
        TokenKind::Semicolon | TokenKind::Delimiter | TokenKind::BatchSeparator | TokenKind::Directive
      ) || is_closing(token)
      {
        return Ok(None);
      }
      elements.push(self.parse_element(&StatementState::default())?);
    }
  }

  // ---- helpers ----

  /// Whether a procedural keyword (ex. the `THEN` of an `IF`) comes before the end of the
  /// statement, outside of any brackets or `CASE` expression.
  fn has_procedural_keyword_ahead(&self, words: &[&str]) -> bool {
    let mut depth = 0usize;
    let mut case_depth = 0usize;
    for token in &self.tokens[self.pos + 1..] {
      match token.kind {
        TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => depth += 1,
        TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
          if depth == 0 {
            return false;
          }
          depth -= 1;
        }
        TokenKind::Semicolon | TokenKind::Delimiter | TokenKind::BatchSeparator if depth == 0 => return false,
        TokenKind::Word if depth == 0 => {
          if token.is_word("CASE") {
            case_depth += 1;
          } else if token.is_word("END") && case_depth > 0 {
            case_depth -= 1;
          } else if case_depth == 0 {
            if words.iter().any(|word| token.is_word(word)) {
              return true;
            }
            if token.is_word("BEGIN") {
              return false;
            }
          }
        }
        _ => {}
      }
    }
    false
  }

  fn take_comments(&mut self) -> &'a [Tok<'a>] {
    let mut comments = BumpVec::new_in(self.bump);
    while let Some(token) = self.peek().filter(|token| token.is_comment()) {
      comments.push(token);
      self.pos += 1;
    }
    comments.into_bump_slice()
  }

  fn peek(&self) -> Option<Tok<'a>> {
    self.tokens.get(self.pos)
  }

  fn peek_past_comments(&self) -> Option<Tok<'a>> {
    self.tokens[self.pos..].iter().find(|token| !token.is_comment())
  }

  fn advance(&mut self) -> Tok<'a> {
    let token = &self.tokens[self.pos];
    self.pos += 1;
    token
  }

  fn enter(&mut self) -> Result<(), SyntaxError> {
    self.depth += 1;
    if self.depth > MAX_DEPTH {
      let token = self.peek().unwrap_or(&self.tokens[self.tokens.len() - 1]);
      return Err(self.error(token, "nested too deeply"));
    }
    Ok(())
  }

  fn leave(&mut self) {
    self.depth -= 1;
  }

  fn error(&self, token: &Token, message: &str) -> SyntaxError {
    SyntaxError {
      message: message.to_string(),
      span: token.start..token.end(),
    }
  }
}

/// What the statement being parsed looks like so far, which decides whether a word that could
/// start a statement continues this one instead.
#[derive(Default)]
struct StatementState<'a> {
  /// The statement's first word.
  head: Option<&'a str>,
  word_count: usize,
  /// Whether the statement creates a procedure, function, trigger or package.
  routine: bool,
  /// Whether the statement declares a MySQL handler, whose body is a statement.
  handler: bool,
  /// Whether a `SELECT` or `VALUES` has been seen.
  query_started: bool,
  /// The first of `SELECT`, `INSERT`, `UPDATE`, `DELETE` or `MERGE`.
  main_dml: Option<&'a str>,
  prev_word: Option<&'a str>,
  prev_is_comma_or_operator: bool,
  /// Whether the statement is within a declaration section.
  declarations: bool,
  /// Whether the last element was a declaration section, which a block follows.
  after_declarations: bool,
  /// Whether the kind of object a `CREATE` or `ALTER` is of has been seen.
  object_kind_seen: bool,
  /// Whether the `SET` of an `UPDATE` has been seen.
  update_set_seen: bool,
}

impl<'a> StatementState<'a> {
  fn update(&mut self, element: &Element<'a>) {
    self.after_declarations = matches!(element, Element::DeclareSection(_));
    let Some(token) = element.as_token() else {
      self.prev_word = None;
      self.prev_is_comma_or_operator = false;
      if self.head.is_none() {
        self.head = Some("");
      }
      return;
    };
    if token.is_comment() {
      return;
    }
    self.prev_is_comma_or_operator = matches!(token.kind, TokenKind::Comma | TokenKind::Operator | TokenKind::Dot);
    if token.kind != TokenKind::Word {
      self.prev_word = None;
      if self.head.is_none() {
        self.head = Some("");
      }
      return;
    }
    let word = token.text;
    if self.head.is_none() {
      self.head = Some(word);
    }
    self.word_count += 1;
    // a routine in an Oracle package or type body (ex. `MEMBER FUNCTION f RETURN NUMBER IS ..`)
    if self.declarations && self.word_count <= 3 && (token.is_word("PROCEDURE") || token.is_word("FUNCTION")) {
      self.routine = true;
    }
    let head_is_create = self
      .head
      .is_some_and(|head| head.eq_ignore_ascii_case("CREATE") || head.eq_ignore_ascii_case("ALTER"));
    // the kind is the first word naming one (ex. not the `event` of `CREATE VIEW v AS SELECT event`)
    if head_is_create
      && self.word_count <= 8
      && !self.object_kind_seen
      && let Some(is_routine) = keywords::object_kind_is_routine(word)
    {
      self.object_kind_seen = true;
      self.routine |= is_routine;
    }
    if token.is_word("HANDLER") {
      self.handler = true;
    }
    if token.is_word("SELECT") || token.is_word("VALUES") {
      self.query_started = true;
    }
    if token.is_word("SET") && self.main_dml.is_some_and(|dml| dml.eq_ignore_ascii_case("UPDATE")) {
      self.update_set_seen = true;
    }
    if self.main_dml.is_none()
      && ["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE"]
        .iter()
        .any(|dml| token.is_word(dml))
    {
      self.main_dml = Some(word);
    }
    self.prev_word = Some(word);
  }

  /// Whether a `BEGIN` in the middle of the statement starts a block.
  fn allows_block(&self) -> bool {
    self.routine
      || self.handler
      || self.after_declarations
      || self.prev_word.is_some_and(|word| {
        ["AS", "IS", "DO", "THEN", "ELSE"]
          .iter()
          .any(|candidate| word.eq_ignore_ascii_case(candidate))
      })
  }
}

/// Whether the token ends a statement: a `;`, a terminator set by a MySQL `DELIMITER` command, or
/// psql's `\;`, which ends a statement without sending it until the next `;`.
fn is_terminator(token: &Token) -> bool {
  matches!(token.kind, TokenKind::Semicolon | TokenKind::Delimiter)
    || (token.kind == TokenKind::Other && token.text == "\\;")
}

fn is_closing(token: &Token) -> bool {
  matches!(
    token.kind,
    TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace
  )
}

fn closing_text(kind: TokenKind) -> &'static str {
  match kind {
    TokenKind::CloseParen => ")",
    TokenKind::CloseBracket => "]",
    _ => "}",
  }
}

/// Removes the comments at the end of `elements` that are on lines of their own, which introduce
/// whatever comes next rather than ending what's before them.
fn take_trailing_own_line_comments<'a>(elements: &mut BumpVec<'a, Element<'a>>) -> &'a [Tok<'a>] {
  let mut split = elements.len();
  while split > 0 {
    match elements[split - 1].as_token() {
      Some(token) if token.is_comment() && token.newlines_before > 0 => split -= 1,
      _ => break,
    }
  }
  let comments = BumpVec::from_iter_in(
    elements[split..].iter().filter_map(|element| element.as_token()),
    elements.bump(),
  );
  elements.truncate(split);
  comments.into_bump_slice()
}
