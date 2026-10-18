use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigKeyValue;
use dprint_core::configuration::GlobalConfiguration;
use dprint_core::configuration::NewLineKind;

use super::*;

/// Formatting configuration builder.
///
/// # Example
///
/// ```
/// use dprint_plugin_sql::configuration::*;
///
/// let config = ConfigurationBuilder::new()
///     .casing(UppercaseKind::Upper)
///     .build();
/// ```
#[derive(Default)]
pub struct ConfigurationBuilder {
  pub(super) config: ConfigKeyMap,
  global_config: Option<GlobalConfiguration>,
}

impl ConfigurationBuilder {
  /// Constructs a new configuration builder.
  pub fn new() -> ConfigurationBuilder {
    ConfigurationBuilder::default()
  }

  /// Gets the final configuration that can be used to format a file.
  pub fn build(&self) -> Configuration {
    if let Some(global_config) = &self.global_config {
      resolve_config(self.config.clone(), global_config).config
    } else {
      let global_config = GlobalConfiguration::default();
      resolve_config(self.config.clone(), &global_config).config
    }
  }

  /// Set the global configuration.
  pub fn global_config(&mut self, global_config: GlobalConfiguration) -> &mut Self {
    self.global_config = Some(global_config);
    self
  }

  /// The width of a line the printer will try to stay under. Note that the printer may exceed this width in certain cases.
  ///
  /// Default: `120`
  pub fn line_width(&mut self, value: u32) -> &mut Self {
    self.insert("lineWidth", (value as i32).into())
  }

  /// Whether to use tabs (true) or spaces (false).
  ///
  /// Default: `false`
  pub fn use_tabs(&mut self, value: bool) -> &mut Self {
    self.insert("useTabs", value.into())
  }

  /// The number of columns for an indent.
  ///
  /// Default: `2`
  pub fn indent_width(&mut self, value: u8) -> &mut Self {
    self.insert("indentWidth", (value as i32).into())
  }

  /// The kind of newline to use.
  ///
  /// Default: `NewLineKind::LineFeed`
  pub fn new_line_kind(&mut self, value: NewLineKind) -> &mut Self {
    self.insert("newLineKind", value.to_string().into())
  }

  /// The casing of keywords.
  ///
  /// Default: `UppercaseKind::Preserve`
  pub fn casing(&mut self, value: UppercaseKind) -> &mut Self {
    self.insert("casing", value.to_string().into())
  }

  /// The minimum number of line breaks between statements at the top level of the file.
  ///
  /// Default: `1`
  pub fn lines_between_queries(&mut self, value: u8) -> &mut Self {
    self.insert("linesBetweenQueries", (value as i32).into())
  }

  /// Whether to write joins at the same indentation as the `FROM` keyword rather than indented beneath it.
  ///
  /// Default: `false`
  pub fn joins_as_top_level(&mut self, value: bool) -> &mut Self {
    self.insert("joinsAsTopLevel", value.into())
  }

  /// The SQL dialect, which decides how the text is split into tokens.
  ///
  /// Default: `Dialect::Generic`
  pub fn dialect(&mut self, value: Dialect) -> &mut Self {
    self.insert("dialect", value.to_string().into())
  }

  #[cfg(test)]
  pub(super) fn get_inner_config(&self) -> ConfigKeyMap {
    self.config.clone()
  }

  fn insert(&mut self, name: &str, value: ConfigKeyValue) -> &mut Self {
    self.config.insert(String::from(name), value);
    self
  }
}

#[cfg(test)]
mod tests {
  use dprint_core::configuration::NewLineKind;
  use dprint_core::configuration::resolve_global_config;

  use super::*;

  #[test]
  fn check_all_values_set() {
    let mut config = ConfigurationBuilder::new();
    config
      .line_width(80)
      .new_line_kind(NewLineKind::CarriageReturnLineFeed)
      .use_tabs(true)
      .indent_width(4)
      .casing(UppercaseKind::Upper)
      .lines_between_queries(2)
      .joins_as_top_level(true)
      .dialect(Dialect::PostgreSql);

    let inner_config = config.get_inner_config();
    assert_eq!(inner_config.len(), 8);
    let diagnostics = resolve_config(inner_config, &resolve_global_config(&mut Default::default()).config).diagnostics;
    assert_eq!(diagnostics.len(), 0);
  }

  #[test]
  fn handle_global_config() {
    let mut global_config = ConfigKeyMap::new();
    global_config.insert(String::from("lineWidth"), 90.into());
    global_config.insert(String::from("newLineKind"), "crlf".into());
    global_config.insert(String::from("useTabs"), true.into());
    let global_config = resolve_global_config(&mut global_config).config;
    let mut config_builder = ConfigurationBuilder::new();
    let config = config_builder.global_config(global_config).build();
    assert_eq!(config.line_width, 90);
    assert!(config.new_line_kind == NewLineKind::CarriageReturnLineFeed);
    assert!(config.use_tabs);
  }

  #[test]
  fn use_defaults_when_global_not_set() {
    let global_config = resolve_global_config(&mut Default::default()).config;
    let mut config_builder = ConfigurationBuilder::new();
    let config = config_builder.global_config(global_config).build();
    assert_eq!(config.line_width, 120);
    assert_eq!(config.indent_width, 2);
    assert!(config.new_line_kind == NewLineKind::LineFeed);
  }

  #[test]
  fn accepts_every_dialect() {
    for dialect in [
      "generic",
      "postgresql",
      "mysql",
      "sqlserver",
      "sqlite",
      "oracle",
      "bigquery",
      "snowflake",
    ] {
      let mut config = ConfigKeyMap::new();
      config.insert(String::from("dialect"), dialect.into());
      let result = resolve_config(config, &resolve_global_config(&mut Default::default()).config);
      assert_eq!(result.diagnostics.len(), 0, "{dialect}");
      assert_eq!(result.config.dialect.to_string(), dialect);
    }
  }
}
