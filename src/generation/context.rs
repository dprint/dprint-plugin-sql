use std::collections::HashSet;

use crate::configuration::Configuration;

pub struct Context<'a> {
  pub config: &'a Configuration,
  /// The text being formatted, which an ignored statement is written from.
  pub text: &'a str,
  /// Where the dollar-quoted strings start that hold the code of a routine (ex. the body of a
  /// PL/pgSQL function), which is formatted as SQL too.
  pub code_bodies: HashSet<usize>,
}

impl<'a> Context<'a> {
  pub fn new(config: &'a Configuration, text: &'a str) -> Self {
    Self {
      config,
      text,
      code_bodies: HashSet::new(),
    }
  }
}
