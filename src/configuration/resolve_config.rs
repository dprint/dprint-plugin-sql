use super::Configuration;
use dprint_core::configuration::*;

/// Resolves configuration from a collection of key value strings.
///
/// # Example
///
/// ```
/// use dprint_core::configuration::ConfigKeyMap;
/// use dprint_core::configuration::resolve_global_config;
/// use dprint_plugin_sql::configuration::resolve_config;
///
/// let config_map = ConfigKeyMap::new(); // get a collection of key value pairs from somewhere
/// let global_config_result = resolve_global_config(config_map, &Default::default());
///
/// // check global_config_result.diagnostics here...
///
/// let config_map = ConfigKeyMap::new(); // get a collection of k/v pairs from somewhere
/// let config_result = resolve_config(
///     config_map,
///     &global_config_result.config
/// );
///
/// // check config_result.diagnostics here and use config_result.config
/// ```
pub fn resolve_config(
  config: ConfigKeyMap,
  global_config: &GlobalConfiguration,
) -> ResolveConfigurationResult<Configuration> {
  let mut diagnostics = Vec::new();
  let mut config = config;

  let resolved_config = Configuration {
    use_tabs: get_value(
      &mut config,
      "useTabs",
      global_config
        .use_tabs
        .unwrap_or(RECOMMENDED_GLOBAL_CONFIGURATION.use_tabs),
      &mut diagnostics,
    ),
    indent_width: get_value(
      &mut config,
      "indentWidth",
      global_config
        .indent_width
        .unwrap_or(RECOMMENDED_GLOBAL_CONFIGURATION.indent_width),
      &mut diagnostics,
    ),
    new_line_kind: get_value(
      &mut config,
      "newLineKind",
      global_config
        .new_line_kind
        .unwrap_or(RECOMMENDED_GLOBAL_CONFIGURATION.new_line_kind),
      &mut diagnostics,
    ),
    uppercase: get_value(&mut config, "uppercase", false, &mut diagnostics),
    lines_between_queries: get_value(&mut config, "linesBetweenQueries", 1, &mut diagnostics),
  };

  diagnostics.extend(get_unknown_property_diagnostics(config));

  ResolveConfigurationResult {
    config: resolved_config,
    diagnostics,
  }
}
