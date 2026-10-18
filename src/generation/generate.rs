use std::rc::Rc;

use dprint_core::formatting::conditions::*;
use dprint_core::formatting::ir_helpers;
use dprint_core::formatting::ir_helpers::GenSeparatedValuesOptions;
use dprint_core::formatting::ir_helpers::GeneratedValue;
use dprint_core::formatting::ir_helpers::LinesSpan;
use dprint_core::formatting::ir_helpers::MultiLineOptions;
use dprint_core::formatting::ir_helpers::SingleLineOptions;
use dprint_core::formatting::*;

use super::Context;
use crate::ast::*;
use crate::configuration::UppercaseKind;
use crate::keywords;
use crate::lexer::Token;
use crate::lexer::TokenKind;

pub fn generate(list: &StatementList, context: &mut Context) -> PrintItems {
  let mut items = gen_statement_list(&list.items, context, true);
  items.push_condition(if_true(
    "endOfFileNewLine",
    Rc::new(|context| Some(context.writer_info.column_number > 0 || context.writer_info.line_number > 0)),
    Signal::NewLine.into(),
  ));
  items
}

/// The items of the top level statement list without the newline at the end of the file, which
/// formatting a range uses.
pub fn generate_items(items: &[Item], context: &mut Context) -> PrintItems {
  gen_statement_list(items, context, true)
}

// ---- statement lists ----

pub(crate) fn gen_statement_list(items: &[Item], context: &mut Context, is_top_level: bool) -> PrintItems {
  let mut result = PrintItems::new();
  for (i, item) in items.iter().enumerate() {
    if i > 0 && is_glued_to_empty_statement(&items[i - 1], item) {
      // T-SQL's `;WITH`, which guards against the statement before it missing its `;`
    } else if i > 0 {
      let mut newlines = if item.blank_line_before { 2 } else { 1 };
      // `linesBetweenQueries` separates a statement from whatever comes after it at the top level,
      // which leaves a comment introducing the next statement right above it
      if is_top_level
        && matches!(items[i - 1].kind, ItemKind::Statement(_))
        && !matches!(item.kind, ItemKind::Command(..))
      {
        newlines = newlines.max(usize::from(context.config.lines_between_queries.max(1)));
      }
      for _ in 0..newlines {
        result.push_signal(Signal::NewLine);
      }
    }
    result.extend(gen_item(item, context));
  }
  result
}

/// Whether `item` directly follows a `;` that's a statement of its own, with nothing between them.
fn is_glued_to_empty_statement(previous: &Item, item: &Item) -> bool {
  let is_empty_statement = matches!(&previous.kind, ItemKind::Statement(statement) if statement.elements.is_empty() && statement.trailing_comments.is_empty() && !statement.ignored);
  is_empty_statement
    && matches!(&item.kind, ItemKind::Statement(statement) if !statement.ignored)
    && item.first_token().is_some_and(|token| !token.space_before)
}

fn gen_item(item: &Item, context: &mut Context) -> PrintItems {
  match &item.kind {
    ItemKind::Comment(token) => gen_comment(token),
    ItemKind::Command(token, comments) => {
      let mut items = gen_raw_text(token.text.trim_end());
      for comment in comments {
        items.extend(gen_trailing_comment(comment));
      }
      items
    }
    ItemKind::Statement(statement) => gen_statement(statement, context),
  }
}

/// The statements of a block's body: a comment on the line of whatever opened the body stays on
/// that line, and the rest are indented on the lines below.
fn gen_body(list: &StatementList, context: &mut Context) -> PrintItems {
  let mut items = PrintItems::new();
  let mut rest = &list.items[..];
  while let Some(first) = rest.first() {
    match &first.kind {
      ItemKind::Comment(token) if token.newlines_before == 0 => {
        items.extend(gen_trailing_comment(token));
        rest = &rest[1..];
      }
      _ => break,
    }
  }
  if !rest.is_empty() {
    items.push_signal(Signal::NewLine);
    items.extend(ir_helpers::with_indent(gen_statement_list(rest, context, false)));
  }
  items
}

// ---- statements ----

fn gen_statement(statement: &Statement, context: &mut Context) -> PrintItems {
  if statement.ignored {
    return gen_ignored_statement(statement, context);
  }
  collect_code_bodies(statement, context);

  let mut items = gen_statement_elements(&statement.elements, context);
  if let Some(terminator) = statement.terminator {
    if terminator.kind == TokenKind::Delimiter && terminator.space_before && !statement.elements.is_empty() {
      items.extend(space_if_not_start_of_line());
    }
    items.extend(ir_helpers::gen_from_string(terminator.text));
  }
  for comment in &statement.trailing_comments {
    items.extend(gen_trailing_comment(comment));
  }
  items
}

/// A statement after a `dprint-ignore` comment, which is written exactly as it is.
fn gen_ignored_statement(statement: &Statement, context: &mut Context) -> PrintItems {
  match (statement.first_token(), statement.last_token()) {
    (Some(first), Some(last)) => gen_raw_text(&context.text[first.start..last.end()]),
    _ => PrintItems::new(),
  }
}

/// Marks the dollar-quoted strings in the statement that hold PL/pgSQL or SQL code (ex. the body
/// of a function), which are formatted too.
fn collect_code_bodies(statement: &Statement, context: &mut Context) {
  let elements = &statement.elements;
  let mut language = None;
  for (i, element) in elements.iter().enumerate() {
    if element.is_word("LANGUAGE")
      && let Some(value) = elements.get(i + 1).and_then(|e| e.as_token())
    {
      language = Some(value.text.trim_matches('\''));
    }
  }
  let is_do = elements
    .iter()
    .find(|e| !e.is_comment())
    .is_some_and(|e| e.is_word("DO"));
  let is_code = match language {
    Some(language) => ["plpgsql", "sql"].iter().any(|l| l.eq_ignore_ascii_case(language)),
    None => is_do,
  };
  if is_code {
    for element in elements {
      if let Some(token) = element.as_token().filter(|token| token.kind == TokenKind::DollarString) {
        context.code_bodies.insert(token.start);
      }
    }
  }
}

/// A statement's elements, which are runs of clauses with blocks of procedural code between them.
fn gen_statement_elements(elements: &[Element], context: &mut Context) -> PrintItems {
  let mut items = PrintItems::new();
  let mut run_start = 0;
  let mut has_written = false;
  let mut previous_was_inline_label = false;

  let separate = |items: &mut PrintItems, has_written: &mut bool, previous_was_inline_label: bool| {
    if *has_written {
      if previous_was_inline_label {
        items.push_space();
      } else {
        items.push_signal(Signal::NewLine);
      }
    }
    *has_written = true;
  };

  for (i, element) in elements.iter().enumerate() {
    let is_segment = element.is_procedural() || matches!(element, Element::Label(_));
    if !is_segment {
      continue;
    }
    if run_start < i {
      separate(&mut items, &mut has_written, previous_was_inline_label);
      let declarations_follow = matches!(element, Element::DeclareSection(section) if section.keyword.is_none());
      items.extend(gen_run(&elements[run_start..i], declarations_follow, context));
      previous_was_inline_label = false;
    }
    separate(&mut items, &mut has_written, previous_was_inline_label);
    items.extend(gen_procedural(element, context));
    previous_was_inline_label = matches!(element, Element::Label(label) if !label.own_line);
    run_start = i + 1;
  }
  if run_start < elements.len() {
    separate(&mut items, &mut has_written, previous_was_inline_label);
    items.extend(gen_run(&elements[run_start..], false, context));
  }
  items
}

/// A run of elements without procedural code, which is laid out as clauses each starting a line.
fn gen_run(elements: &[Element], declarations_follow: bool, context: &mut Context) -> PrintItems {
  let clauses = split_clauses_ex(elements, declarations_follow);
  // a query that a cursor is declared or opened for, or that's returned, is indented beneath the
  // statement's head when it doesn't fit on its line
  let first_word = elements.iter().find(|e| !e.is_comment());
  let embeds_query = first_word.is_some_and(|e| ["CURSOR", "DECLARE", "OPEN", "RETURN"].iter().any(|w| e.is_word(w)));
  let query_start = if embeds_query {
    clauses
      .iter()
      .position(|clause| clause.kind == ClauseKind::With || clause.keyword.first().is_some_and(|e| e.is_word("SELECT")))
      .filter(|index| *index > 0)
  } else {
    None
  };
  let head_count = query_start.unwrap_or(clauses.len());
  let mut items = PrintItems::new();
  for (i, clause) in clauses[..head_count].iter().enumerate() {
    if i > 0 && clause.kind != ClauseKind::InlineList {
      items.push_signal(Signal::NewLine);
    }
    items.extend(gen_clause(clause, context));
  }
  if query_start.is_some() {
    let query = &clauses[head_count..];
    let force = query.iter().any(|clause| clause.is_known_multi_line());
    items.extend(gen_clauses_fit(query, force, ClausesLayout::AfterHead, context));
  }
  items
}

// ---- procedural code ----

fn gen_procedural(element: &Element, context: &mut Context) -> PrintItems {
  match element {
    Element::Block(block) => gen_block(block, context),
    Element::DeclareSection(section) => gen_declare_section(section, context),
    Element::If(block) => gen_if(block, context),
    Element::Conditional(conditional) => gen_conditional(conditional, context),
    Element::Loop(block) => gen_loop(block, context),
    Element::CaseStatement(statement) => gen_case_statement(statement, context),
    Element::Label(label) => gen_label(label),
    _ => gen_fill(std::slice::from_ref(element), context),
  }
}

fn gen_block(block: &Block, context: &mut Context) -> PrintItems {
  let mut items = gen_keywords(&block.begin, context);
  items.extend(gen_body(&block.body, context));
  if let Some(exception) = &block.exception {
    items.push_signal(Signal::NewLine);
    items.extend(gen_keyword(exception.keyword, context));
    let mut handlers = PrintItems::new();
    for comment in &exception.leading_comments {
      handlers.push_signal(Signal::NewLine);
      handlers.extend(gen_comment(comment));
    }
    for handler in &exception.handlers {
      handlers.push_signal(Signal::NewLine);
      handlers.extend(gen_handler(handler, context));
    }
    items.extend(ir_helpers::with_indent(handlers));
  }
  items.push_signal(Signal::NewLine);
  items.extend(gen_end(&block.end, context));
  items
}

/// `WHEN condition THEN` followed by the statements of an exception handler or a procedural
/// `CASE` branch.
fn gen_handler(handler: &Handler, context: &mut Context) -> PrintItems {
  let mut items = gen_keyword(handler.when, context);
  if !handler.condition.is_empty() {
    items.push_space();
    items.extend(gen_fill(&handler.condition, context));
  }
  items.extend(space_if_not_start_of_line());
  items.extend(gen_keyword(handler.then, context));
  items.extend(gen_body(&handler.body, context));
  items
}

/// `END` and the words after it, of which only the ones saying what ends are keywords (ex. the
/// `IF` of `END IF`, but not the name of a procedure).
fn gen_end(tokens: &[Tok], context: &mut Context) -> PrintItems {
  let mut items = PrintItems::new();
  for (i, token) in tokens.iter().enumerate() {
    if i > 0 {
      items.push_space();
    }
    let is_keyword =
      i == 0 || keywords::contains(keywords::RESERVED, token.text) || token.is_word("TRY") || token.is_word("CATCH");
    items.extend(gen_word(token, is_keyword, context));
  }
  items
}

