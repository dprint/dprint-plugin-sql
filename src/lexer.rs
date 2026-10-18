// Splits SQL text into tokens.
//
// The lexer is lossless: every character of the input belongs to a token or to the whitespace
// between two of them, so the formatter only ever decides what whitespace goes between tokens and
// never what the tokens are. Where the dialects disagree on what some text is (ex. `#` starts a
// comment in MySQL, a temporary table's name in SQL Server and is an operator in PostgreSQL), the
// configured dialect decides, and the generic one picks whatever is least likely to be wrong.

use std::ops::Range;

use crate::configuration::Dialect;
use crate::keywords;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
  /// An unquoted identifier or keyword, including a T-SQL temporary table's name (`#temp`).
  Word,
  /// A quoted identifier (`"name"`, `` `name` `` or `[name]`).
  QuotedIdent,
  /// A string literal, including any prefix (`N'..'`, `E'..'`, `X'..'`, `q'[..]'`).
  String,
  /// A dollar-quoted string (`$$..$$` or `$tag$..$tag$`).
  DollarString,
  Number,
  /// A variable or parameter (`@name`, `@@name`, `:name`, `$1`, `?`, `${name}`).
  Variable,
  Operator,
  Comma,
  Semicolon,
  Dot,
  OpenParen,
  CloseParen,
  OpenBracket,
  CloseBracket,
  OpenBrace,
  CloseBrace,
  /// A comment that runs to the end of the line (`-- ..`, `# ..` or `// ..`).
  LineComment,
  /// A `/* .. */` comment.
  BlockComment,
  /// A template expression or tag (`{{ .. }}`, `{% .. %}` or `{# .. #}`).
  Template,
  /// A template tag or comment on a line of its own (ex. `{% if x %}`), which is treated like a
  /// line comment: it keeps its line, and whatever comes after it starts the next one.
  TemplateLine,
  /// A statement terminator other than `;` set by a MySQL `DELIMITER` command.
  Delimiter,
  /// Separates batches of statements: `GO` in SQL Server and `/` in Oracle, on a line of its own.
  BatchSeparator,
  /// A command to a client rather than SQL, which takes up its whole line (ex. `DELIMITER //`, a
  /// psql `\connect` or an `sqlcmd` `:setvar`).
  Directive,
  /// A character that is none of the above.
  Other,
}

#[derive(Debug, Clone)]
pub struct Token<'a> {
  pub kind: TokenKind,
  pub text: &'a str,
  /// Where the token starts in the text.
  pub start: usize,
  /// How many line breaks are in the whitespace before the token.
  pub newlines_before: u32,
  /// Whether any whitespace comes before the token.
  pub space_before: bool,
}

impl Token<'_> {
  pub fn end(&self) -> usize {
    self.start + self.text.len()
  }

  /// Whether this is the word `word`, ignoring case. `word` is expected in uppercase.
  pub fn is_word(&self, word: &str) -> bool {
    self.kind == TokenKind::Word && self.text.eq_ignore_ascii_case(word)
  }

  pub fn is_comment(&self) -> bool {
    matches!(
      self.kind,
      TokenKind::LineComment | TokenKind::BlockComment | TokenKind::TemplateLine
    )
  }

  /// Whether this token ends its line however the text after it is written.
  pub fn is_line_comment(&self) -> bool {
    matches!(self.kind, TokenKind::LineComment | TokenKind::TemplateLine)
  }

  pub fn is_operator(&self, op: &str) -> bool {
    self.kind == TokenKind::Operator && self.text == op
  }

  /// Whether the token is the first thing on its line.
  pub fn starts_line(&self) -> bool {
    self.newlines_before > 0
  }

  /// Whether the token is written over more than one line.
  pub fn is_multi_line(&self) -> bool {
    self.text.contains('\n')
  }
}

#[derive(Debug)]
pub struct LexError {
  pub message: String,
  pub span: Range<usize>,
}

pub fn lex(text: &str, dialect: Dialect) -> Result<Vec<Token<'_>>, LexError> {
  let mut lexer = Lexer {
    text,
    pos: 0,
    dialect,
    delimiter: None,
    tokens: Vec::new(),
    newlines: 0,
    space: false,
    line_start: true,
    open_brackets: 0,
    last_significant: None,
  };
  lexer.run()?;
  Ok(lexer.tokens)
}

struct Lexer<'a> {
  text: &'a str,
  pos: usize,
  dialect: Dialect,
  /// The statement terminator set by a MySQL `DELIMITER` command, when it isn't `;`.
  delimiter: Option<&'a str>,
  tokens: Vec<Token<'a>>,
  newlines: u32,
  space: bool,
  /// Whether only whitespace precedes the current position on its line.
  line_start: bool,
  /// How many `[` that aren't quoted identifiers are open.
  open_brackets: i32,
  /// The kind of the last token that isn't a comment.
  last_significant: Option<TokenKind>,
}

impl<'a> Lexer<'a> {
  fn run(&mut self) -> Result<(), LexError> {
    loop {
      self.skip_whitespace();
      if self.pos >= self.text.len() {
        return Ok(());
      }
      let start = self.pos;
      let kind = self.next_kind()?;
      self.push(kind, start);
      if kind == TokenKind::Semicolon {
        for _ in 0..self.copy_from_stdin_count() {
          if !self.copy_data() {
            break;
          }
        }
      }
    }
  }

  /// How many of the statements just ended are psql's `COPY .. FROM STDIN`, each of which the
  /// data to copy follows on the lines after it, up to a line holding only `\.`. psql sends the
  /// statements separated by `\;` together, so there may be more than one.
  fn copy_from_stdin_count(&self) -> usize {
    if !matches!(self.dialect, Dialect::PostgreSql | Dialect::Generic) {
      return 0;
    }
    let statement_start = self.tokens[..self.tokens.len() - 1]
      .iter()
      .rposition(|token| {
        matches!(
          token.kind,
          TokenKind::Semicolon | TokenKind::Delimiter | TokenKind::BatchSeparator | TokenKind::Directive
        )
      })
      .map(|index| index + 1)
      .unwrap_or(0);
    let statement = &self.tokens[statement_start..self.tokens.len() - 1];
    statement
      .split(|token| token.kind == TokenKind::Other && token.text == "\\;")
      .filter(|part| is_copy_from_stdin(part))
      .count()
  }

