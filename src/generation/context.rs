use std::collections::HashSet;

use dprint_core::formatting::Arena;

use crate::configuration::Configuration;

/// A vector allocated in the arena, for the collections built while generating, which are freed
/// along with the print items.
pub type BumpVec<'a, T> = dprint_core::formatting::bumpalo::collections::Vec<'a, T>;

pub struct Context<'a> {
  pub arena: &'a Arena,
  pub config: &'a Configuration,
  /// The text being formatted, which an ignored statement is written from.
  pub text: &'a str,
  /// Where the dollar-quoted strings start that hold the code of a routine (ex. the body of a
  /// PL/pgSQL function), which is formatted as SQL too.
  pub code_bodies: HashSet<usize>,
  /// Where the name of a table starts that the clause being written begins with (ex. the `view`
  /// of `FROM view`).
  pub table_name_start: Option<usize>,
  /// How many values separated onto lines when they don't fit on one are within each other.
  pub separated_values_depth: u32,
}

impl<'a> Context<'a> {
  pub fn new(arena: &'a Arena, config: &'a Configuration, text: &'a str) -> Self {
    Self {
      arena,
      config,
      text,
      code_bodies: HashSet::new(),
      table_name_start: None,
      separated_values_depth: 0,
    }
  }
}