fn gen_declare_section(section: &DeclareSection, context: &mut Context) -> PrintItems {
  match section.keyword {
    Some(keyword) => {
      let mut items = gen_keyword(keyword, context);
      items.extend(gen_body(&section.declarations, context));
      items
    }
    // the declarations of an Oracle routine follow its `IS`, which ends the line before them
    None => ir_helpers::with_indent(gen_statement_list(&section.declarations.items, context, false)),
  }
}

fn gen_if(block: &IfBlock, context: &mut Context) -> PrintItems {
  let mut items = PrintItems::new();
  for (i, branch) in block.branches.iter().enumerate() {
    if i > 0 {
      items.push_signal(Signal::NewLine);
    }
    items.extend(gen_keyword(branch.keyword, context));
    items.push_space();
    items.extend(gen_conditions_hanging(&branch.condition, context));
    items.extend(space_if_not_start_of_line());
    items.extend(gen_keyword(branch.then, context));
    items.extend(gen_body(&branch.body, context));
  }
  if let Some(else_branch) = &block.else_branch {
    items.push_signal(Signal::NewLine);
    items.extend(gen_keyword(else_branch.keyword, context));
    items.extend(gen_body(&else_branch.body, context));
  }
  items.push_signal(Signal::NewLine);
  items.extend(gen_end(&block.end, context));
  items
}

/// T-SQL's `IF` and `WHILE`, whose body is a single statement. A block is written beneath the
/// condition at the same indentation, like its `BEGIN` and `END` always are, and any other
/// statement is indented beneath it.
fn gen_conditional(conditional: &Conditional, context: &mut Context) -> PrintItems {
  let mut items = gen_keyword(conditional.keyword, context);
  items.push_space();
  // the lines of a condition that doesn't fit are indented further than the statement beneath it
  items.extend(ir_helpers::with_indent(gen_conditions_hanging(
    &conditional.condition,
    context,
  )));
  items.push_signal(Signal::NewLine);
  items.extend(gen_conditional_body(&conditional.body, context));
  for comment in &conditional.else_comments {
    items.push_signal(Signal::NewLine);
    items.extend(gen_comment(comment));
  }
  if let Some((keyword, statement)) = &conditional.else_branch {
    items.push_signal(Signal::NewLine);
    items.extend(gen_keyword(keyword, context));
    let is_else_if = matches!(statement.elements.first(), Some(Element::Conditional(c)) if c.keyword.is_word("IF"));
    if is_else_if {
      items.push_space();
      items.extend(gen_statement(statement, context));
    } else {
      items.push_signal(Signal::NewLine);
      items.extend(gen_conditional_body(statement, context));
    }
  }
  items
}

fn gen_conditional_body(statement: &Statement, context: &mut Context) -> PrintItems {
  let is_block = statement
    .elements
    .iter()
    .find(|e| !e.is_comment())
    .is_some_and(|e| matches!(e, Element::Block(_)));
  let items = gen_statement(statement, context);
  if is_block {
    items
  } else {
    ir_helpers::with_indent(items)
  }
}

fn gen_loop(block: &Loop, context: &mut Context) -> PrintItems {
  let mut items = gen_loop_header(&block.header, context);
  items.extend(gen_body(&block.body, context));
  if let Some((keyword, condition)) = &block.until {
    items.push_signal(Signal::NewLine);
    items.extend(gen_keyword(keyword, context));
    items.push_space();
    items.extend(gen_conditions_hanging(condition, context));
  }
  items.push_signal(Signal::NewLine);
  items.extend(gen_end(&block.end, context));
  items
}

/// The header of a loop up to and including its `LOOP` or `DO`. A query to loop over (ex.
/// `FOR row IN SELECT ..`) stays on the line when it fits, and otherwise is indented on the
/// lines between the `IN` and the `LOOP`.
fn gen_loop_header(header: &[Element], context: &mut Context) -> PrintItems {
  let query_start = header
    .iter()
    .enumerate()
    .skip(1)
    .find(|(i, element)| element.is_word("SELECT") || (element.is_word("WITH") && *i + 1 < header.len()))
    .map(|(i, _)| i);
  let Some(query_start) = query_start.filter(|start| *start < header.len() - 1) else {
    return gen_fill(header, context);
  };
  let last = header.len() - 1;
  let mut items = gen_fill(&header[..query_start], context);
  let clauses = split_clauses(&header[query_start..last]);
  let force = clauses.iter().any(|clause| clause.is_known_multi_line());
  items.extend(gen_clauses_fit(&clauses, force, ClausesLayout::BetweenWords, context));
  items.extend(gen_fill(&header[last..], context));
  items
}

fn gen_case_statement(statement: &CaseStatement, context: &mut Context) -> PrintItems {
  let mut items = gen_keyword(statement.case_keyword, context);
  if !statement.operand.is_empty() {
    items.push_space();
    items.extend(gen_fill(&statement.operand, context));
  }
  let mut branches = PrintItems::new();
  for branch in &statement.branches {
    branches.push_signal(Signal::NewLine);
    branches.extend(gen_handler(branch, context));
  }
  if let Some(else_branch) = &statement.else_branch {
    branches.push_signal(Signal::NewLine);
    branches.extend(gen_keyword(else_branch.keyword, context));
    branches.extend(gen_body(&else_branch.body, context));
  }
  items.extend(ir_helpers::with_indent(branches));
  items.push_signal(Signal::NewLine);
  items.extend(gen_end(&statement.end, context));
  items
}

fn gen_label(label: &Label) -> PrintItems {
  let text: String = label.tokens.iter().map(|token| token.text).collect();
  ir_helpers::gen_from_string(&text)
}

// ---- clauses ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClauseKind {
  /// What comes before the first clause keyword, written as it flows.
  Head,
  /// A keyword followed by whatever is after it, written as it flows (ex. `LIMIT 10`).
  Fill,
  /// A keyword followed by a comma separated list, which is written on the keyword's line when
  /// it fits and otherwise with each item on a line of its own (ex. `SELECT a, b`).
  List,
  /// A comma separated list that continues the line of the clause before it (ex. the parameters
  /// of a T-SQL procedure).
  InlineList,
  /// A keyword followed by conditions joined by `AND` and `OR`, which go on lines of their own
  /// when they don't fit (ex. `WHERE a = 1 AND b = 2`).
  Condition,
  /// `FROM`, its tables and its joins.
  From,
  /// `WITH` and its common table expressions.
  With,
  /// `UNION`, `INTERSECT`, `EXCEPT` or `MINUS` on a line of its own.
  SetOperator,
  /// `WHEN [NOT] MATCHED .. THEN` and the action of a `MERGE`.
  MergeWhen,
  /// A keyword followed by what's after it written exactly as it is, since the whitespace in it
  /// means something (ex. the regular expression of a `MATCH_RECOGNIZE`'s `PATTERN`).
  Verbatim,
}

struct Clause<'e, 'a> {
  kind: ClauseKind,
  /// Comments on lines of their own above the clause.
  leading_comments: Vec<Tok<'a>>,
  keyword: &'e [Element<'a>],
  body: &'e [Element<'a>],
}

impl Clause<'_, '_> {
  fn is_known_multi_line(&self) -> bool {
    !self.leading_comments.is_empty() || is_known_multi_line(self.keyword) || is_known_multi_line(self.body)
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
  Query,
  Insert,
  Update,
  Delete,
  Merge,
  /// Creates a procedure, function, trigger or package.
  Routine,
  AlterTable,
  Declare,
  Exec,
  /// The clauses of a window (ex. `OVER (PARTITION BY a ORDER BY b)`).
  Window,
  Other,
}

/// Splits a run of elements into clauses, each of which starts a line when the run is written over
/// several lines.
fn split_clauses<'e, 'a>(elements: &'e [Element<'a>]) -> Vec<Clause<'e, 'a>> {
  split_clauses_ex(elements, false)
}

/// Like `split_clauses`, where `declarations_follow` says the declarations of an Oracle routine
/// come next.
fn split_clauses_ex<'e, 'a>(elements: &'e [Element<'a>], declarations_follow: bool) -> Vec<Clause<'e, 'a>> {
  let mode = detect_mode(elements);
  let mut scan = ClauseScan {
    mode,
    query: mode == Mode::Query,
    select_list: false,
    insert_body: false,
    set_seen: false,
    using_seen: false,
    conflict_seen: false,
    body_started: false,
    params_seen: false,
    referencing_seen: false,
    query_started: false,
    declarations_follow,
    last_kind: None,
    returns_seen: false,
    dml_start: elements.iter().position(|e| !e.is_comment()).unwrap_or(0),
  };
  // (start, keyword length, kind)
  let mut starts: Vec<(usize, usize, ClauseKind)> = vec![(0, 0, ClauseKind::Head)];
  if let Some(start) = scan.special_head_end(elements) {
    starts.push((start, 0, ClauseKind::InlineList));
  }
  let mut i = 0;
  while i < elements.len() {
    if starts
      .last()
      .is_some_and(|(start, _, kind)| *kind == ClauseKind::InlineList && *start == i)
    {
      // the list runs to the end, apart from a routine's clauses after its parameters
      if scan.mode != Mode::Routine {
        break;
      }
    }
    if let Some((kind, len)) = scan.match_clause(elements, i) {
      if i == 0 {
        starts[0] = (0, len, kind);
      } else {
        starts.push((i, len, kind));
      }
      i += len.max(1);
      continue;
    }
    if let Some(start) = scan.match_params(elements, i) {
      starts.push((start, 0, ClauseKind::InlineList));
    }
    i += 1;
  }

  let mut clauses: Vec<Clause> = Vec::with_capacity(starts.len());
  let mut pending: Vec<Tok<'a>> = Vec::new();
  for (index, (start, keyword_len, kind)) in starts.iter().enumerate() {
    let end = starts
      .get(index + 1)
      .map(|(start, _, _)| *start)
      .unwrap_or(elements.len());
    let keyword = &elements[*start..start + keyword_len];
    let mut body = &elements[start + keyword_len..end];
    let leading_comments = std::mem::take(&mut pending);
    // comments on lines of their own at the end of a clause introduce the next one
    if let Some((_, _, next_kind)) = starts.get(index + 1)
      && *kind != ClauseKind::InlineList
      && *next_kind != ClauseKind::InlineList
    {
      let split = own_line_comments_start(body);
      pending = body[split..].iter().filter_map(|e| e.as_token()).collect();
      body = &body[..split];
    }
    clauses.push(Clause {
      kind: *kind,
      leading_comments,
      keyword,
      body,
    });
  }
  clauses.retain(|clause| !(clause.keyword.is_empty() && clause.body.is_empty() && clause.leading_comments.is_empty()));
  clauses
}