  /// The data after a `COPY .. FROM STDIN`, which is kept as it is. Returns whether there was any.
  fn copy_data(&mut self) -> bool {
    let Some(line_end) = self.rest().find('\n') else {
      return false;
    };
    // anything after the `;` on its line is still SQL, so the data starts on the next line, but a
    // comment there can't be data
    let rest_of_line = self.rest()[..line_end].trim_matches(is_space);
    if !rest_of_line.is_empty() && !rest_of_line.starts_with("--") {
      return false;
    }
    if !rest_of_line.is_empty() {
      self.skip_whitespace();
      let start = self.pos;
      let kind = self.line_comment();
      self.push(kind, start);
      return self.copy_data();
    }
    let data_start = self.pos + line_end + 1;
    let mut end = self.text.len();
    let mut line_start = data_start;
    while line_start < self.text.len() {
      let line_end = self.text[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(self.text.len());
      if self.text[line_start..line_end].trim_end_matches(is_space) == "\\." {
        end = line_start + self.text[line_start..line_end].trim_end_matches(is_space).len();
        break;
      }
      line_start = line_end + 1;
    }
    if end <= data_start {
      return false;
    }
    self.pos = end;
    self.newlines = 1;
    self.push(TokenKind::Directive, data_start);
    true
  }

  fn push(&mut self, kind: TokenKind, start: usize) {
    match kind {
      TokenKind::OpenBracket => self.open_brackets += 1,
      TokenKind::CloseBracket => self.open_brackets -= 1,
      _ => {}
    }
    if !matches!(
      kind,
      TokenKind::LineComment | TokenKind::BlockComment | TokenKind::TemplateLine
    ) {
      self.last_significant = Some(kind);
    }
    self.tokens.push(Token {
      kind,
      text: &self.text[start..self.pos],
      start,
      newlines_before: self.newlines,
      space_before: self.space || self.newlines > 0,
    });
    self.newlines = 0;
    self.space = false;
    self.line_start = false;
  }

  fn skip_whitespace(&mut self) {
    while let Some(c) = self.peek() {
      match c {
        '\n' => {
          self.newlines += 1;
          self.line_start = true;
        }
        ' ' | '\t' | '\r' | '\x0b' | '\x0c' => self.space = true,
        _ => return,
      }
      self.pos += c.len_utf8();
    }
  }

  fn next_kind(&mut self) -> Result<TokenKind, LexError> {
    if self.line_start
      && let Some(kind) = self.line_command()
    {
      return Ok(kind);
    }
    if self.is_go_after_semicolon() {
      self.pos += 2;
      return Ok(TokenKind::BatchSeparator);
    }
    if let Some(delimiter) = self.delimiter
      && self.rest().starts_with(delimiter)
    {
      self.pos += delimiter.len();
      return Ok(TokenKind::Delimiter);
    }

    let c = self.peek().unwrap();
    let next = self.peek_at(1);
    match c {
      '-' if next == Some('-') && self.is_double_dash_comment() => Ok(self.line_comment()),
      '/' if next == Some('*') => self.block_comment(),
      '/' if next == Some('/') && self.dialect == Dialect::Snowflake => Ok(self.line_comment()),
      '#' => Ok(self.hash()),
      '\'' => self.string(1, self.backslash_escapes()),
      '"' => self.double_quoted(),
      '`' => self.quoted('`', TokenKind::QuotedIdent, self.dialect == Dialect::BigQuery),
      '[' => self.open_bracket(),
      ']' => Ok(self.single(TokenKind::CloseBracket)),
      '{' => self.open_brace(),
      '}' => Ok(self.single(TokenKind::CloseBrace)),
      '(' => Ok(self.single(TokenKind::OpenParen)),
      ')' => Ok(self.single(TokenKind::CloseParen)),
      ',' => Ok(self.single(TokenKind::Comma)),
      ';' => Ok(self.single(TokenKind::Semicolon)),
      '.' => Ok(self.dot()),
      '@' => Ok(self.at()),
      ':' => Ok(self.colon()),
      '?' => Ok(self.question_mark()),
      '$' => self.dollar(),
      '%' if self.is_python_placeholder() => Ok(TokenKind::Variable),
      '&' if self.dialect == Dialect::Oracle && next.is_some_and(|c| c == '&' || is_ident_start(c)) => {
        // SQL*Plus substitution variable
        self.pos += if next == Some('&') { 2 } else { 1 };
        self.eat_ident_chars();
        Ok(TokenKind::Variable)
      }
      // psql's `\;`, which separates statements sent to the server together rather than ending one
      '\\' if next == Some(';') && matches!(self.dialect, Dialect::PostgreSql | Dialect::Generic) => {
        self.pos += 2;
        Ok(TokenKind::Other)
      }
      // a psql meta-command, which runs to the end of its line wherever it starts, and ends the
      // query before it (ex. `SELECT 1 \gset`)
      '\\'
        if next.is_some_and(|c| c.is_ascii_alphabetic())
          && matches!(self.dialect, Dialect::PostgreSql | Dialect::Generic) =>
      {
        self.pos += self.rest_of_line().trim_end_matches(is_space).len();
        Ok(TokenKind::Directive)
      }
      '\\' => Ok(self.single(TokenKind::Other)),
      c if c.is_ascii_digit() => Ok(self.number()),
      c if is_ident_start(c) => self.word(),
      c if is_operator_char(c) => Ok(self.operator()),
      _ => Ok(self.single(TokenKind::Other)),
    }
  }

  // ---- line commands ----

  /// A command to the client, which takes up a line of its own.
  fn line_command(&mut self) -> Option<TokenKind> {
    let line = self.rest_of_line();
    let trimmed = line.trim_end_matches(is_space);
    let first_word_len = trimmed.find(is_space).unwrap_or(trimmed.len());
    let first_word = &trimmed[..first_word_len];
    let after_first = trimmed[first_word_len..].trim_start_matches(is_space);

    if first_word.eq_ignore_ascii_case("delimiter")
      && matches!(self.dialect, Dialect::MySql | Dialect::Generic)
      && self.is_between_statements()
      && let Some(delimiter) = self.delimiter_command_value(after_first)
    {
      self.pos += trimmed.len();
      self.delimiter = if delimiter == ";" { None } else { Some(delimiter) };
      return Some(TokenKind::Directive);
    }

    if matches!(self.dialect, Dialect::SqlServer | Dialect::Generic) && first_word.eq_ignore_ascii_case("go") {
      // `GO` or `GO 5`, optionally followed by a comment
      let rest = after_first;
      let count_len = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
      let after_count = rest[count_len..].trim_start_matches(is_space);
      if after_count.is_empty() || after_count.starts_with("--") {
        let len = trimmed.len() - after_count.len();
        self.pos += line[..len].trim_end_matches(is_space).len();
        return Some(TokenKind::BatchSeparator);
      }
    }

    if matches!(self.dialect, Dialect::Oracle | Dialect::Generic) && trimmed == "/" {
      self.pos += 1;
      return Some(TokenKind::BatchSeparator);
    }

    // a client's commands are only between statements, since a line of a statement may start the
    // same way (ex. a T-SQL parameter `@name` or an Oracle `SET` clause)
    let is_directive = self.is_between_statements()
      && match self.dialect {
        Dialect::SqlServer => is_sqlcmd_command(trimmed),
        Dialect::Oracle => is_sql_plus_command(first_word, after_first),
        Dialect::Generic => is_sqlcmd_command(trimmed),
        _ => false,
      };
    if is_directive {
      self.pos += trimmed.len();
      // a SQL*Plus command continues onto the next line after a ` -` (ex. `EXEC p( -`), though
      // a remark or a prompt is often a line of dashes
      let continues = self.dialect == Dialect::Oracle
        && !["rem", "remark", "pro", "prompt"]
          .iter()
          .any(|c| c.eq_ignore_ascii_case(first_word));
      while continues
        && (self.text[..self.pos].ends_with(" -") || self.text[..self.pos].ends_with("\t-"))
        && self.rest().trim_start_matches([' ', '\t']).starts_with(['\n', '\r'])
      {
        let next_line = self.rest().trim_start_matches([' ', '\t']);
        let next_line = next_line.strip_prefix('\r').unwrap_or(next_line);
        let next_line = next_line.strip_prefix('\n').unwrap_or(next_line);
        let line_end = next_line.find('\n').unwrap_or(next_line.len());
        let line_len = next_line[..line_end].trim_end_matches(is_space).len();
        if line_len == 0 {
          break;
        }
        self.pos = self.text.len() - next_line.len() + line_len;
      }
      return Some(TokenKind::Directive);
    }
    None
  }

  /// Whether this is a `GO` at the end of a line after a statement's `;` (ex. `SELECT 1; GO`),
  /// which is a batch separator like one on a line of its own, since a statement can't continue
  /// after the `;`.
  fn is_go_after_semicolon(&self) -> bool {
    matches!(self.dialect, Dialect::SqlServer | Dialect::Generic)
      && self.newlines == 0
      && self
        .tokens
        .last()
        .is_some_and(|token| token.kind == TokenKind::Semicolon)
      && self.rest().get(..2).is_some_and(|word| word.eq_ignore_ascii_case("go"))
      && {
        let after = self.rest_of_line()[2..].trim_matches(is_space);
        after.is_empty() || after.starts_with("--")
      }
  }

  /// Whether the previous token ends a statement, so that what's next can't be part of one.
  fn is_between_statements(&self) -> bool {
    self.last_significant.is_none_or(|kind| {
      matches!(
        kind,
        TokenKind::Semicolon | TokenKind::Delimiter | TokenKind::Directive | TokenKind::BatchSeparator
      )
    })
  }

  /// The delimiter a `DELIMITER` command sets, from the text that follows the word.
  fn delimiter_command_value(&self, value: &'a str) -> Option<&'a str> {
    if value.is_empty() || value.contains(is_space) || value.contains(['(', ')']) || value.ends_with(',') {
      return None;
    }
    // a column named `delimiter` is far more likely in the generic dialect than a delimiter made
    // of letters
    if self.dialect == Dialect::Generic && value.chars().any(|c| c.is_alphanumeric()) {
      return None;
    }
    Some(value)
  }

  // ---- comments ----

  fn is_double_dash_comment(&self) -> bool {
    // MySQL only reads `--` as a comment when whitespace follows it
    self.dialect != Dialect::MySql || self.peek_at(2).is_none_or(|c| is_space(c) || c.is_control())
  }

  fn line_comment(&mut self) -> TokenKind {
    let line = self.rest_of_line();
    self.pos += line.strip_suffix('\r').unwrap_or(line).len();
    TokenKind::LineComment
  }

  fn block_comment(&mut self) -> Result<TokenKind, LexError> {
    let start = self.pos;
    let nested = matches!(
      self.dialect,
      Dialect::PostgreSql | Dialect::SqlServer | Dialect::Generic
    );
    if nested && let Some(end) = find_nested_comment_end(&self.text[start..]) {
      self.pos = start + end;
      return Ok(TokenKind::BlockComment);
    }
    // a comment that only ends when nesting is ignored is read that way, since the generic dialect
    // can't know which the author meant
    if (!nested || self.dialect == Dialect::Generic)
      && let Some(end) = self.text[start + 2..].find("*/")
    {
      self.pos = start + 2 + end + 2;
      return Ok(TokenKind::BlockComment);
    }
    // SQLite lets a comment run to the end of the input
    if self.dialect == Dialect::Sqlite {
      self.pos = start + self.text[start..].trim_end_matches(is_space).len();
      return Ok(TokenKind::BlockComment);
    }
    Err(self.error(start, "unterminated block comment"))
  }

  fn hash(&mut self) -> TokenKind {
    let next = self.peek_at(1);
    let starts_name = next.is_some_and(|c| c == '#' || is_ident_start(c));
    match self.dialect {
      Dialect::MySql | Dialect::BigQuery => self.line_comment(),
      Dialect::SqlServer if starts_name => self.temp_table_name(),
      Dialect::PostgreSql => self.operator(),
      Dialect::Generic => {
        if next.is_none_or(is_space) || next == Some('!') {
          self.line_comment()
        } else if next == Some('@') && self.peek_at(2).is_some_and(is_ident_start) && !self.line_start {
          // a T-SQL temporary table whose name starts with `@` (ex. `#@temp`), as in SQL Server
          self.single(TokenKind::Other)
        } else if next.is_some_and(|c| is_operator_char(c) && c != '#') && !self.line_start {
          // a PostgreSQL operator (ex. `#>` or `#%#`), though at the start of a line it's far more
          // likely a MySQL comment (ex. `#=== section ===`)
          self.operator()
        } else if starts_name
          && !self.is_between_statements()
          && (!self.line_start || self.previous_continues_onto_next_line())
        {
          self.temp_table_name()
        } else {
          self.line_comment()
        }
      }
      _ => self.single(TokenKind::Other),
    }
  }

  fn temp_table_name(&mut self) -> TokenKind {
    self.pos += if self.peek_at(1) == Some('#') { 2 } else { 1 };
    self.eat_ident_chars();
    TokenKind::Word
  }

  /// Whether the previous token leaves its statement wanting more, such that a `#name` on the
  /// next line is more likely a temporary table's name than a comment.
  fn previous_continues_onto_next_line(&self) -> bool {
    self.last_significant.is_some_and(|kind| {
      matches!(
        kind,
        TokenKind::Word | TokenKind::Comma | TokenKind::OpenParen | TokenKind::Dot
      )
    })
  }

  // ---- strings and quoted identifiers ----

  fn backslash_escapes(&self) -> bool {
    matches!(self.dialect, Dialect::MySql | Dialect::BigQuery | Dialect::Snowflake)
  }

  /// A string starting `prefix_len` characters before the opening quote at `self.pos + prefix_len - 1`.
  fn string(&mut self, prefix_len: usize, backslash_escapes: bool) -> Result<TokenKind, LexError> {
    let start = self.pos;
    let quote_pos = start + prefix_len - 1;
    if self.dialect == Dialect::BigQuery && self.text[quote_pos..].starts_with("'''") {
      return self.triple_quoted(start, quote_pos, "'''");
    }
    self.pos = quote_pos;
    self.quoted_from(start, '\'', TokenKind::String, backslash_escapes)
  }

  fn double_quoted(&mut self) -> Result<TokenKind, LexError> {
    match self.dialect {
      Dialect::BigQuery if self.rest().starts_with("\"\"\"") => self.triple_quoted(self.pos, self.pos, "\"\"\""),
      Dialect::BigQuery => self.quoted('"', TokenKind::String, true),
      Dialect::MySql => self.quoted('"', TokenKind::String, true),
      _ => self.quoted('"', TokenKind::QuotedIdent, false),
    }
  }

  fn quoted(&mut self, quote: char, kind: TokenKind, backslash_escapes: bool) -> Result<TokenKind, LexError> {
    self.quoted_from(self.pos, quote, kind, backslash_escapes)
  }

  /// Reads from the opening quote at `self.pos` to the closing one. A doubled quote is an escaped
  /// quote rather than the end, as is one after a backslash when `backslash_escapes` is set.
  fn quoted_from(
    &mut self,
    start: usize,
    quote: char,
    kind: TokenKind,
    backslash_escapes: bool,
  ) -> Result<TokenKind, LexError> {
    let mut chars = self.text[self.pos + 1..].char_indices();
    while let Some((i, c)) = chars.next() {
      if backslash_escapes && c == '\\' {
        chars.next();
      } else if c == quote {
        if self.text[self.pos + 1 + i + 1..].starts_with(quote) {
          chars.next();
        } else {
          self.pos += 1 + i + 1;
          return Ok(kind);
        }
      }
    }
    Err(self.error(
      start,
      match kind {
        TokenKind::QuotedIdent => "unterminated quoted identifier",
        _ => "unterminated string",
      },
    ))
  }

  fn triple_quoted(&mut self, start: usize, quote_pos: usize, quotes: &str) -> Result<TokenKind, LexError> {
    let content_start = quote_pos + 3;
    let mut i = content_start;
    let bytes = self.text.as_bytes();
    while i < self.text.len() {
      if bytes[i] == b'\\' {
        i += 2;
      } else if bytes[i..].starts_with(quotes.as_bytes()) {
        self.pos = i + 3;
        return Ok(TokenKind::String);
      } else {
        i += 1;
      }
    }
    Err(self.error(start, "unterminated string"))
  }

  /// An Oracle quoted string (`q'[text]'`), whose delimiter is the character after the quote.
  fn oracle_quoted_string(&mut self, prefix_len: usize) -> Result<TokenKind, LexError> {
    let start = self.pos;
    let delimiter_pos = start + prefix_len + 1;
    let Some(open) = self.text[delimiter_pos..].chars().next() else {
      return Err(self.error(start, "unterminated string"));
    };
    let close = match open {
      '[' => ']',
      '{' => '}',
      '(' => ')',
      '<' => '>',
      c => c,
    };
    let content_start = delimiter_pos + open.len_utf8();
    let mut search = content_start;
    while let Some(index) = self.text[search..].find(close) {
      let close_pos = search + index;
      if self.text[close_pos + close.len_utf8()..].starts_with('\'') {
        self.pos = close_pos + close.len_utf8() + 1;
        return Ok(TokenKind::String);
      }
      search = close_pos + close.len_utf8();
    }
    Err(self.error(start, "unterminated string"))
  }

  fn open_bracket(&mut self) -> Result<TokenKind, LexError> {
    let is_identifier = match self.dialect {
      Dialect::SqlServer | Dialect::Sqlite => true,
      Dialect::Generic => self.generic_bracket_is_identifier(),
      _ => false,
    };
    if is_identifier {
      let start = self.pos;
      // a `]` is escaped by doubling it, which the generic dialect doesn't allow for, since a
      // doubled `]` there is more likely the end of nested arrays (ex. `ARRAY[[1, 2]]`)
      let mut i = start + 1;
      while let Some(index) = self.text[i..].find(']') {
        let close = i + index;
        if self.dialect != Dialect::Generic && self.text[close + 1..].starts_with(']') {
          i = close + 2;
        } else {
          self.pos = close + 1;
          return Ok(TokenKind::QuotedIdent);
        }
      }
      if self.dialect == Dialect::Generic {
        return Ok(self.single(TokenKind::OpenBracket));
      }
      return Err(self.error(start, "unterminated quoted identifier"));
    }
    Ok(self.single(TokenKind::OpenBracket))
  }

  /// Whether a `[` is a T-SQL quoted identifier rather than the start of an array or a subscript.
  ///
  /// A subscript follows what it indexes without a space (`arr[1]`, `ARRAY[1, 2]`), and the
  /// contents of a quoted identifier stay on one line without nesting brackets. Getting this wrong
  /// is harmless: a quoted identifier is written as it is, so all that's lost is formatting
  /// within the brackets.
  fn generic_bracket_is_identifier(&self) -> bool {
    let follows_value = !self.space
      && self.newlines == 0
      && self.tokens.last().is_some_and(|token| {
        matches!(
          token.kind,
          TokenKind::Word
            | TokenKind::QuotedIdent
            | TokenKind::CloseParen
            | TokenKind::CloseBracket
            | TokenKind::Variable
        )
      });
    // brackets within an array's brackets are another array (ex. `ARRAY[[1, 2], [3, 4]]`)
    if follows_value || self.open_brackets > 0 {
      return false;
    }
    let line = self.rest_of_line();
    match line[1..].find(']') {
      Some(index) => {
        let content = &line[1..1 + index];
        !content.is_empty() && !content.contains('[') && !content.trim_matches(is_space).is_empty()
      }
      None => false,
    }
  }

  fn open_brace(&mut self) -> Result<TokenKind, LexError> {
    let close = match self.peek_at(1) {
      Some('{') => "}}",
      Some('%') => "%}",
      Some('#') => "#}",
      _ => return Ok(self.single(TokenKind::OpenBrace)),
    };
    let start = self.pos;
    match self.text[start + 2..].find(close) {
      Some(index) => {
        self.pos = start + 2 + index + 2;
        let ends_line = self.rest_of_line().trim_matches(is_space).is_empty();
        let is_tag = close != "}}";
        Ok(if is_tag && self.line_start && ends_line {
          TokenKind::TemplateLine
        } else {
          TokenKind::Template
        })
      }
      None => Err(self.error(start, "unterminated template tag")),
    }
  }

  // ---- variables and parameters ----

  fn at(&mut self) -> TokenKind {
    if self.dialect == Dialect::PostgreSql {
      return self.operator();
    }
    // a Snowflake stage and the path in it (ex. `@my_stage/path/file.csv`, `@~` or `@%table`)
    if self.dialect == Dialect::Snowflake
      && self
        .peek_at(1)
        .is_some_and(|c| c == '~' || c == '%' || is_ident_start(c))
    {
      self.pos += 1;
      self.pos += self
        .rest()
        .find(|c: char| c.is_ascii_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '\'' | '"'))
        .unwrap_or(self.rest().len());
      return TokenKind::Variable;
    }
    let start = self.pos;
    let mut i = start + 1;
    if self.text[i..].starts_with('@') {
      i += 1;
    }
    match self.text[i..].chars().next() {
      Some(c) if is_ident_start(c) => {
        self.pos = i;
        self.eat_variable_chars();
        TokenKind::Variable
      }
      // a quoted MySQL user variable (`@'name'`)
      Some(quote @ ('\'' | '"' | '`')) if i == start + 1 && self.dialect != Dialect::PostgreSql => {
        self.pos = i;
        match self.quoted_from(start, quote, TokenKind::Variable, false) {
          Ok(kind) => kind,
          Err(_) => {
            self.pos = start;
            self.operator()
          }
        }
      }
      _ => self.operator(),
    }
  }

  fn colon(&mut self) -> TokenKind {
    match self.peek_at(1) {
      Some(':') | Some('=') => {
        self.pos += 2;
        TokenKind::Operator
      }
      Some(c) if is_ident_start(c) || c.is_ascii_digit() => {
        self.pos += 1;
        self.eat_variable_chars();
        TokenKind::Variable
      }
      // a psql variable interpolated as a literal or an identifier (`:'name'` or `:"name"`)
      Some(quote @ ('\'' | '"')) if matches!(self.dialect, Dialect::PostgreSql | Dialect::Generic) => {
        let start = self.pos;
        self.pos += 1;
        match self.quoted_from(start, quote, TokenKind::Variable, false) {
          Ok(kind) => kind,
          Err(_) => {
            self.pos = start + 1;
            TokenKind::Operator
          }
        }
      }
      _ => self.single(TokenKind::Operator),
    }
  }

  fn question_mark(&mut self) -> TokenKind {
    // a parameter can't follow a value, so a `?` that does is PostgreSQL's operator (ex. `a?'key'`)
    let follows_value = self.tokens.last().is_some_and(|token| {
      matches!(
        token.kind,
        TokenKind::Word
          | TokenKind::QuotedIdent
          | TokenKind::String
          | TokenKind::Number
          | TokenKind::CloseParen
          | TokenKind::CloseBracket
      )
    }) && !self
      .tokens
      .last()
      .is_some_and(|token| keywords::contains(keywords::RESERVED, token.text) && token.kind == TokenKind::Word);
    if self.dialect == Dialect::PostgreSql && (follows_value || matches!(self.peek_at(1), Some('|' | '&' | '-' | '#')))
    {
      return self.operator();
    }
    self.pos += 1;
    while self.peek().is_some_and(|c| c.is_ascii_digit()) {
      self.pos += 1;
    }
    TokenKind::Variable
  }

  fn dollar(&mut self) -> Result<TokenKind, LexError> {
    let start = self.pos;
    let next = self.peek_at(1);
    if next.is_some_and(|c| c.is_ascii_digit()) {
      self.pos += 1;
      while self.peek().is_some_and(|c| c.is_ascii_digit()) {
        self.pos += 1;
      }
      return Ok(TokenKind::Variable);
    }
    if next == Some('{') {
      return match self.text[start..].find('}') {
        Some(index) => {
          self.pos = start + index + 1;
          Ok(TokenKind::Variable)
        }
        None => Ok(self.single(TokenKind::Other)),
      };
    }
    if let Some(tag_len) = self.dollar_quote_tag_len() {
      let tag = &self.text[start..start + tag_len];
      return match self.text[start + tag_len..].find(tag) {
        Some(index) => {
          self.pos = start + tag_len + index + tag_len;
          Ok(TokenKind::DollarString)
        }
        None => Err(self.error(start, "unterminated dollar-quoted string")),
      };
    }
    if next.is_some_and(is_ident_start) {
      self.pos += 1;
      self.eat_ident_chars();
      return Ok(TokenKind::Variable);
    }
    Ok(self.single(TokenKind::Other))
  }

  /// The length of the dollar quote's tag (`$tag$` or `$$`) starting at the current position.
  fn dollar_quote_tag_len(&self) -> Option<usize> {
    if !matches!(
      self.dialect,
      Dialect::PostgreSql | Dialect::Snowflake | Dialect::Generic
    ) {
      return None;
    }
    let rest = &self.text[self.pos + 1..];
    for (i, c) in rest.char_indices() {
      if c == '$' {
        return Some(i + 2);
      }
      // `$` is an identifier character, so the tag ends at the first one rather than going on
      let is_valid = if i == 0 {
        is_ident_start(c)
      } else {
        is_ident_char(c) && c != '$'
      };
      if !is_valid {
        return None;
      }
    }
    None
  }

  /// A Python DB-API placeholder (`%s` or `%(name)s`).
  fn is_python_placeholder(&mut self) -> bool {
    let rest = &self.text[self.pos + 1..];
    let len = if let Some(after) = rest.strip_prefix('(') {
      match after.find(')') {
        Some(index) if after[..index].chars().all(is_ident_char) && after[index + 1..].starts_with(['s', 'd']) => {
          index + 3
        }
        _ => return false,
      }
    } else if rest.starts_with('s') {
      1
    } else {
      return false;
    };
    if rest[len..].chars().next().is_some_and(is_ident_char) {
      return false;
    }
    self.pos += 1 + len;
    true
  }

  // ---- words, numbers and operators ----

  fn word(&mut self) -> Result<TokenKind, LexError> {
    let start = self.pos;
    self.eat_ident_chars();
    let word = &self.text[start..self.pos];
    let next = self.peek();

    if next == Some('-') && self.dialect == Dialect::BigQuery {
      self.eat_bigquery_dashed_name();
      if self.pos > start + word.len() {
        return Ok(TokenKind::Word);
      }
    }
    // a file's URL in a Snowflake `PUT` or `GET` (ex. `file:///tmp/data.csv`), whose `//` would
    // otherwise start a comment
    if self.dialect == Dialect::Snowflake && word.eq_ignore_ascii_case("file") && self.rest().starts_with("://") {
      self.eat_until_space();
      return Ok(TokenKind::Word);
    }

    // a prefix written directly against the quote of the string or identifier it modifies
    if next == Some('\'') {
      let prefix_len = word.len() + 1;
      if word.eq_ignore_ascii_case("q") || word.eq_ignore_ascii_case("nq") {
        if matches!(self.dialect, Dialect::Oracle | Dialect::Generic) {
          self.pos = start;
          return self.oracle_quoted_string(word.len());
        }
      } else if word.eq_ignore_ascii_case("e") {
        self.pos = start;
        return self.string(prefix_len, true);
      } else if ["n", "x", "b"].iter().any(|prefix| word.eq_ignore_ascii_case(prefix)) {
        self.pos = start;
        return self.string(prefix_len, self.backslash_escapes());
      } else if self.dialect == Dialect::BigQuery && is_bigquery_string_prefix(word) {
        // a backslash in a raw string isn't an escape, but still keeps the quote after it from
        // ending the string (ex. `r'\''`)
        self.pos = start;
        return self.string(prefix_len, true);
      }
    } else if next == Some('"') && self.dialect == Dialect::BigQuery && is_bigquery_string_prefix(word) {
      if self.rest().starts_with("\"\"\"") {
        return self.triple_quoted(start, self.pos, "\"\"\"");
      }
      return self.quoted_from(start, '"', TokenKind::String, true);
    } else if next == Some('&') && word.eq_ignore_ascii_case("u") {
      // a unicode escaped string or identifier (`U&'..'` or `U&".."`)
      match self.peek_at(1) {
        Some('\'') => {
          self.pos += 1;
          return self.quoted_from(start, '\'', TokenKind::String, false);
        }
        Some('"') => {
          self.pos += 1;
          return self.quoted_from(start, '"', TokenKind::QuotedIdent, false);
        }
        _ => {}
      }
    }
    Ok(TokenKind::Word)
  }

  /// The rest of a BigQuery name holding dashes (ex. `my-project.dataset.table`), which is only
  /// taken as a name where it's in the path of a table, since `a-b` is otherwise a subtraction.
  fn eat_bigquery_dashed_name(&mut self) {
    let mut end = self.pos;
    while self.text[end..].starts_with('-')
      && self.text[end + 1..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
    {
      end += 1;
      end += self.text[end..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(self.text.len() - end);
    }
    if end == self.pos {
      return;
    }
    let rest = &self.text[end..];
    let is_name = (rest.starts_with('.') && rest[1..].starts_with(is_ident_start))
      || self.last_significant == Some(TokenKind::Dot)
      || self
        .tokens
        .iter()
        .rev()
        .find(|token| !token.is_comment())
        .is_some_and(|token| {
          ["FROM", "JOIN", "INTO", "TABLE", "UPDATE"]
            .iter()
            .any(|word| token.is_word(word))
        });
    if is_name {
      self.pos = end;
    }
  }

  fn eat_until_space(&mut self) {
    self.pos += self
      .rest()
      .find(|c: char| c.is_ascii_whitespace())
      .unwrap_or(self.rest().len());
  }

  fn eat_ident_chars(&mut self) {
    let allows_hash = matches!(self.dialect, Dialect::SqlServer | Dialect::Oracle);
    while let Some(c) = self.peek() {
      // the MySQL client ends a statement at its delimiter wherever it appears outside of a
      // string or comment, so `END$$` is `END` followed by the delimiter `$$`
      if self
        .delimiter
        .is_some_and(|delimiter| self.rest().starts_with(delimiter))
      {
        break;
      }
      if is_ident_char(c) || (allows_hash && c == '#') {
        self.pos += c.len_utf8();
      } else {
        break;
      }
    }
  }

  fn eat_variable_chars(&mut self) {
    while let Some(c) = self.peek() {
      // `#` starts a comment in MySQL and BigQuery, and in the generic dialect when whitespace
      // follows it
      let allows_hash = match self.dialect {
        Dialect::SqlServer | Dialect::Oracle => true,
        Dialect::Generic => self.peek_at(1).is_some_and(is_ident_char),
        _ => false,
      };
      if is_ident_char(c) || (allows_hash && c == '#') {
        self.pos += c.len_utf8();
      } else {
        break;
      }
    }
  }

  fn number(&mut self) -> TokenKind {
    let bytes = self.text.as_bytes();
    let start = self.pos;
    let radix_digits = match (bytes.get(start), bytes.get(start + 1).map(|b| b.to_ascii_lowercase())) {
      (Some(b'0'), Some(b'x')) => Some(16),
      (Some(b'0'), Some(b'b')) => Some(2),
      (Some(b'0'), Some(b'o')) => Some(8),
      _ => None,
    };
    if let Some(radix) = radix_digits
      && bytes.get(start + 2).is_some_and(|b| (*b as char).is_digit(radix))
    {
      self.pos += 2;
      while self.peek().is_some_and(|c| c.is_digit(radix) || c == '_') {
        self.pos += 1;
      }
    } else {
      self.eat_digits();
      // `1..10` is a range in PL/pgSQL and PL/SQL, not a number followed by a fraction
      if self.peek() == Some('.') && self.peek_at(1) != Some('.') {
        self.pos += 1;
        self.eat_digits();
      }
      self.eat_exponent();
    }
    // a number running into letters is a word (ex. MySQL allows identifiers like `1st`)
    if self.peek().is_some_and(|c| c.is_alphabetic() || c == '_') {
      self.eat_ident_chars();
      return TokenKind::Word;
    }
    TokenKind::Number
  }

  fn eat_digits(&mut self) {
    while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
      self.pos += 1;
    }
  }

  fn eat_exponent(&mut self) {
    if matches!(self.peek(), Some('e' | 'E')) {
      let sign_len = usize::from(matches!(self.peek_at(1), Some('+' | '-')));
      if self.peek_at(1 + sign_len).is_some_and(|c| c.is_ascii_digit()) {
        self.pos += 1 + sign_len;
        self.eat_digits();
      }
    }
  }

  fn dot(&mut self) -> TokenKind {
    if self.peek_at(1) == Some('.') {
      self.pos += 2;
      return TokenKind::Operator;
    }
    let follows_value = !self.space
      && self.newlines == 0
      && self.tokens.last().is_some_and(|token| {
        matches!(
          token.kind,
          TokenKind::Word | TokenKind::QuotedIdent | TokenKind::CloseParen | TokenKind::Variable
        )
      });
    if !follows_value && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
      self.pos += 1;
      self.eat_digits();
      self.eat_exponent();
      return TokenKind::Number;
    }
    self.single(TokenKind::Dot)
  }

  fn operator(&mut self) -> TokenKind {
    let start = self.pos;
    let mut end = start;
    for (i, c) in self.text[start..].char_indices() {
      let at = start + i;
      let is_part = is_operator_char(c)
        && !(i > 0 && self.starts_something_else(at, c))
        && (c != '#' || i == 0 || matches!(self.dialect, Dialect::PostgreSql | Dialect::Generic))
        && (c != '?' || i == 0 || self.dialect == Dialect::PostgreSql);
      if !is_part {
        break;
      }
      end = at + c.len_utf8();
    }
    if end == start {
      end = start + self.peek().unwrap().len_utf8();
    }
    // like PostgreSQL, an operator can't end in `+` or `-` unless it holds one of the characters
    // that only operators use, so `=-1` is `=` followed by `-1`, and no other dialect has an
    // operator ending in either
    let mut text = &self.text[start..end];
    let is_postgres = matches!(self.dialect, Dialect::PostgreSql | Dialect::Generic);
    if !is_postgres || !text.contains(['~', '!', '@', '#', '%', '^', '&', '|', '`', '?']) {
      while text.len() > 1 && text.ends_with(['+', '-']) {
        text = &text[..text.len() - 1];
      }
    }
    self.pos = start + text.len();
    TokenKind::Operator
  }

  /// Whether the operator character `c` at `at` starts a token of its own rather than continuing
  /// the operator before it.
  fn starts_something_else(&self, at: usize, c: char) -> bool {
    let next = self.text[at + c.len_utf8()..].chars().next();
    match c {
      '-' => next == Some('-'),
      '/' => next == Some('*') || (next == Some('/') && self.dialect == Dialect::Snowflake),
      '@' => self.dialect != Dialect::PostgreSql && next.is_some_and(|c| c == '@' || is_ident_start(c)),
      _ => false,
    }
  }

  fn single(&mut self, kind: TokenKind) -> TokenKind {
    self.pos += self.peek().unwrap().len_utf8();
    kind
  }

  // ---- helpers ----

  fn rest(&self) -> &'a str {
    &self.text[self.pos..]
  }