fn detect_mode(elements: &[Element]) -> Mode {
  let mut significant = elements.iter().filter(|e| !e.is_comment());
  let Some(first) = significant.next() else {
    return Mode::Other;
  };
  if let Element::Group(group) = first {
    return if is_query_start(&group.elements) {
      Mode::Query
    } else {
      Mode::Other
    };
  }
  let Some(token) = first.as_token().filter(|token| token.kind == TokenKind::Word) else {
    return Mode::Other;
  };
  let is = |word: &str| token.is_word(word);
  let second = significant.next();
  if is("SELECT") || is("WITH") || is("VALUES") || is("PERFORM") {
    Mode::Query
  } else if is("INSERT") || is("UPSERT") || (is("REPLACE") && !matches!(second, Some(Element::Group(_)))) {
    Mode::Insert
  } else if is("UPDATE") {
    Mode::Update
  } else if is("DELETE") {
    Mode::Delete
  } else if is("MERGE") {
    Mode::Merge
  } else if is("PARTITION") || is("ORDER") || is("ROWS") || is("RANGE") || is("GROUPS") || is("MEASURES") {
    Mode::Window
  } else if (is("CREATE") || is("ALTER"))
    && elements.iter().filter(|e| !e.is_comment()).take(8).any(|e| {
      ["PROCEDURE", "PROC", "FUNCTION", "TRIGGER", "PACKAGE", "BODY", "EVENT"]
        .iter()
        .any(|w| e.is_word(w))
    })
  {
    Mode::Routine
  } else if is("ALTER") && second.is_some_and(|e| e.is_word("TABLE")) {
    Mode::AlterTable
  } else if is("DECLARE")
    && !elements
      .iter()
      .any(|e| e.is_word("CURSOR") || e.is_word("HANDLER") || e.is_word("CONDITION"))
  {
    Mode::Declare
  } else if is("EXEC") || is("EXECUTE") {
    Mode::Exec
  } else if is("PROCEDURE")
    || is("FUNCTION")
    || (["MEMBER", "STATIC", "CONSTRUCTOR", "MAP", "ORDER", "OVERRIDING", "FINAL"]
      .iter()
      .any(|w| is(w))
      && elements
        .iter()
        .filter(|e| !e.is_comment())
        .take(3)
        .any(|e| e.is_word("FUNCTION") || e.is_word("PROCEDURE")))
  {
    // a routine in an Oracle package or type body
    Mode::Routine
  } else {
    Mode::Other
  }
}

struct ClauseScan {
  mode: Mode,
  /// Whether the clauses of a query (ex. `FROM` and `WHERE`) are recognized.
  query: bool,
  /// Whether the last clause is the list of a `SELECT`, which an `INTO` may follow.
  select_list: bool,
  /// Whether what's inserted has started (ex. its `VALUES` or `SELECT`).
  insert_body: bool,
  set_seen: bool,
  using_seen: bool,
  conflict_seen: bool,
  /// Whether a routine's `AS` or `IS` has been seen.
  body_started: bool,
  /// Whether a T-SQL routine's parameters have been seen.
  params_seen: bool,
  /// Whether a trigger's `REFERENCING` has been seen, which names its old and new rows.
  referencing_seen: bool,
  /// Whether a `SELECT` or `VALUES` has started a query.
  query_started: bool,
  /// Whether the declarations of an Oracle routine follow the elements.
  declarations_follow: bool,
  /// The kind of the last clause that was matched.
  last_kind: Option<ClauseKind>,
  /// Whether a routine's `RETURNS` has been seen.
  returns_seen: bool,
  /// Where the statement's `INSERT`, `UPDATE`, `DELETE` or `MERGE` is, which follows its common
  /// table expressions when it has any.
  dml_start: usize,
}

impl ClauseScan {
  /// Where the comma separated list starts in a statement whose head is followed by one, which
  /// continues the line of the head: the actions of an `ALTER TABLE` and the arguments of an
  /// `EXEC`.
  fn special_head_end(&mut self, elements: &[Element]) -> Option<usize> {
    let mut i = elements.iter().position(|e| !e.is_comment())?;
    match self.mode {
      Mode::AlterTable => {
        i += 2;
        while word_at(elements, i)
          .is_some_and(|t| t.is_word("IF") || t.is_word("NOT") || t.is_word("EXISTS") || t.is_word("ONLY"))
        {
          i += 1;
        }
        i = skip_name(elements, i)?;
        if elements
          .get(i)
          .is_some_and(|e| e.as_token().is_some_and(|t| t.is_operator("*")))
        {
          i += 1;
        }
      }
      Mode::Exec => {
        i += 1;
        // `EXEC @result = name`
        if elements.get(i).is_some_and(|e| e.is_kind(TokenKind::Variable))
          && elements
            .get(i + 1)
            .is_some_and(|e| e.as_token().is_some_and(|t| t.is_operator("=")))
        {
          i += 2;
        }
        i = skip_name(elements, i)?;
      }
      _ => return None,
    }
    let rest = elements.get(i..)?;
    let starts_list = rest
      .iter()
      .find(|e| !e.is_comment())
      .is_some_and(|e| !matches!(e, Element::Group(_)) && !e.is_kind(TokenKind::Semicolon));
    starts_list.then_some(i)
  }

  /// Where the parameters of a T-SQL routine start, which aren't in brackets.
  fn match_params(&mut self, elements: &[Element], i: usize) -> Option<usize> {
    if self.mode != Mode::Routine || self.params_seen || self.body_started || !elements[i].is_kind(TokenKind::Variable)
    {
      return None;
    }
    // the parameters follow the routine's name
    let kind_index = elements[..i]
      .iter()
      .rposition(|e| e.is_word("PROCEDURE") || e.is_word("PROC") || e.is_word("FUNCTION"))?;
    if skip_name(elements, kind_index + 1) != Some(i) {
      return None;
    }
    self.params_seen = true;
    Some(i)
  }

  fn match_clause(&mut self, elements: &[Element], i: usize) -> Option<(ClauseKind, usize)> {
    let token = word_at(elements, i)?;
    let previous = prev_significant(elements, i);
    if previous.is_some_and(|e| e.is_kind(TokenKind::Dot))
      || elements.get(i + 1).is_some_and(|e| e.is_kind(TokenKind::Dot))
    {
      return None;
    }
    let prev_word = previous
      .and_then(|e| e.as_token())
      .filter(|t| t.kind == TokenKind::Word);
    let words = ClauseWords {
      elements,
      i,
      token,
      prev_word,
    };
    let result = self.match_clause_words(&words);
    if let Some((kind, _)) = result {
      self.select_list = kind == ClauseKind::List && (token.is_word("SELECT") || token.is_word("PERFORM"));
      self.last_kind = Some(kind);
    }
    result
  }

  fn match_clause_words(&mut self, words: &ClauseWords) -> Option<(ClauseKind, usize)> {
    use ClauseKind::*;
    let elements = words.elements;
    let i = words.i;
    let is = |word: &str| words.token.is_word(word);
    let next_is = |offset: usize, word: &str| words.next_is(offset, word);
    let prev_is = |candidates: &[&str]| words.prev_is(candidates);

    if self.mode == Mode::Window {
      if (is("PARTITION") || is("ORDER")) && next_is(1, "BY") {
        return Some((List, 2));
      }
      if is("ROWS") || is("RANGE") || is("GROUPS") {
        return Some((Fill, 1));
      }
      // the clauses of a `MATCH_RECOGNIZE`
      if is("MEASURES") || is("DEFINE") || is("SUBSET") {
        return Some((List, 1));
      }
      if (is("ONE") || is("ALL")) && (next_is(1, "ROW") || next_is(1, "ROWS")) && next_is(2, "PER") {
        return Some((Fill, 1));
      }
      if is("AFTER") && next_is(1, "MATCH") {
        return Some((Fill, 2));
      }
      if is("PATTERN") {
        return Some((Verbatim, 1));
      }
      return None;
    }

    // set operators, unless it's BigQuery's `SELECT * EXCEPT (column)`
    if self.query && (is("UNION") || is("INTERSECT") || is("EXCEPT") || is("MINUS")) {
      let mut len = 1;
      if next_is(1, "ALL") || next_is(1, "DISTINCT") {
        len += 1;
      }
      return is_query_start(&elements[i + len..]).then_some((SetOperator, len));
    }

    // the statement after common table expressions (ex. `WITH a AS (..) UPDATE t SET ..`)
    if self.mode == Mode::Query && self.last_kind == Some(With) {
      let mode = if is("INSERT") || is("UPSERT") || is("REPLACE") {
        Some(Mode::Insert)
      } else if is("UPDATE") {
        Some(Mode::Update)
      } else if is("DELETE") {
        Some(Mode::Delete)
      } else if is("MERGE") {
        Some(Mode::Merge)
      } else {
        None
      };
      if let Some(mode) = mode {
        self.mode = mode;
        self.query = false;
        self.insert_body = false;
        self.dml_start = i;
        return Some((Fill, 1));
      }
    }

    if is("SELECT") || (i == 0 && is("PERFORM")) {
      // PostgreSQL's `ON CONFLICT .. DO SELECT`, which isn't a query
      if prev_is(&["DO"]) && self.conflict_seen {
        return None;
      }
      // a second query on the line of the first isn't one of its clauses, since once it starts a
      // line the generic dialect reads it as a statement of its own
      if self.query_started
        && !words.token.starts_line()
        && !prev_is(&["UNION", "ALL", "DISTINCT", "INTERSECT", "EXCEPT", "MINUS"])
      {
        return None;
      }
      let allowed = match self.mode {
        Mode::Other | Mode::Routine | Mode::Declare | Mode::Exec | Mode::AlterTable => {
          i == 0 || self.query || prev_is(&["AS", "IS", "QUERY", "EXPLAIN", "ANALYZE", "VERBOSE", "RETURN", "FOR"])
        }
        _ => true,
      };
      if !allowed {
        return None;
      }
      self.query = true;
      self.insert_body = true;
      self.query_started = true;
      return Some((List, 1 + select_modifiers_len(elements, i + 1)));
    }
    if is("WITH") && is_cte_start(elements, i) {
      let allowed = i == 0
        || (self.mode == Mode::Insert && !self.insert_body)
        || prev_is(&["AS", "IS", "QUERY", "EXPLAIN", "ANALYZE", "VERBOSE"]);
      if allowed {
        self.query = true;
        self.insert_body = true;
        return Some((With, if next_is(1, "RECURSIVE") { 2 } else { 1 }));
      }
    }
    if is("VALUES") {
      let inserting = self.mode == Mode::Insert && !self.insert_body && !prev_is(&["DEFAULT"]);
      // MySQL's `VALUES(column)` function, as opposed to the clause
      let is_call =
        matches!(elements.get(i + 1), Some(Element::Group(group)) if group.is_paren() && !group.open.space_before);
      if inserting || (!is_call && (i == 0 || prev_is(&["AS"]))) {
        self.query = true;
        self.insert_body = true;
        self.query_started = true;
        return Some((List, 1));
      }
    }

    let is_dml = matches!(self.mode, Mode::Insert | Mode::Update | Mode::Delete | Mode::Merge);
    if is("INTO") && self.select_list {
      return Some((Fill, 1));
    }
    if is("BULK") && next_is(1, "COLLECT") && next_is(2, "INTO") && self.select_list {
      return Some((Fill, 3));
    }
    if is("FROM") && !prev_is(&["DISTINCT"]) && (self.query || matches!(self.mode, Mode::Update | Mode::Delete)) {
      // `DELETE FROM` names the table that's deleted from
      let names_target = self.mode == Mode::Delete
        && !self.query
        && elements[self.dml_start..i].iter().filter(|e| !e.is_comment()).all(|e| {
          e.is_word("DELETE")
            || e.is_word("TOP")
            || e.is_word("LOW_PRIORITY")
            || e.is_word("QUICK")
            || e.is_word("IGNORE")
            || matches!(e, Element::Group(_))
            || e.is_kind(TokenKind::Number)
        });
      if !names_target {
        return Some((From, 1));
      }
    }
    if is("WHERE")
      && (self.query
        || matches!(self.mode, Mode::Update | Mode::Delete)
        || (self.mode == Mode::Insert && self.conflict_seen))
    {
      return Some((Condition, 1));
    }
    if self.query || matches!(self.mode, Mode::Update | Mode::Delete) {
      if is("ORDER") && next_is(1, "BY") {
        return Some((List, 2));
      }
      if is("ORDER") && next_is(1, "SIBLINGS") && next_is(2, "BY") {
        return Some((List, 3));
      }
      if is("LIMIT") {
        return Some((Fill, 1));
      }
    }
    if self.query {
      if is("GROUP") && next_is(1, "BY") {
        return Some((List, 2));
      }
      if is("HAVING") || is("QUALIFY") {
        return Some((Condition, 1));
      }
      if is("WINDOW") && word_at(elements, i + 1).is_some() && next_is(2, "AS") {
        return Some((List, 1));
      }
      if is("OFFSET") && !prev_is(&["WITH"]) {
        return Some((Fill, 1));
      }
      if is("FETCH") && (next_is(1, "FIRST") || next_is(1, "NEXT")) {
        return Some((Fill, 2));
      }
      if is("FOR")
        && ["UPDATE", "SHARE", "NO", "KEY", "XML", "JSON", "BROWSE", "READ"]
          .iter()
          .any(|w| next_is(1, w))
      {
        return Some((Fill, 1));
      }
      if is("LOCK") && next_is(1, "IN") {
        return Some((Fill, 1));
      }
      if is("OPTION") && matches!(elements.get(i + 1), Some(Element::Group(_))) {
        return Some((Fill, 1));
      }
      if (is("CONNECT") && next_is(1, "BY")) || (is("START") && next_is(1, "WITH")) {
        return Some((Condition, 2));
      }
    }
    match self.mode {
      Mode::Update if is("SET") && !self.set_seen => {
        self.set_seen = true;
        return Some((List, 1));
      }
      Mode::Insert => {
        if is("SET") && (!self.insert_body || (self.conflict_seen && prev_is(&["UPDATE"]))) {
          self.insert_body = true;
          return Some((List, 1));
        }
        if is("ON") && next_is(1, "CONFLICT") {
          self.conflict_seen = true;
          return Some((Fill, 2));
        }
        if is("ON") && next_is(1, "DUPLICATE") && next_is(2, "KEY") && next_is(3, "UPDATE") {
          return Some((List, 4));
        }
      }
      Mode::Delete if is("USING") => return Some((Fill, 1)),
      Mode::Merge => {
        if is("USING") && !self.using_seen {
          self.using_seen = true;
          return Some((Fill, 1));
        }
        if is("ON") && self.using_seen {
          return Some((Condition, 1));
        }
        if is("WHEN") && (next_is(1, "MATCHED") || (next_is(1, "NOT") && next_is(2, "MATCHED"))) {
          return Some((MergeWhen, 1));
        }
      }
      Mode::Routine => {
        if is("LANGUAGE") && word_or_string_at(elements, i + 1) {
          return Some((Fill, 1));
        }
        // the body of a SQL function in PostgreSQL and Databricks (ex. `RETURNS int RETURN a + b`),
        // as opposed to the type that an Oracle function returns
        if is("RETURN") && self.returns_seen && !self.body_started {
          self.body_started = true;
          return Some((Fill, 1));
        }
        if !self.body_started {
          if is("RETURNS") {
            self.returns_seen = true;
            return Some((Fill, 1));
          }
          if is("WITH") && self.params_seen {
            return Some((Fill, 1));
          }
          // Oracle's `IS` ends the line of the header, as does an `AS` that its declarations follow
          if is("IS") {
            self.body_started = true;
            return None;
          }
          if is("REFERENCING") {
            self.referencing_seen = true;
          }
          // as opposed to `EXECUTE AS` or a trigger's `REFERENCING OLD TABLE AS name`
          if is("AS") && !prev_is(&["EXECUTE"]) && !(self.referencing_seen && prev_is(&["TABLE", "ROW", "OLD", "NEW"]))
          {
            self.body_started = true;
            if i + 1 == elements.len() && self.declarations_follow {
              return None;
            }
            return Some((Fill, 1));
          }
        }
      }
      Mode::Declare if i == 0 => return Some((List, 1)),
      _ => {}
    }
    if is_dml && (is("RETURNING") || is("OUTPUT")) {
      return Some((List, 1));
    }
    None
  }
}

/// The word that may start a clause and what's around it.
struct ClauseWords<'e, 'a> {
  elements: &'e [Element<'a>],
  i: usize,
  token: Tok<'a>,
  prev_word: Option<Tok<'a>>,
}

impl ClauseWords<'_, '_> {
  fn next_is(&self, offset: usize, word: &str) -> bool {
    word_at(self.elements, self.i + offset).is_some_and(|t| t.is_word(word))
  }

  fn prev_is(&self, words: &[&str]) -> bool {
    self.prev_word.is_some_and(|t| words.iter().any(|w| t.is_word(w)))
  }
}

/// How many of the elements after a `SELECT` modify it (ex. `DISTINCT`, `TOP (10)`), which are
/// written as part of its keyword.
fn select_modifiers_len(elements: &[Element], start: usize) -> usize {
  let mut i = start;
  while let Some(token) = word_at(elements, i) {
    if token.is_word("DISTINCT") {
      i += 1;
      if word_at(elements, i).is_some_and(|t| t.is_word("ON")) && matches!(elements.get(i + 1), Some(Element::Group(_)))
      {
        i += 2;
      }
    } else if token.is_word("TOP") {
      i += 1;
      if elements.get(i).is_some_and(|e| {
        matches!(e, Element::Group(_)) || e.is_kind(TokenKind::Number) || e.is_kind(TokenKind::Variable)
      }) {
        i += 1;
      }
      if word_at(elements, i).is_some_and(|t| t.is_word("PERCENT")) {
        i += 1;
      }
      if word_at(elements, i).is_some_and(|t| t.is_word("WITH"))
        && word_at(elements, i + 1).is_some_and(|t| t.is_word("TIES"))
      {
        i += 2;
      }
    } else if token.is_word("AS") && word_at(elements, i + 1).is_some_and(|t| t.is_word("STRUCT") || t.is_word("VALUE"))
    {
      i += 2;
    } else if [
      "ALL",
      "DISTINCTROW",
      "HIGH_PRIORITY",
      "STRAIGHT_JOIN",
      "SQL_SMALL_RESULT",
      "SQL_BIG_RESULT",
      "SQL_BUFFER_RESULT",
      "SQL_CACHE",
      "SQL_NO_CACHE",
      "SQL_CALC_FOUND_ROWS",
    ]
    .iter()
    .any(|w| token.is_word(w))
    {
      i += 1;
    } else {
      break;
    }
  }
  i - start
}

/// Where a name made of parts separated by dots (ex. `schema.table`) that starts at `start` ends.
fn skip_name(elements: &[Element], start: usize) -> Option<usize> {
  let is_part = |e: &Element| {
    e.as_token().is_some_and(|t| {
      matches!(
        t.kind,
        TokenKind::Word | TokenKind::QuotedIdent | TokenKind::Variable | TokenKind::Template
      )
    })
  };
  if !elements.get(start).is_some_and(is_part) {
    return None;
  }
  let mut i = start + 1;
  while elements.get(i).is_some_and(|e| e.is_kind(TokenKind::Dot)) && elements.get(i + 1).is_some_and(is_part) {
    i += 2;
  }
  Some(i)
}

fn gen_clause(clause: &Clause, context: &mut Context) -> PrintItems {
  let mut items = PrintItems::new();
  let first_token = clause.keyword.first().or(clause.body.first()).map(|e| e.first_token());
  items.extend(gen_leading_comments(&clause.leading_comments, first_token));
  let mut body = clause.body;
  let mut keyword_comment_ends_line = false;
  if !clause.keyword.is_empty() {
    items.extend(gen_fill_ex(clause.keyword, context, true));
    // a comment on the keyword's line stays there
    while let Some(token) = body
      .first()
      .and_then(|e| e.as_token())
      .filter(|t| t.is_comment() && t.newlines_before == 0)
    {
      items.extend(gen_trailing_comment(token));
      keyword_comment_ends_line |= token.is_line_comment();
      body = &body[1..];
    }
  }
  if body.is_empty() {
    return items;
  }
  let needs_space = !clause.keyword.is_empty() && !keyword_comment_ends_line;
  match clause.kind {
    ClauseKind::Head => items.extend(gen_fill(body, context)),
    ClauseKind::Fill | ClauseKind::Condition | ClauseKind::With | ClauseKind::MergeWhen => {
      if needs_space {
        items.push_space();
      }
      items.extend(match clause.kind {
        ClauseKind::Fill => gen_fill(body, context),
        ClauseKind::Condition => gen_conditions_hanging(body, context),
        ClauseKind::With => gen_with(body, context),
        _ => gen_merge_when(body, context),
      });
    }
    ClauseKind::List | ClauseKind::InlineList => items.extend(gen_list(
      body,
      ListStyle::AfterKeyword,
      keyword_comment_ends_line,
      context,
    )),
    ClauseKind::From => items.extend(gen_from(body, keyword_comment_ends_line, context)),
    ClauseKind::Verbatim => {
      if needs_space {
        items.push_space();
      }
      let start = body[0].first_token().start;
      let end = body[body.len() - 1].last_token().end();
      items.extend(gen_raw_text(&context.text[start..end]));
    }
    ClauseKind::SetOperator => {
      items.push_signal(Signal::NewLine);
      items.extend(gen_fill(body, context));
    }
  }
  items
}

/// Where clauses that are written on one line when they fit are, which decides how they're
/// written when they don't.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClausesLayout {
  /// In brackets (ex. a subquery), with the clauses indented on the lines between them.
  InBrackets,
  /// Between the words of a loop's header (ex. `FOR r IN .. LOOP`), with the clauses indented on
  /// the lines between them.
  BetweenWords,
  /// After the head of a statement (ex. `CURSOR c IS ..`), with the clauses indented on the
  /// lines below it.
  AfterHead,
}

/// Clauses that are written on one line when they fit and otherwise on a line each.
fn gen_clauses_fit(clauses: &[Clause], force: bool, layout: ClausesLayout, context: &mut Context) -> PrintItems {
  let indent_width = context.config.indent_width;
  ir_helpers::gen_separated_values(
    |_| {
      clauses
        .iter()
        .map(|clause| GeneratedValue {
          items: gen_clause(clause, context),
          lines_span: None,
          allow_inline_multi_line: false,
          allow_inline_single_line: false,
          is_known_multi_line: clause.is_known_multi_line(),
        })
        .collect()
    },
    GenSeparatedValuesOptions {
      prefer_hanging: false,
      force_use_new_lines: force,
      allow_blank_lines: false,
      indent_width,
      single_line_options: match layout {
        ClausesLayout::InBrackets => SingleLineOptions::separated_same_line(Signal::SpaceOrNewLine.into()),
        ClausesLayout::BetweenWords => SingleLineOptions::surrounded_line(),
        ClausesLayout::AfterHead => SingleLineOptions::separated_line_starting_with_space(),
      },
      multi_line_options: match layout {
        ClausesLayout::InBrackets | ClausesLayout::BetweenWords => MultiLineOptions::surround_newlines_indented(),
        ClausesLayout::AfterHead => MultiLineOptions::new_line_start(),
      },
      force_possible_newline_at_start: false,
    },
  )
  .items
}