  fn rest_of_line(&self) -> &'a str {
    let rest = self.rest();
    match rest.find('\n') {
      Some(index) => &rest[..index],
      None => rest,
    }
  }

  fn peek(&self) -> Option<char> {
    self.text[self.pos..].chars().next()
  }

  fn peek_at(&self, offset: usize) -> Option<char> {
    self.text[self.pos..].chars().nth(offset)
  }

  fn error(&self, start: usize, message: &str) -> LexError {
    LexError {
      message: message.to_string(),
      span: start..start + self.text[start..].chars().next().map(|c| c.len_utf8()).unwrap_or(0),
    }
  }
}

/// Whether the tokens are a `COPY .. FROM STDIN`.
fn is_copy_from_stdin(tokens: &[Token]) -> bool {
  let mut words = tokens.iter().filter(|token| !token.is_comment());
  if !words.next().is_some_and(|token| token.is_word("COPY")) {
    return false;
  }
  let mut after_from = false;
  for token in words {
    if token.is_word("FROM") {
      after_from = true;
    } else if after_from {
      return token.is_word("STDIN");
    }
  }
  false
}

fn find_nested_comment_end(text: &str) -> Option<usize> {
  let bytes = text.as_bytes();
  let mut depth = 0;
  let mut i = 0;
  while i + 1 < bytes.len() {
    if bytes[i] == b'/' && bytes[i + 1] == b'*' {
      depth += 1;
      i += 2;
    } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
      depth -= 1;
      i += 2;
      if depth == 0 {
        return Some(i);
      }
    } else {
      i += 1;
    }
  }
  None
}

fn is_bigquery_string_prefix(word: &str) -> bool {
  ["r", "b", "rb", "br"]
    .iter()
    .any(|prefix| word.eq_ignore_ascii_case(prefix))
}

fn is_sqlcmd_command(line: &str) -> bool {
  const COMMANDS: &[&str] = &[
    "setvar",
    "r",
    "on",
    "connect",
    "out",
    "error",
    "exit",
    "quit",
    "listvar",
    "serverlist",
    "xml",
    "help",
    "reset",
    "ed",
    "perftrace",
  ];
  let Some(rest) = line.strip_prefix(':') else {
    return false;
  };
  let word_len = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
  let word = &rest[..word_len];
  word_len > 0
    && rest[word_len..].chars().next().is_none_or(is_space)
    && COMMANDS.iter().any(|c| c.eq_ignore_ascii_case(word))
}

fn is_sql_plus_command(first_word: &str, rest: &str) -> bool {
  // only the commands whose names are unlikely to start a line of SQL
  const COMMANDS: &[&str] = &[
    "prompt", "rem", "remark", "spool", "whenever", "define", "undefine", "accept", "ttitle", "btitle", "show",
    "column", "col", "desc", "describe", "conn", "connect", "var", "variable", "print", "host", "clear", "compute",
    "pause",
  ];
  const SET_OPTIONS: &[&str] = &[
    "serveroutput",
    "define",
    "echo",
    "feedback",
    "linesize",
    "lines",
    "pagesize",
    "pages",
    "verify",
    "timing",
    "heading",
    "termout",
    "trimspool",
    "trimout",
    "sqlblanklines",
    "autocommit",
    "colsep",
    "long",
    "numwidth",
    "tab",
    "wrap",
    "arraysize",
    "scan",
    "concat",
    "escape",
    "markup",
    "newpage",
    "underline",
  ];
  if first_word.starts_with('@') {
    return true;
  }
  if first_word.eq_ignore_ascii_case("set") {
    let option = rest.split_whitespace().next().unwrap_or("");
    return SET_OPTIONS.iter().any(|o| o.eq_ignore_ascii_case(option));
  }
  // `EXEC p` runs a procedure, while `EXECUTE IMMEDIATE` is only in PL/SQL
  if first_word.eq_ignore_ascii_case("exec") || first_word.eq_ignore_ascii_case("execute") {
    return !rest
      .split_whitespace()
      .next()
      .is_some_and(|word| word.eq_ignore_ascii_case("immediate"));
  }
  COMMANDS.iter().any(|c| c.eq_ignore_ascii_case(first_word))
}