// ---- lists ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListStyle {
  /// After a keyword (ex. `SELECT a, b`), with the items indented on the lines below it when they
  /// don't fit.
  AfterKeyword,
  /// Within brackets, with the items indented on the lines between them when they don't fit.
  InBrackets,
}

struct ListItem<'e, 'a> {
  /// Comments on lines of their own before the item.
  leading_comments: Vec<Tok<'a>>,
  elements: &'e [Element<'a>],
  comma: Option<Tok<'a>>,
  /// Comments on the item's last line.
  trailing_comments: Vec<Tok<'a>>,
  /// Comments on lines of their own after the last item.
  after_comments: Vec<Tok<'a>>,
  blank_line_before: bool,
}

impl ListItem<'_, '_> {
  fn has_trailing_line_comment(&self) -> bool {
    self.trailing_comments.iter().any(|c| c.is_line_comment())
  }

  /// Whether the item is written over several lines however the list is written. A line comment
  /// at the end of the last item of a list after a keyword doesn't count, since the line ends
  /// there anyway.
  fn is_known_multi_line(&self, is_last: bool, style: ListStyle) -> bool {
    !self.leading_comments.is_empty()
      || !self.after_comments.is_empty()
      || (self.has_trailing_line_comment() && !(is_last && style == ListStyle::AfterKeyword))
      || is_known_multi_line(self.elements)
  }
}

/// Splits a comma separated list into its items, giving each the comments around it.
fn split_list<'e, 'a>(elements: &'e [Element<'a>]) -> Vec<ListItem<'e, 'a>> {
  let mut segments: Vec<(&'e [Element<'a>], Option<Tok<'a>>)> = Vec::new();
  let mut start = 0;
  let angle_types = angle_bracket_types(elements);
  for (i, element) in elements.iter().enumerate() {
    if angle_types.iter().any(|range| range.contains(&i)) {
      continue;
    }
    if let Some(token) = element.as_token().filter(|t| t.kind == TokenKind::Comma) {
      segments.push((&elements[start..i], Some(token)));
      start = i + 1;
    }
  }
  segments.push((&elements[start..], None));

  let mut items: Vec<ListItem> = Vec::with_capacity(segments.len());
  let mut carried: Vec<Tok> = Vec::new();
  let count = segments.len();
  for (index, (segment, comma)) in segments.into_iter().enumerate() {
    let mut segment = segment;
    // line comments on the line of the previous item's comma belong to it
    if let Some(previous) = items.last_mut() {
      while let Some(token) = segment
        .first()
        .and_then(|e| e.as_token())
        .filter(|t| t.is_line_comment() && t.newlines_before == 0)
      {
        previous.trailing_comments.push(token);
        segment = &segment[1..];
      }
    }
    let mut leading_comments = std::mem::take(&mut carried);
    while let Some(token) = segment
      .first()
      .and_then(|e| e.as_token())
      .filter(|t| t.is_comment() && t.newlines_before > 0)
    {
      leading_comments.push(token);
      segment = &segment[1..];
    }
    // comments at the end: the line comments on the item's last line trail it, after its comma,
    // and the comments on lines of their own introduce the next item
    let comments_start = segment.len()
      - segment
        .iter()
        .rev()
        .take_while(|e| {
          e.as_token()
            .is_some_and(|t| t.is_line_comment() || (t.is_comment() && t.newlines_before > 0))
        })
        .count();
    let mut trailing_comments = Vec::new();
    let mut after_comments = Vec::new();
    for element in &segment[comments_start..] {
      let token = element.as_token().unwrap();
      if token.newlines_before > 0 || !after_comments.is_empty() {
        after_comments.push(token);
      } else {
        trailing_comments.push(token);
      }
    }
    let content = &segment[..comments_start];
    if index + 1 < count {
      carried = std::mem::take(&mut after_comments);
    }
    if content.is_empty()
      && comma.is_none()
      && leading_comments.is_empty()
      && trailing_comments.is_empty()
      && after_comments.is_empty()
    {
      // nothing after a trailing comma
      continue;
    }
    let blank_line_before = leading_comments
      .first()
      .map(|c| c.newlines_before >= 2)
      .or_else(|| content.first().map(|e| e.first_token().newlines_before >= 2))
      .unwrap_or(false);
    items.push(ListItem {
      leading_comments,
      elements: content,
      comma,
      trailing_comments,
      after_comments,
      blank_line_before,
    });
  }
  if !carried.is_empty()
    && let Some(last) = items.last_mut()
  {
    last.after_comments.extend(carried);
  }
  items
}

fn gen_list(elements: &[Element], style: ListStyle, force: bool, context: &mut Context) -> PrintItems {
  let items = split_list(elements);
  if items.is_empty() {
    return PrintItems::new();
  }
  let count = items.len();
  let starts_with_case = matches!(items[0].elements.first(), Some(Element::Case(_)));
  if style == ListStyle::AfterKeyword
    && count == 1
    && !force
    && items[0].leading_comments.is_empty()
    && !starts_with_case
  {
    // a lone item stays against its keyword, however it's written: moving it to the next line
    // only when it's too long would leave whatever breaks within it deciding the line it starts
    // on, which the next time it's formatted would decide differently
    let mut result = PrintItems::new();
    result.push_space();
    result.extend(gen_list_item(&items[0], context));
    return result;
  }
  let force = force
    || items.iter().enumerate().any(|(i, item)| {
      (i + 1 < count && item.has_trailing_line_comment())
        || !item.leading_comments.is_empty()
        || !item.after_comments.is_empty()
    })
    || (style == ListStyle::InBrackets && items.last().is_some_and(|item| item.has_trailing_line_comment()));
  let indent_width = context.config.indent_width;
  // a string spanning lines doesn't break up the list it's in
  let has_multi_line_text_item = items.iter().any(|item| is_multi_line_text_item(item.elements));
  // nor do brackets that are the only item, which stay against what's before them (ex. `FROM (`)
  let is_lone_group = count == 1 && items[0].leading_comments.is_empty() && lone_group(items[0].elements).is_some();
  ir_helpers::gen_separated_values(
    |_| {
      let mut line = 0;
      let mut values = Vec::with_capacity(count);
      for (i, item) in items.iter().enumerate() {
        if i > 0 {
          line += if item.blank_line_before { 2 } else { 1 };
        }
        let is_known_multi_line = item.is_known_multi_line(i + 1 == count, style);
        let item_items = gen_list_item(item, context);
        values.push(GeneratedValue {
          // a lone item in brackets breaks within its brackets before the list breaks before it
          items: if is_lone_group {
            item_items
          } else {
            ir_helpers::new_line_group(item_items)
          },
          lines_span: Some(LinesSpan {
            start_line: line,
            end_line: line,
          }),
          allow_inline_multi_line: is_lone_group || is_multi_line_text_item(item.elements),
          allow_inline_single_line: has_multi_line_text_item,
          is_known_multi_line,
        });
      }
      values
    },
    GenSeparatedValuesOptions {
      prefer_hanging: false,
      force_use_new_lines: force,
      allow_blank_lines: style == ListStyle::InBrackets,
      indent_width,
      single_line_options: match style {
        ListStyle::AfterKeyword => SingleLineOptions::separated_line_starting_with_space(),
        ListStyle::InBrackets => SingleLineOptions::separated_same_line(Signal::SpaceOrNewLine.into()),
      },
      multi_line_options: match style {
        ListStyle::AfterKeyword => MultiLineOptions::new_line_start(),
        ListStyle::InBrackets => MultiLineOptions::surround_newlines_indented(),
      },
      force_possible_newline_at_start: false,
    },
  )
  .items
}

fn gen_list_item(item: &ListItem, context: &mut Context) -> PrintItems {
  let mut items = PrintItems::new();
  items.extend(gen_leading_comments(
    &item.leading_comments,
    item.elements.first().map(|e| e.first_token()),
  ));
  items.extend(gen_fill(item.elements, context));
  if item.comma.is_some() {
    items.push_str(",");
  }
  for comment in &item.trailing_comments {
    items.extend(gen_trailing_comment(comment));
  }
  for comment in &item.after_comments {
    items.push_signal(Signal::NewLine);
    items.extend(gen_comment(comment));
  }
  items
}

// ---- conditions ----

struct ConditionPart<'e, 'a> {
  leading_comments: Vec<Tok<'a>>,
  /// The `AND` or `OR` before the condition, or the `ON` of a join.
  operator: Option<Tok<'a>>,
  elements: &'e [Element<'a>],
}

impl ConditionPart<'_, '_> {
  /// Whether the condition is written over several lines however the conditions are written. A
  /// line comment at the end of the last condition doesn't count, since the line ends there anyway.
  fn is_known_multi_line(&self, is_last: bool) -> bool {
    let elements = if is_last {
      without_trailing_comments(self.elements)
    } else {
      self.elements
    };
    !self.leading_comments.is_empty() || is_known_multi_line(elements)
  }
}

/// Splits conditions at the `AND` and `OR` between them, which doesn't include the `AND` of a
/// `BETWEEN`.
fn split_conditions<'e, 'a>(elements: &'e [Element<'a>]) -> Vec<ConditionPart<'e, 'a>> {
  let mut parts = Vec::new();
  let mut start = 0;
  let mut operator = None;
  let mut in_between = false;
  for (i, element) in elements.iter().enumerate() {
    let Some(token) = element.as_token().filter(|t| t.kind == TokenKind::Word) else {
      continue;
    };
    if token.is_word("BETWEEN") {
      in_between = true;
    } else if token.is_word("AND") && in_between {
      in_between = false;
    } else if (token.is_word("AND") || token.is_word("OR")) && i > start {
      parts.push((operator, &elements[start..i]));
      operator = Some(token);
      start = i + 1;
    }
  }
  parts.push((operator, &elements[start..]));

  let mut result: Vec<ConditionPart> = Vec::with_capacity(parts.len());
  let mut carried = Vec::new();
  let count = parts.len();
  for (index, (operator, part)) in parts.into_iter().enumerate() {
    let mut part = part;
    let leading_comments = std::mem::take(&mut carried);
    if index + 1 < count {
      // comments on lines of their own before an `AND` introduce the condition after it
      let split = own_line_comments_start(part);
      carried = part[split..].iter().filter_map(|e| e.as_token()).collect();
      part = &part[..split];
    }
    result.push(ConditionPart {
      leading_comments,
      operator,
      elements: part,
    });
  }
  result
}

/// Conditions after a keyword, of which the first stays on the keyword's line and the rest are
/// indented on lines of their own when they don't fit (ex. `WHERE a = 1` then `  AND b = 2`).
fn gen_conditions_hanging(elements: &[Element], context: &mut Context) -> PrintItems {
  let parts = split_conditions(elements);
  gen_condition_parts(
    None,
    &parts,
    MultiLineOptions::same_line_start_hanging_indent(),
    false,
    context,
  )
}

fn gen_condition_parts(
  first: Option<PrintItems>,
  parts: &[ConditionPart],
  multi_line_options: MultiLineOptions,
  force: bool,
  context: &mut Context,
) -> PrintItems {
  if first.is_none() && parts.len() == 1 && parts[0].leading_comments.is_empty() {
    return gen_condition_part(&parts[0], context);
  }
  let force = force || parts.iter().skip(1).any(|part| !part.leading_comments.is_empty());
  let indent_width = context.config.indent_width;
  ir_helpers::gen_separated_values(
    |_| {
      let mut values = Vec::with_capacity(parts.len() + 1);
      if let Some(first) = first {
        values.push(GeneratedValue::from_items(first));
      }
      for (i, part) in parts.iter().enumerate() {
        values.push(GeneratedValue {
          items: ir_helpers::new_line_group(gen_condition_part(part, context)),
          lines_span: None,
          allow_inline_multi_line: false,
          allow_inline_single_line: false,
          is_known_multi_line: part.is_known_multi_line(i + 1 == parts.len()),
        });
      }
      values
    },
    GenSeparatedValuesOptions {
      prefer_hanging: false,
      force_use_new_lines: force,
      allow_blank_lines: false,
      indent_width,
      single_line_options: SingleLineOptions::separated_same_line(Signal::SpaceOrNewLine.into()),
      multi_line_options,
      force_possible_newline_at_start: false,
    },
  )
  .items
}

fn gen_condition_part(part: &ConditionPart, context: &mut Context) -> PrintItems {
  let mut items = PrintItems::new();
  let first_token = part.operator.or_else(|| part.elements.first().map(|e| e.first_token()));
  items.extend(gen_leading_comments(&part.leading_comments, first_token));
  if let Some(operator) = part.operator {
    items.extend(gen_keyword(operator, context));
    items.extend(space_or_comment_spacing_opt(part.elements.first()));
  }
  items.extend(gen_fill(part.elements, context));
  items
}

// ---- from and joins ----

fn gen_from(body: &[Element], force_list: bool, context: &mut Context) -> PrintItems {
  let mut join_starts = Vec::new();
  let mut i = 0;
  while i < body.len() {
    if let Some(len) = join_keyword_len(body, i) {
      join_starts.push((i, len));
      i += len;
    } else {
      i += 1;
    }
  }
  let refs_end = join_starts.first().map(|(start, _)| *start).unwrap_or(body.len());
  let mut refs = &body[..refs_end];
  let mut pending = Vec::new();
  if !join_starts.is_empty() {
    let split = own_line_comments_start(refs);
    pending = refs[split..].iter().filter_map(|e| e.as_token()).collect();
    refs = &refs[..split];
  }

  let mut items = gen_list(refs, ListStyle::AfterKeyword, force_list, context);
  for (index, (start, len)) in join_starts.iter().enumerate() {
    let end = join_starts
      .get(index + 1)
      .map(|(start, _)| *start)
      .unwrap_or(body.len());
    let mut rest = &body[start + len..end];
    let leading_comments = std::mem::take(&mut pending);
    if index + 1 < join_starts.len() {
      let split = own_line_comments_start(rest);
      pending = rest[split..].iter().filter_map(|e| e.as_token()).collect();
      rest = &rest[..split];
    }
    let mut join = PrintItems::new();
    join.push_signal(Signal::NewLine);
    join.extend(gen_leading_comments(
      &leading_comments,
      Some(body[*start].first_token()),
    ));
    join.extend(gen_join(&body[*start..start + len], rest, context));
    if context.config.joins_as_top_level {
      items.extend(join);
    } else {
      items.extend(ir_helpers::with_indent(join));
    }
  }
  items
}

/// How many words starting at `i` make up the keyword of a join (ex. `LEFT OUTER JOIN`).
fn join_keyword_len(elements: &[Element], i: usize) -> Option<usize> {
  if prev_significant(elements, i).is_some_and(|e| e.is_kind(TokenKind::Dot)) {
    return None;
  }
  let word = |offset: usize| word_at(elements, i + offset);
  let first = word(0)?;
  if first.is_word("STRAIGHT_JOIN") {
    return Some(1);
  }
  if (first.is_word("CROSS") || first.is_word("OUTER")) && word(1).is_some_and(|t| t.is_word("APPLY")) {
    return Some(2);
  }
  if first.is_word("LATERAL") && word(1).is_some_and(|t| t.is_word("VIEW")) {
    return Some(if word(2).is_some_and(|t| t.is_word("OUTER")) {
      3
    } else {
      2
    });
  }
  let mut len = 0;
  for optional in [
    &["NATURAL", "ASOF", "GLOBAL"][..],
    &["INNER", "CROSS", "LEFT", "RIGHT", "FULL", "POSITIONAL"],
    &["OUTER"],
    &["SEMI", "ANTI"],
    &["HASH", "MERGE", "LOOP", "REMOTE"],
  ] {
    if word(len).is_some_and(|t| optional.iter().any(|w| t.is_word(w))) {
      len += 1;
    }
  }
  word(len).is_some_and(|t| t.is_word("JOIN")).then_some(len + 1)
}

fn gen_join(keyword: &[Element], rest: &[Element], context: &mut Context) -> PrintItems {
  let mut items = gen_fill_ex(keyword, context, true);
  if rest.is_empty() {
    return items;
  }
  items.extend(space_or_comment_spacing(&rest[0]));
  match rest.iter().position(|e| e.is_word("ON")) {
    Some(on_index) if on_index > 0 => {
      let table = gen_fill(&rest[..on_index], context);
      let mut parts = split_conditions(&rest[on_index + 1..]);
      parts[0].operator = rest[on_index].as_token();
      items.extend(gen_condition_parts(
        Some(table),
        &parts,
        MultiLineOptions::same_line_start_hanging_indent(),
        false,
        context,
      ));
    }
    _ => items.extend(gen_fill(rest, context)),
  }
  items
}

// ---- with ----

/// The common table expressions after a `WITH`, each starting a line of its own after the first.
fn gen_with(body: &[Element], context: &mut Context) -> PrintItems {
  let mut items = PrintItems::new();
  for (i, item) in split_list(body).iter().enumerate() {
    if i > 0 {
      items.push_signal(Signal::NewLine);
    }
    items.extend(gen_list_item(item, context));
  }
  items
}

// ---- merge ----

fn gen_merge_when(body: &[Element], context: &mut Context) -> PrintItems {
  let Some(then_index) = body.iter().position(|e| e.is_word("THEN")) else {
    return gen_fill(body, context);
  };
  let mut items = gen_fill(&body[..then_index], context);
  items.extend(space_if_not_start_of_line());
  items.extend(gen_fill_ex(&body[then_index..then_index + 1], context, true));
  let action = &body[then_index + 1..];
  if action.is_empty() {
    return items;
  }
  let mut action_items = PrintItems::new();
  action_items.push_signal(Signal::NewLine);
  if action.first().is_some_and(|e| e.is_word("UPDATE")) && action.get(1).is_some_and(|e| e.is_word("SET")) {
    action_items.extend(gen_fill_ex(&action[..2], context, true));
    action_items.extend(gen_list(&action[2..], ListStyle::AfterKeyword, false, context));
  } else {
    action_items.extend(gen_fill(action, context));
  }
  items.extend(ir_helpers::with_indent(action_items));
  items
}

// ---- groups ----

fn gen_group(group: &Group, context: &mut Context) -> PrintItems {
  let mut items = ir_helpers::gen_from_string(group.open.text);
  let elements = &group.elements;
  if elements.iter().all(|e| e.is_comment()) {
    if !elements.is_empty() {
      let mut comments = PrintItems::new();
      for (i, element) in elements.iter().enumerate() {
        let token = element.as_token().unwrap();
        if i == 0 && token.newlines_before == 0 {
          comments.extend(gen_trailing_comment(token));
        } else {
          comments.push_signal(Signal::NewLine);
          comments.extend(gen_comment(token));
        }
      }
      items.extend(ir_helpers::with_indent(comments));
      items.push_signal(Signal::NewLine);
    }
    items.extend(ir_helpers::gen_from_string(group.close.text));
    return items;
  }

  let force = group_forces_multi_line(group);
  if is_query_start(elements) || (group.is_paren() && detect_mode(elements) == Mode::Window) {
    let clauses = split_clauses(elements);
    let force = force || clauses.iter().any(|clause| clause.is_known_multi_line());
    items.extend(gen_clauses_fit(&clauses, force, ClausesLayout::InBrackets, context));
  } else if !elements.iter().any(|e| e.is_kind(TokenKind::Comma)) && has_condition_operator(elements) {
    let parts = split_conditions(elements);
    let force = force || parts.iter().any(|part| part.is_known_multi_line(false));
    items.extend(gen_condition_parts(
      None,
      &parts,
      MultiLineOptions::surround_newlines_indented(),
      force,
      context,
    ));
  } else {
    items.extend(gen_list(elements, ListStyle::InBrackets, force, context));
  }
  items.extend(ir_helpers::gen_from_string(group.close.text));
  items
}

/// Whether the brackets are written over several lines however wide the line is: when the author
/// started their contents on a new line, or when something within them can't be written on one.
/// Brackets that are the only thing within them stay against them however they're written, so
/// they don't count.
fn group_forces_multi_line(group: &Group) -> bool {
  let starts_new_line = group
    .elements
    .first()
    .is_some_and(|e| e.first_token().newlines_before > 0);
  starts_new_line
    || match lone_group(&group.elements) {
      Some(_) => group.elements.iter().any(|e| e.is_line_comment()),
      None => is_known_multi_line(&group.elements),
    }
}

/// The brackets that are all there is to the elements apart from an alias after them (ex.
/// `(SELECT ..) AS t`), which are written against whatever is before them even when they're
/// written over several lines.
fn lone_group<'e, 'a>(elements: &'e [Element<'a>]) -> Option<&'e Group<'a>> {
  let mut significant = elements.iter().filter(|e| !e.is_comment());
  let Some(Element::Group(group)) = significant.next() else {
    return None;
  };
  let rest: Vec<&Element> = significant.collect();
  let is_name = |e: &Element| {
    e.is_kind(TokenKind::QuotedIdent)
      || (e.is_kind(TokenKind::Word)
        && !e
          .as_token()
          .is_some_and(|t| keywords::contains(keywords::RESERVED, t.text)))
  };
  let is_alias = match rest.as_slice() {
    [] => true,
    [name] => is_name(name),
    [as_keyword, name] => as_keyword.is_word("AS") && is_name(name),
    _ => false,
  };
  is_alias.then_some(group)
}

fn has_condition_operator(elements: &[Element]) -> bool {
  let mut in_between = false;
  for element in elements {
    if element.is_word("BETWEEN") {
      in_between = true;
    } else if element.is_word("AND") && in_between {
      in_between = false;
    } else if element.is_word("AND") || element.is_word("OR") {
      return true;
    }
  }
  false
}

// ---- case ----