/// Whether `c` can start an identifier. Whitespace beyond ASCII is part of an identifier in
/// PostgreSQL and MySQL, so it's kept with the text around it rather than taken as a space.
pub fn is_ident_start(c: char) -> bool {
  c.is_alphabetic() || c == '_' || is_unicode_space(c)
}

pub fn is_ident_char(c: char) -> bool {
  c.is_alphanumeric() || c == '_' || c == '$' || is_unicode_space(c)
}

fn is_unicode_space(c: char) -> bool {
  !c.is_ascii() && c.is_whitespace()
}

/// Whether `c` is whitespace between tokens, which is only ever ASCII.
pub fn is_space(c: char) -> bool {
  matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn is_operator_char(c: char) -> bool {
  matches!(
    c,
    '+' | '-' | '*' | '/' | '<' | '>' | '=' | '~' | '!' | '@' | '#' | '%' | '^' | '&' | '|' | '?'
  )
}

#[cfg(test)]
mod test {
  use super::*;

  fn tokens(text: &str, dialect: Dialect) -> Vec<(TokenKind, &str)> {
    lex(text, dialect)
      .unwrap()
      .into_iter()
      .map(|t| (t.kind, t.text))
      .collect()
  }

  fn texts(text: &str, dialect: Dialect) -> Vec<&str> {
    tokens(text, dialect).into_iter().map(|(_, text)| text).collect()
  }

  use TokenKind::*;

  #[test]
  fn lexes_basic_select() {
    assert_eq!(
      tokens("SELECT a, b.c FROM t WHERE x >= 1.5;", Dialect::Generic),
      vec![
        (Word, "SELECT"),
        (Word, "a"),
        (Comma, ","),
        (Word, "b"),
        (Dot, "."),
        (Word, "c"),
        (Word, "FROM"),
        (Word, "t"),
        (Word, "WHERE"),
        (Word, "x"),
        (Operator, ">="),
        (Number, "1.5"),
        (Semicolon, ";"),
      ]
    );
  }

  #[test]
  fn records_whitespace_before_tokens() {
    let tokens = lex("a  b\n\nc(d)", Dialect::Generic).unwrap();
    assert_eq!(
      tokens
        .iter()
        .map(|t| (t.space_before, t.newlines_before))
        .collect::<Vec<_>>(),
      vec![(false, 0), (true, 0), (true, 2), (false, 0), (false, 0), (false, 0)]
    );
  }

  #[test]
  fn lexes_strings() {
    assert_eq!(
      texts("'it''s' N'x' E'a\\'b' X'FF' B'01'", Dialect::Generic),
      vec!["'it''s'", "N'x'", "E'a\\'b'", "X'FF'", "B'01'"]
    );
    // backslash escapes depend on the dialect
    assert_eq!(texts(r"'a\' b", Dialect::Generic), vec![r"'a\'", "b"]);
    assert_eq!(texts(r"'a\'b'", Dialect::MySql), vec![r"'a\'b'"]);
    assert_eq!(texts(r#""a\"b""#, Dialect::MySql), vec![r#""a\"b""#]);
    assert_eq!(texts(r"'C:\'", Dialect::SqlServer), vec![r"'C:\'"]);
    assert_eq!(
      texts("U&'d\\0061t' U&\"x\"", Dialect::PostgreSql),
      vec!["U&'d\\0061t'", "U&\"x\""]
    );
    assert_eq!(texts("'multi\nline'", Dialect::Generic), vec!["'multi\nline'"]);
  }

  #[test]
  fn lexes_oracle_quoted_strings() {
    assert_eq!(
      texts("q'[it's]' Q'{a}b}' q'!x!' nq'(y)'", Dialect::Oracle),
      vec!["q'[it's]'", "Q'{a}b}'", "q'!x!'", "nq'(y)'"]
    );
  }

  #[test]
  fn lexes_bigquery_strings() {
    assert_eq!(
      texts("r'\\d' b\"x\" '''a\n'b'''", Dialect::BigQuery),
      vec!["r'\\d'", "b\"x\"", "'''a\n'b'''"]
    );
    assert_eq!(tokens("\"text\"", Dialect::BigQuery), vec![(String, "\"text\"")]);
  }

  #[test]
  fn lexes_dollar_strings() {
    assert_eq!(
      tokens("$$a $b$ ;$$ $fn$x$$y$fn$ $1", Dialect::PostgreSql),
      vec![
        (DollarString, "$$a $b$ ;$$"),
        (DollarString, "$fn$x$$y$fn$"),
        (Variable, "$1")
      ]
    );
    assert_eq!(
      tokens("$action $1", Dialect::SqlServer),
      vec![(Variable, "$action"), (Variable, "$1")]
    );
    assert!(lex("$$ unterminated", Dialect::PostgreSql).is_err());
  }

  #[test]
  fn lexes_quoted_identifiers() {
    assert_eq!(
      tokens("\"a\"\"b\" `c``d` [e]]f]", Dialect::SqlServer),
      vec![
        (QuotedIdent, "\"a\"\"b\""),
        (QuotedIdent, "`c``d`"),
        (QuotedIdent, "[e]]f]")
      ]
    );
  }

  #[test]
  fn lexes_brackets_by_dialect() {
    assert_eq!(texts("arr[1]", Dialect::PostgreSql), vec!["arr", "[", "1", "]"]);
    assert_eq!(texts("[My Table]", Dialect::SqlServer), vec!["[My Table]"]);
    // the generic dialect tells them apart by what comes before
    assert_eq!(
      texts("arr[1] ARRAY[1, 2]", Dialect::Generic),
      vec!["arr", "[", "1", "]", "ARRAY", "[", "1", ",", "2", "]"]
    );
    assert_eq!(
      texts("SELECT [a], [b] FROM [dbo].[t]", Dialect::Generic),
      vec!["SELECT", "[a]", ",", "[b]", "FROM", "[dbo]", ".", "[t]"]
    );
  }

  #[test]
  fn lexes_comments() {
    assert_eq!(
      tokens("a -- c\r\nb /* x /* y */ z */ c", Dialect::PostgreSql),
      vec![
        (Word, "a"),
        (LineComment, "-- c"),
        (Word, "b"),
        (BlockComment, "/* x /* y */ z */"),
        (Word, "c")
      ]
    );
    // MySQL comments don't nest
    assert_eq!(texts("/* x /* y */ z", Dialect::MySql), vec!["/* x /* y */", "z"]);
    // the generic dialect falls back to not nesting when that's the only way the comment ends
    assert_eq!(texts("/* x /* y */ z", Dialect::Generic), vec!["/* x /* y */", "z"]);
    assert!(lex("/* x", Dialect::Generic).is_err());
  }

  #[test]
  fn lexes_double_dash_in_mysql() {
    assert_eq!(texts("a--b", Dialect::MySql), vec!["a", "-", "-", "b"]);
    assert_eq!(texts("a-- b", Dialect::MySql), vec!["a", "-- b"]);
    assert_eq!(texts("a--", Dialect::MySql), vec!["a", "--"]);
  }

  #[test]
  fn lexes_hash_by_dialect() {
    assert_eq!(tokens("# c", Dialect::MySql), vec![(LineComment, "# c")]);
    assert_eq!(tokens("SELECT * FROM #temp", Dialect::SqlServer)[3], (Word, "#temp"));
    assert_eq!(tokens("SELECT * FROM ##temp", Dialect::SqlServer)[3], (Word, "##temp"));
    assert_eq!(tokens("a #> b", Dialect::PostgreSql)[1], (Operator, "#>"));
    assert_eq!(tokens("a # b", Dialect::PostgreSql)[1], (Operator, "#"));
    assert_eq!(tokens("#comment", Dialect::Generic), vec![(LineComment, "#comment")]);
    assert_eq!(tokens("# comment", Dialect::Generic), vec![(LineComment, "# comment")]);
    assert_eq!(tokens("FROM #temp", Dialect::Generic)[1], (Word, "#temp"));
    assert_eq!(tokens("FROM\n#temp", Dialect::Generic)[1], (Word, "#temp"));
    assert_eq!(tokens("SELECT 1;\n#temp", Dialect::Generic)[3], (LineComment, "#temp"));
    assert_eq!(tokens("USE db; #inline", Dialect::Generic)[3], (LineComment, "#inline"));
    assert_eq!(tokens("a #%# b", Dialect::Generic)[1], (Operator, "#%#"));
    assert_eq!(
      tokens("#=== section ===", Dialect::Generic)[0],
      (LineComment, "#=== section ===")
    );
  }

  #[test]
  fn lexes_variables() {
    assert_eq!(
      tokens("@a @@ROWCOUNT :b :1 ? ?1 ${x} %s %(name)s", Dialect::Generic),
      vec![
        (Variable, "@a"),
        (Variable, "@@ROWCOUNT"),
        (Variable, ":b"),
        (Variable, ":1"),
        (Variable, "?"),
        (Variable, "?1"),
        (Variable, "${x}"),
        (Variable, "%s"),
        (Variable, "%(name)s"),
      ]
    );
    assert_eq!(texts("@'my var'", Dialect::MySql), vec!["@'my var'"]);
    assert_eq!(texts(":'v' :\"v\"", Dialect::PostgreSql), vec![":'v'", ":\"v\""]);
    assert_eq!(texts("&x &&y", Dialect::Oracle), vec!["&x", "&&y"]);
  }

  #[test]
  fn lexes_operators() {
    assert_eq!(texts("a=-1", Dialect::Generic), vec!["a", "=", "-", "1"]);
    assert_eq!(
      texts("a<>b a!=b a<=>b a||b a::int a:=1 a=>b", Dialect::Generic),
      vec![
        "a", "<>", "b", "a", "!=", "b", "a", "<=>", "b", "a", "||", "b", "a", "::", "int", "a", ":=", "1", "a", "=>",
        "b"
      ]
    );
    assert_eq!(
      texts("j->>'a' j#>>'{a}' j @> k j ?| k", Dialect::PostgreSql),
      vec!["j", "->>", "'a'", "j", "#>>", "'{a}'", "j", "@>", "k", "j", "?|", "k"]
    );
    assert_eq!(texts("a=?", Dialect::Generic), vec!["a", "=", "?"]);
    assert_eq!(
      tokens("a<@ARRAY[1] AND b@@c AND d?'k' AND e = ?", Dialect::PostgreSql),
      vec![
        (Word, "a"),
        (Operator, "<@"),
        (Word, "ARRAY"),
        (OpenBracket, "["),
        (Number, "1"),
        (CloseBracket, "]"),
        (Word, "AND"),
        (Word, "b"),
        (Operator, "@@"),
        (Word, "c"),
        (Word, "AND"),
        (Word, "d"),
        (Operator, "?"),
        (String, "'k'"),
        (Word, "AND"),
        (Word, "e"),
        (Operator, "="),
        (Variable, "?"),
      ]
    );
    assert_eq!(texts("a=@b", Dialect::SqlServer), vec!["a", "=", "@b"]);
    assert_eq!(texts("a*-b", Dialect::Generic), vec!["a", "*", "-", "b"]);
    assert_eq!(texts("1--x\n2", Dialect::Generic), vec!["1", "--x", "2"]);
    assert_eq!(texts("SET @x *= 2", Dialect::SqlServer), vec!["SET", "@x", "*=", "2"]);
  }

  #[test]
  fn lexes_numbers() {
    assert_eq!(
      tokens("1 1.5 .5 1e10 1.5E-3 0xFF 1_000 1..10", Dialect::Generic),
      vec![
        (Number, "1"),
        (Number, "1.5"),
        (Number, ".5"),
        (Number, "1e10"),
        (Number, "1.5E-3"),
        (Number, "0xFF"),
        (Number, "1_000"),
        (Number, "1"),
        (Operator, ".."),
        (Number, "10"),
      ]
    );
    assert_eq!(
      tokens("1st t.5", Dialect::MySql),
      vec![(Word, "1st"), (Word, "t"), (Dot, "."), (Number, "5")]
    );
  }

  #[test]
  fn lexes_templates() {
    assert_eq!(
      tokens("{{ ref('x') }} {% if a %} {# c #} {fn now()}", Dialect::Generic),
      vec![
        (Template, "{{ ref('x') }}"),
        (Template, "{% if a %}"),
        (Template, "{# c #}"),
        (OpenBrace, "{"),
        (Word, "fn"),
        (Word, "now"),
        (OpenParen, "("),
        (CloseParen, ")"),
        (CloseBrace, "}"),
      ]
    );
  }

  #[test]
  fn lexes_template_tags_on_their_own_lines() {
    assert_eq!(
      tokens("SELECT 1\n  {% if x %}\nUNION {% endif %}\n{# c #}", Dialect::Generic),
      vec![
        (Word, "SELECT"),
        (Number, "1"),
        (TemplateLine, "{% if x %}"),
        (Word, "UNION"),
        (Template, "{% endif %}"),
        (TemplateLine, "{# c #}"),
      ]
    );
    // an expression is never on a line of its own
    assert_eq!(
      tokens("{{ config(x=1) }}\nSELECT 1", Dialect::Generic)[0],
      (Template, "{{ config(x=1) }}")
    );
  }

  #[test]
  fn lexes_batch_separators() {
    assert_eq!(
      tokens("SELECT 1\nGO\nSELECT 2\ngo 5 -- c", Dialect::SqlServer),
      vec![
        (Word, "SELECT"),
        (Number, "1"),
        (BatchSeparator, "GO"),
        (Word, "SELECT"),
        (Number, "2"),
        (BatchSeparator, "go 5"),
        (LineComment, "-- c"),
      ]
    );
    // `GO` is only a separator on a line of its own, or after a `;` at the end of a line
    assert_eq!(tokens("SELECT go FROM t", Dialect::SqlServer)[1], (Word, "go"));
    assert_eq!(
      tokens("SELECT 1; GO\nSELECT 2", Dialect::SqlServer)[3],
      (BatchSeparator, "GO")
    );
    assert_eq!(
      tokens("SELECT 1; GO -- c\nSELECT 2", Dialect::SqlServer)[3],
      (BatchSeparator, "GO")
    );
    assert_eq!(tokens("SELECT 1; go_on", Dialect::SqlServer)[3], (Word, "go_on"));
    assert_eq!(
      tokens("BEGIN NULL; END;\n/\n", Dialect::Oracle)[5],
      (BatchSeparator, "/")
    );
    assert_eq!(tokens("a\n/ b", Dialect::Oracle)[1], (Operator, "/"));
  }

  #[test]
  fn lexes_delimiter_command() {
    assert_eq!(
      tokens(
        "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; END //\nDELIMITER ;\nSELECT 1;",
        Dialect::MySql
      ),
      vec![
        (Directive, "DELIMITER //"),
        (Word, "CREATE"),
        (Word, "PROCEDURE"),
        (Word, "p"),
        (OpenParen, "("),
        (CloseParen, ")"),
        (Word, "BEGIN"),
        (Word, "SELECT"),
        (Number, "1"),
        (Semicolon, ";"),
        (Word, "END"),
        (Delimiter, "//"),
        (Directive, "DELIMITER ;"),
        (Word, "SELECT"),
        (Number, "1"),
        (Semicolon, ";"),
      ]
    );
    assert_eq!(
      tokens("DELIMITER $$\nSELECT 1$$", Dialect::Generic)[3],
      (Delimiter, "$$")
    );
    // a column named delimiter isn't a command
    assert_eq!(
      tokens("CREATE TABLE t (\ndelimiter VARCHAR(10)\n)", Dialect::MySql)[4],
      (Word, "delimiter")
    );
    assert_eq!(tokens("delimiter text", Dialect::Generic)[0], (Word, "delimiter"));
    // nor is an option in the middle of a statement
    assert_eq!(
      tokens("COPY t FROM STDIN WITH (\n  DELIMITER ';'\n);", Dialect::Generic)[6],
      (Word, "DELIMITER")
    );
  }

  #[test]
  fn lexes_client_commands() {
    assert_eq!(
      tokens("\\connect db\nSELECT 1;", Dialect::PostgreSql)[0],
      (Directive, "\\connect db")
    );
    // a psql meta-command runs to the end of its line wherever it starts
    assert_eq!(
      texts("SELECT $1 \\bind 'v' \\g\n\\endpipeline", Dialect::PostgreSql),
      vec!["SELECT", "$1", "\\bind 'v' \\g", "\\endpipeline"]
    );
    assert_eq!(
      tokens(":setvar x 1\nSELECT 1", Dialect::SqlServer)[0],
      (Directive, ":setvar x 1")
    );
    assert_eq!(
      tokens("PROMPT don't stop\nSELECT 1 FROM dual;", Dialect::Oracle)[0],
      (Directive, "PROMPT don't stop")
    );
    assert_eq!(
      tokens("SET SERVEROUTPUT ON\nBEGIN NULL; END;", Dialect::Oracle)[0],
      (Directive, "SET SERVEROUTPUT ON")
    );
    assert_eq!(tokens("@script.sql", Dialect::Oracle)[0], (Directive, "@script.sql"));
    assert_eq!(tokens("SET x = 1;", Dialect::Oracle)[0], (Word, "SET"));
    // only between statements
    assert_eq!(tokens("UPDATE t\nSET define = 1;", Dialect::Oracle)[2], (Word, "SET"));
    assert_eq!(tokens("CALL p(\n@x)", Dialect::Oracle)[3], (Variable, "@x"));
  }

  #[test]
  fn lexes_copy_data() {
    let text = "COPY t (a, b) FROM stdin;\n1\t'x\n2\t)\n\\.\nSELECT 1;";
    assert_eq!(
      tokens(text, Dialect::PostgreSql)[9..],
      [
        (Semicolon, ";"),
        (Directive, "1\t'x\n2\t)\n\\."),
        (Word, "SELECT"),
        (Number, "1"),
        (Semicolon, ";"),
      ]
    );
  }

  #[test]
  fn lexes_copy_data_of_several_statements() {
    let text = "select 0\\; copy t from stdin\\; copy t from stdin\\; select 1; -- c\n1\n\\.\n2\n\\.\nselect 2;";
    let tokens = tokens(text, Dialect::PostgreSql);
    assert_eq!(tokens.iter().filter(|(kind, _)| *kind == Directive).count(), 2);
    assert_eq!(
      tokens.iter().find(|(kind, _)| *kind == LineComment),
      Some(&(LineComment, "-- c"))
    );
    assert_eq!(tokens.last(), Some(&(Semicolon, ";")));
  }

  #[test]
  fn lexes_psql_semicolon_escape() {
    assert_eq!(
      texts("copy t to stdout\\; select 1;", Dialect::PostgreSql),
      vec!["copy", "t", "to", "stdout", "\\;", "select", "1", ";"]
    );
  }

  #[test]
  fn lexes_snowflake_double_slash_comments() {
    assert_eq!(
      tokens("a // c\nb", Dialect::Snowflake),
      vec![(Word, "a"), (LineComment, "// c"), (Word, "b")]
    );
    assert_eq!(tokens("a // b", Dialect::Generic)[1], (Operator, "//"));
  }

  #[test]
  fn reports_unterminated_tokens() {
    for (text, dialect, message) in [
      ("'abc", Dialect::Generic, "unterminated string"),
      ("\"abc", Dialect::Generic, "unterminated quoted identifier"),
      ("[abc", Dialect::SqlServer, "unterminated quoted identifier"),
      ("/* abc", Dialect::Generic, "unterminated block comment"),
      ("{{ abc", Dialect::Generic, "unterminated template tag"),
      ("q'[abc'", Dialect::Oracle, "unterminated string"),
    ] {
      let error = lex(text, dialect).unwrap_err();
      assert_eq!(error.message, message, "{text}");
      assert_eq!(error.span.start, 0);
    }
  }

  #[test]
  fn is_lossless() {
    let text = "SELECT  a,\n  'x' -- c\n  /* d */ FROM [t]  ;\n";
    let tokens = lex(text, Dialect::SqlServer).unwrap();
    let mut rebuilt = std::string::String::new();
    let mut last_end = 0;
    for token in &tokens {
      assert!(text[last_end..token.start].trim().is_empty());
      rebuilt.push_str(&text[last_end..token.start]);
      rebuilt.push_str(token.text);
      last_end = token.end();
    }
    rebuilt.push_str(&text[last_end..]);
    assert_eq!(rebuilt, text);
  }
}