fn gen_case(case: &Case, context: &mut Context) -> PrintItems {
  let mut items = gen_keyword(case.case_keyword, context);
  if !case.operand.is_empty() {
    items.push_space();
    items.extend(gen_fill(&case.operand, context));
  }
  let force = case_forces_multi_line(case);
  let indent_width = context.config.indent_width;
  let branch_count = case.branches.len() + usize::from(case.else_branch.is_some());
  items.extend(
    ir_helpers::gen_separated_values(
      |_| {
        let mut values = Vec::with_capacity(branch_count);
        for branch in &case.branches {
          let mut items = PrintItems::new();
          items.extend(gen_leading_comments(&branch.leading_comments, Some(branch.when)));
          items.extend(gen_keyword(branch.when, context));
          items.extend(space_or_comment_spacing_opt(branch.condition.first()));
          items.extend(gen_fill(&branch.condition, context));
          items.extend(space_if_not_start_of_line());
          items.extend(gen_keyword(branch.then, context));
          items.extend(space_or_comment_spacing_opt(branch.result.first()));
          items.extend(gen_fill(&branch.result, context));
          values.push(GeneratedValue::from_items(ir_helpers::new_line_group(items)));
        }
        if let Some(else_branch) = &case.else_branch {
          let mut items = PrintItems::new();
          items.extend(gen_leading_comments(
            &else_branch.leading_comments,
            Some(else_branch.keyword),
          ));
          items.extend(gen_keyword(else_branch.keyword, context));
          items.extend(space_or_comment_spacing_opt(else_branch.result.first()));
          items.extend(gen_fill(&else_branch.result, context));
          values.push(GeneratedValue::from_items(ir_helpers::new_line_group(items)));
        }
        if !case.end_comments.is_empty()
          && let Some(last) = values.last_mut()
        {
          for comment in &case.end_comments {
            last.items.push_signal(Signal::NewLine);
            last.items.extend(gen_comment(comment));
          }
        }
        values
      },
      GenSeparatedValuesOptions {
        prefer_hanging: false,
        force_use_new_lines: force,
        allow_blank_lines: false,
        indent_width,
        single_line_options: SingleLineOptions::surrounded_line(),
        multi_line_options: MultiLineOptions::surround_newlines_indented(),
        force_possible_newline_at_start: false,
      },
    )
    .items,
  );
  items.extend(gen_keyword(case.end, context));
  items
}

/// Whether a `CASE` is written over several lines however wide the line is: when the author
/// started its first `WHEN` on a new line, or when something within it can't be written on one.
fn case_forces_multi_line(case: &Case) -> bool {
  let first_branch_token = case
    .branches
    .first()
    .map(|b| b.leading_comments.first().copied().unwrap_or(b.when));
  first_branch_token.is_some_and(|t| t.newlines_before > 0)
    || !case.end_comments.is_empty()
    || is_known_multi_line(&case.operand)
    || case
      .branches
      .iter()
      .any(|b| !b.leading_comments.is_empty() || is_known_multi_line(&b.condition) || is_known_multi_line(&b.result))
    || case
      .else_branch
      .as_ref()
      .is_some_and(|e| !e.leading_comments.is_empty() || is_known_multi_line(&e.result))
}

// ---- elements ----

fn gen_fill(elements: &[Element], context: &mut Context) -> PrintItems {
  gen_fill_ex(elements, context, false)
}

/// Writes the elements one after another as they flow, with only comments breaking the line.
/// `all_keywords` says that every word is a keyword (ex. the words of a clause's keyword).
fn gen_fill_ex(elements: &[Element], context: &mut Context, all_keywords: bool) -> PrintItems {
  let mut items = PrintItems::new();
  let angle_types = angle_bracket_types(elements);
  for (i, element) in elements.iter().enumerate() {
    if i > 0 {
      let previous = &elements[i - 1];
      if angle_types
        .iter()
        .any(|range| range.contains(&(i - 1)) && range.contains(&i))
      {
        // the parameters of a type in angle brackets keep their spacing
        if element.first_token().space_before {
          items.push_space();
        }
      } else if element.is_comment() {
        let token = element.as_token().unwrap();
        if token.newlines_before > 0 {
          items.push_signal(Signal::NewLine);
        } else if !previous.is_line_comment() {
          items.extend(space_if_not_start_of_line());
        }
      } else if previous.is_line_comment() {
        // the comment already ended the line
      } else if previous.is_comment() {
        let first = element.first_token();
        if first.newlines_before > 0 {
          items.push_signal(Signal::NewLine);
        } else if first.space_before {
          items.push_space();
        }
      } else if needs_space(elements, i) || would_join(previous.last_token(), element.first_token(), context) {
        items.push_space();
      }
    }
    items.extend(gen_element(elements, i, context, all_keywords));
  }
  items
}

fn gen_element(elements: &[Element], i: usize, context: &mut Context, all_keywords: bool) -> PrintItems {
  match &elements[i] {
    Element::Token(token) => {
      let is_keyword = token.kind == TokenKind::Word && (all_keywords || is_keyword_at(elements, i));
      gen_token(token, is_keyword, context)
    }
    Element::Group(group) => gen_group(group, context),
    Element::Case(case) => gen_case(case, context),
    element => gen_procedural(element, context),
  }
}

/// Whether the word at `i` is a keyword: one that's always reserved, or one that's a keyword
/// after the word before it (ex. the `KEY` of `PRIMARY KEY`). A word that's part of a qualified
/// name (ex. `t.order`) is a name.
fn is_keyword_at(elements: &[Element], i: usize) -> bool {
  const CONTEXTUAL: &[(&str, &[&str])] = &[
    ("ACTION", &["NO"]),
    ("FIRST", &["NULLS", "FETCH"]),
    ("KEY", &["PRIMARY", "FOREIGN", "UNIQUE", "DUPLICATE"]),
    ("LAST", &["NULLS"]),
    ("MATCHED", &["NOT"]),
    ("MODE", &["SHARE"]),
    ("NEXT", &["FETCH"]),
    ("NOTHING", &["DO"]),
    ("ONLY", &["ROWS", "ROW"]),
    ("PRECISION", &["DOUBLE"]),
    ("ROW", &["CURRENT", "EACH"]),
    ("SOURCE", &["BY"]),
    ("TARGET", &["BY"]),
    ("TIES", &["WITH"]),
    ("VARYING", &["CHARACTER", "CHAR"]),
    ("ZONE", &["TIME"]),
  ];
  let Some(token) = elements[i].as_token() else {
    return false;
  };
  let previous = prev_significant(elements, i);
  if previous.is_some_and(|e| e.is_kind(TokenKind::Dot))
    || elements.get(i + 1).is_some_and(|e| e.is_kind(TokenKind::Dot))
  {
    return false;
  }
  if keywords::contains(keywords::RESERVED, token.text) {
    return true;
  }
  // words that are keywords before the word after them
  let next = elements.get(i + 1).and_then(|e| e.as_token());
  let next_is = |words: &[&str]| next.is_some_and(|next| words.iter().any(|w| next.is_word(w)));
  if (token.is_word("NO") && next_is(&["ACTION"]))
    || ((token.is_word("ROW") || token.is_word("ROWS"))
      && next_is(&["ONLY", "BETWEEN", "UNBOUNDED", "PRECEDING", "FOLLOWING", "CURRENT"]))
  {
    return true;
  }
  let Some(previous) = previous.and_then(|e| e.as_token()) else {
    return false;
  };
  if (token.is_word("ROW") || token.is_word("ROWS")) && previous.kind == TokenKind::Number {
    return true;
  }
  if previous.is_operator("::") {
    return keywords::contains(keywords::TYPES, token.text);
  }
  CONTEXTUAL
    .iter()
    .any(|(word, after)| token.is_word(word) && after.iter().any(|a| previous.is_word(a)))
}

fn gen_token(token: &Token, is_keyword: bool, context: &mut Context) -> PrintItems {
  match token.kind {
    TokenKind::Word => gen_word(token, is_keyword, context),
    TokenKind::LineComment | TokenKind::BlockComment | TokenKind::TemplateLine => gen_comment(token),
    TokenKind::DollarString if context.code_bodies.contains(&token.start) => gen_code_body(token, context),
    _ if token.is_multi_line() => gen_raw_text(token.text),
    _ => ir_helpers::gen_from_string(token.text),
  }
}

/// Text written exactly as it is, including the line breaks within it. The first line is
/// indented like any other, but the lines after it aren't, since the text is a token whose
/// contents can't change (ex. a string spanning lines).
fn gen_raw_text(text: &str) -> PrintItems {
  let mut items = PrintItems::new();
  items.push_force_current_line_indentation();
  items.extend(ir_helpers::gen_from_raw_string(text));
  items
}

fn gen_word(token: &Token, is_keyword: bool, context: &Context) -> PrintItems {
  let mut items = PrintItems::new();
  match (is_keyword, context.config.uppercase) {
    (true, UppercaseKind::Upper) => items.push_string(token.text.to_ascii_uppercase()),
    (true, UppercaseKind::Lower) => items.push_string(token.text.to_ascii_lowercase()),
    _ => items.push_str(token.text),
  }
  items
}

fn gen_keyword(token: &Token, context: &Context) -> PrintItems {
  gen_word(token, token.kind == TokenKind::Word, context)
}

fn gen_keywords(tokens: &[Tok], context: &Context) -> PrintItems {
  let mut items = PrintItems::new();
  for (i, token) in tokens.iter().enumerate() {
    if i > 0 {
      items.push_space();
    }
    items.extend(gen_keyword(token, context));
  }
  items
}

/// The code in a dollar-quoted string, such as the body of a PL/pgSQL function, which is
/// formatted as SQL when it can be. The code starts on the line after the opening quote, without
/// any indentation, since it's still the contents of a string.
fn gen_code_body(token: &Token, context: &mut Context) -> PrintItems {
  let text = token.text;
  let tag_len = text[1..].find('$').map(|i| i + 2).unwrap_or(text.len());
  if text.len() < tag_len * 2 {
    return gen_raw_text(text);
  }
  let tag = &text[..tag_len];
  let inner = &text[tag_len..text.len() - tag_len];
  if inner.trim().is_empty() {
    return gen_raw_text(text);
  }
  let Some(formatted) = crate::format_text::format_embedded(inner, context.config) else {
    return gen_raw_text(text);
  };
  let formatted = formatted.trim_end_matches('\n');
  let result = if !inner.contains('\n') && !formatted.contains('\n') {
    format!("{tag} {formatted} {tag}")
  } else {
    format!("{tag}\n{formatted}\n{tag}")
  };
  gen_raw_text(&result)
}

/// Whether a space goes between the element at `i` and the one before it, neither of which is a
/// comment.
/// Whether two tokens that are apart in the text would read as something else written against each
/// other (ex. `@ a` as the variable `@a`, or `{ {` as the start of a template).
fn would_join(previous: &Token, next: &Token, context: &Context) -> bool {
  if !next.space_before && next.newlines_before == 0 {
    return false;
  }
  let text = format!("{}{}", previous.text, next.text);
  match crate::lexer::lex(&text, context.config.dialect) {
    Ok(tokens) => !(tokens.len() == 2 && tokens[0].text == previous.text && tokens[1].text == next.text),
    Err(_) => true,
  }
}

fn needs_space(elements: &[Element], i: usize) -> bool {
  let previous = &elements[i - 1];
  let next = &elements[i];
  let next_first = next.first_token();

  if next.is_kind(TokenKind::Comma) || next.is_kind(TokenKind::Semicolon) {
    return false;
  }
  if previous.is_kind(TokenKind::Comma) || previous.is_kind(TokenKind::Semicolon) {
    return true;
  }
  if previous.is_kind(TokenKind::Dot) || next.is_kind(TokenKind::Dot) {
    // keep apart a dot and a number that would otherwise read as one number (ex. `\.` then `2`)
    let separated = next_first.space_before || next_first.newlines_before > 0;
    return separated
      && (previous.is_kind(TokenKind::Number) || next_first.text.starts_with(|c: char| c.is_ascii_digit()));
  }
  if let Some(operator) = next.as_token().filter(|t| t.kind == TokenKind::Operator) {
    return match operator.text {
      "::" => false,
      ":" | ".." => operator.space_before,
      // an attribute like `%TYPE` or `%NOTFOUND` in PL/SQL and PL/pgSQL, which a `%` written
      // against what's on both sides of it might be
      "%" if is_attribute_operator(elements, i) => false,
      _ => true,
    };
  }
  if let Some(operator) = previous.as_token().filter(|t| t.kind == TokenKind::Operator) {
    return match operator.text {
      "::" => false,
      "%" if is_attribute_operator(elements, i - 1) => false,
      ":" | ".." => next_first.space_before,
      // a `*` where a unary operator would be is a wildcard (ex. `SELECT *`)
      "*" => true,
      _ => !(is_unary_operator(operator) && is_unary_position(elements, i - 1)),
    };
  }
  if let Element::Group(group) = next {
    return match group.open.kind {
      TokenKind::OpenParen => match previous {
        Element::Token(token) if token.kind == TokenKind::Word => {
          keywords::contains(keywords::SPACE_BEFORE_PAREN, token.text) || group.open.space_before
        }
        Element::Case(_) => true,
        _ => group.open.space_before,
      },
      _ => group.open.space_before,
    };
  }
  if previous.is_kind(TokenKind::Other) || next.is_kind(TokenKind::Other) {
    return next_first.space_before;
  }
  if matches!(previous, Element::Group(_)) {
    return !next.is_kind(TokenKind::Template) || next_first.space_before;
  }
  // two operands or words, which stay together when the source has them together (ex. MySQL's
  // `_utf8'text'`)
  next_first.space_before
}

/// The ranges of the elements that are a type's parameters in angle brackets, from the `<` to the
/// `>` (ex. BigQuery's `ARRAY<INT64>` or `STRUCT<a INT64, b STRING>`), whose `<` and `>` aren't
/// comparisons.
fn angle_bracket_types(elements: &[Element]) -> Vec<std::ops::RangeInclusive<usize>> {
  let mut ranges = Vec::new();
  let mut i = 1;
  while i < elements.len() {
    let opens = elements[i]
      .as_token()
      .is_some_and(|t| t.is_operator("<") && !t.space_before)
      && elements[i - 1].as_token().is_some_and(|t| {
        ["ARRAY", "STRUCT", "MAP", "RANGE", "TABLE"]
          .iter()
          .any(|w| t.is_word(w))
      });
    if !opens {
      i += 1;
      continue;
    }
    let mut depth = 0i32;
    let mut end = None;
    for (j, element) in elements.iter().enumerate().skip(i) {
      if let Some(token) = element.as_token().filter(|t| t.kind == TokenKind::Operator) {
        depth += token.text.matches('<').count() as i32 - token.text.matches('>').count() as i32;
        if depth <= 0 {
          end = Some(j);
          break;
        }
      }
    }
    match end {
      Some(end) => {
        // along with what's against them on either side
        ranges.push(i - 1..=(end + 1).min(elements.len() - 1));
        i = end + 1;
      }
      None => i += 1,
    }
  }
  ranges
}

/// Whether the `%` at `i` is written against what's on both sides of it, as the `%` of an
/// attribute (ex. `employees.name%TYPE`) is.
fn is_attribute_operator(elements: &[Element], i: usize) -> bool {
  let operator = elements[i].first_token();
  !operator.space_before
    && elements
      .get(i + 1)
      .is_some_and(|next| next.is_kind(TokenKind::Word) && !next.first_token().space_before)
}

fn is_unary_operator(token: &Token) -> bool {
  matches!(token.text, "-" | "+" | "~" | "!" | "@")
}

/// Whether an operator at `i` is in the position of a unary one: at the start, or after another
/// operator, a comma or a keyword that doesn't stand for a value.
fn is_unary_position(elements: &[Element], i: usize) -> bool {
  match prev_significant(elements, i) {
    None => true,
    Some(Element::Token(token)) => match token.kind {
      TokenKind::Operator | TokenKind::Comma | TokenKind::Semicolon => true,
      TokenKind::Word => {
        keywords::contains(keywords::RESERVED, token.text) && !keywords::contains(keywords::VALUE_KEYWORDS, token.text)
      }
      _ => false,
    },
    Some(_) => false,
  }
}

// ---- comments ----

fn gen_comment(token: &Token) -> PrintItems {
  match token.kind {
    TokenKind::LineComment | TokenKind::TemplateLine => {
      let text = token.text.trim_end_matches([' ', '\t']);
      // a template tag spanning lines (ex. `{% if a` then `and b %}`) is written as it is
      let mut items = if token.is_multi_line() {
        gen_raw_text(text)
      } else {
        ir_helpers::gen_from_string(text)
      };
      items.push_signal(Signal::ExpectNewLine);
      items
    }
    _ if token.is_multi_line() => gen_raw_text(token.text),
    _ => ir_helpers::gen_from_string(token.text),
  }
}

/// Comments that start lines of their own before something, which keep whatever was on the line
/// after them on that line, and otherwise end it. `next` is the first token of what they're
/// before.
fn gen_leading_comments(comments: &[Tok], next: Option<Tok>) -> PrintItems {
  let mut items = PrintItems::new();
  for (i, comment) in comments.iter().enumerate() {
    items.extend(gen_comment(comment));
    let following = comments.get(i + 1).copied().or(next);
    if !comment.is_line_comment() && following.is_some_and(|t| t.newlines_before == 0) {
      items.push_space();
    } else {
      items.push_signal(Signal::NewLine);
    }
  }
  items
}

fn gen_trailing_comment(token: &Token) -> PrintItems {
  let mut items = space_if_not_start_of_line();
  items.extend(gen_comment(token));
  items
}

fn space_if_not_start_of_line() -> PrintItems {
  if_false(
    "spaceIfNotStartOfLine",
    condition_resolvers::is_start_of_line(),
    " ".into(),
  )
  .into()
}

/// The space after a keyword and before the element that follows it, or a line break when that
/// element is a comment on a line of its own.
fn space_or_comment_spacing(next: &Element) -> PrintItems {
  match next.as_token() {
    Some(token) if token.is_comment() && token.newlines_before > 0 => Signal::NewLine.into(),
    _ => space_if_not_start_of_line(),
  }
}

fn space_or_comment_spacing_opt(next: Option<&Element>) -> PrintItems {
  next.map(space_or_comment_spacing).unwrap_or_default()
}

// ---- helpers ----

fn word_at<'a>(elements: &[Element<'a>], i: usize) -> Option<Tok<'a>> {
  elements.get(i)?.as_token().filter(|t| t.kind == TokenKind::Word)
}

fn word_or_string_at(elements: &[Element], i: usize) -> bool {
  elements
    .get(i)
    .is_some_and(|e| e.is_kind(TokenKind::Word) || e.is_kind(TokenKind::String) || e.is_kind(TokenKind::QuotedIdent))
}

/// The element before `i` that isn't a comment.
fn prev_significant<'e, 'a>(elements: &'e [Element<'a>], i: usize) -> Option<&'e Element<'a>> {
  elements[..i].iter().rev().find(|e| !e.is_comment())
}

/// Where the comments at the end of the elements that are on lines of their own start, which is
/// the end of the elements when there aren't any. A comment on the line of what's before it ends
/// that line rather than introducing what's next, so it isn't one of them.
fn own_line_comments_start(elements: &[Element]) -> usize {
  let mut split = elements.len();
  while split > 0 && elements[split - 1].is_comment() {
    split -= 1;
  }
  while split < elements.len() && elements[split].first_token().newlines_before == 0 {
    split += 1;
  }
  split
}

/// Whether the elements start a query (ex. a subquery's `SELECT`).
fn is_query_start(elements: &[Element]) -> bool {
  let Some((index, first)) = elements.iter().enumerate().find(|(_, e)| !e.is_comment()) else {
    return false;
  };
  match first {
    Element::Group(group) => is_query_start(&group.elements),
    Element::Token(token) => {
      token.is_word("SELECT")
        || token.is_word("VALUES")
        || token.is_word("TABLE")
        || (token.is_word("WITH") && is_cte_start(elements, index))
    }
    _ => false,
  }
}

/// Whether the `WITH` at `i` begins common table expressions rather than a table hint or an
/// option (ex. `WITH (NOLOCK)` or `WITH TIES`).
fn is_cte_start(elements: &[Element], i: usize) -> bool {
  let mut rest = elements[i + 1..].iter().filter(|e| !e.is_comment());
  match rest.next() {
    Some(e) if e.is_word("RECURSIVE") => true,
    Some(e) if e.is_kind(TokenKind::Word) || e.is_kind(TokenKind::QuotedIdent) => match rest.next() {
      Some(e) if e.is_word("AS") => true,
      Some(Element::Group(_)) => rest.next().is_some_and(|e| e.is_word("AS")),
      _ => false,
    },
    _ => false,
  }
}

/// Whether the elements are written over several lines however wide the line is, because of a
/// comment that ends a line or brackets that are kept on several lines.
fn is_known_multi_line(elements: &[Element]) -> bool {
  elements.iter().enumerate().any(|(i, element)| match element {
    Element::Token(token) => token.is_line_comment() || (token.is_comment() && i > 0 && token.newlines_before > 0),
    Element::Group(group) => group_forces_multi_line(group),
    Element::Case(case) => case_forces_multi_line(case),
    Element::Label(_) => false,
    _ => true,
  })
}

/// The elements without the comments at their end that are on the line of what's before them.
fn without_trailing_comments<'e, 'a>(elements: &'e [Element<'a>]) -> &'e [Element<'a>] {
  let mut end = elements.len();
  while end > 0
    && elements[end - 1]
      .as_token()
      .is_some_and(|t| t.is_comment() && t.newlines_before == 0)
  {
    end -= 1;
  }
  &elements[..end]
}

/// Whether a list item is written over several lines only because of a string with a line break
/// in it (ex. a string spanning lines with an alias after it), which then doesn't break up the
/// list it's in.
///
/// Brackets rule that out, since whether they're broken depends on where the list puts them, and
/// a list that stays on its line because of brackets broken the last time it was formatted would
/// be written differently each time.
fn is_multi_line_text_item(elements: &[Element]) -> bool {
  elements
    .iter()
    .any(|element| element.as_token().is_some_and(|token| token.is_multi_line()))
    && elements.iter().all(|element| matches!(element, Element::Token(_)))
}
