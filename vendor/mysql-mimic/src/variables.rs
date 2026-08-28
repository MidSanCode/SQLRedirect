//! Session variables system.
//!
//! Provides MySQL session variables (@@variable_name) with defaults that
//! match what MySQL clients expect.

use std::collections::HashMap;
use std::sync::Arc;

/// A single variable value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// String value.
    String(String),
    /// Integer value.
    Int(i64),
    /// Boolean value.
    Bool(bool),
    /// Null value.
    Null,
}

impl Value {
    /// Convert to string representation for the MySQL wire protocol.
    pub fn to_mysql_string(&self) -> Option<String> {
        match self {
            Value::String(s) => Some(s.clone()),
            Value::Int(i) => Some(i.to_string()),
            Value::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
            Value::Null => None,
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::String(s) => write!(f, "{s}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Bool(b) => write!(f, "{}", if *b { "1" } else { "0" }),
            Value::Null => write!(f, "NULL"),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

/// Global variables shared across all sessions.
///
/// These provide the default values for session variables.
#[derive(Debug, Clone)]
pub struct GlobalVariables {
    defaults: HashMap<String, Value>,
}

impl Default for GlobalVariables {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalVariables {
    /// Create a new set of global variables with MySQL-compatible defaults.
    pub fn new() -> Self {
        let mut defaults = HashMap::new();

        // Server identification
        defaults.insert("version".into(), Value::String("8.0.0-mysql-mimic".into()));
        defaults.insert(
            "version_comment".into(),
            Value::String("mysql-mimic".into()),
        );
        defaults.insert(
            "version_compile_os".into(),
            Value::String(std::env::consts::OS.into()),
        );
        defaults.insert(
            "version_compile_machine".into(),
            Value::String(std::env::consts::ARCH.into()),
        );

        // Character set / collation defaults
        defaults.insert(
            "character_set_client".into(),
            Value::String("utf8mb4".into()),
        );
        defaults.insert(
            "character_set_connection".into(),
            Value::String("utf8mb4".into()),
        );
        defaults.insert(
            "character_set_results".into(),
            Value::String("utf8mb4".into()),
        );
        defaults.insert(
            "character_set_server".into(),
            Value::String("utf8mb4".into()),
        );
        defaults.insert(
            "collation_connection".into(),
            Value::String("utf8mb4_general_ci".into()),
        );
        defaults.insert(
            "collation_server".into(),
            Value::String("utf8mb4_general_ci".into()),
        );

        // SQL mode
        defaults.insert("sql_mode".into(), Value::String("ANSI".into()));

        // Timeouts
        defaults.insert("wait_timeout".into(), Value::Int(28800));
        defaults.insert("interactive_timeout".into(), Value::Int(28800));
        defaults.insert("net_write_timeout".into(), Value::Int(28800));

        // Transaction
        defaults.insert(
            "transaction_isolation".into(),
            Value::String("READ-COMMITTED".into()),
        );
        defaults.insert("transaction_read_only".into(), Value::Bool(false));
        defaults.insert("autocommit".into(), Value::Bool(true));

        // Misc
        defaults.insert("max_allowed_packet".into(), Value::Int(67108864)); // 64MB
        defaults.insert("sql_auto_is_null".into(), Value::Bool(false));
        defaults.insert("sql_select_limit".into(), Value::Null);
        defaults.insert("lower_case_table_names".into(), Value::Int(0));
        defaults.insert("system_time_zone".into(), Value::String("UTC".into()));
        defaults.insert("time_zone".into(), Value::String("UTC".into()));
        defaults.insert("init_connect".into(), Value::String(String::new()));
        defaults.insert("license".into(), Value::String("MIT".into()));
        defaults.insert("performance_schema".into(), Value::Int(0));
        defaults.insert("auto_increment_increment".into(), Value::Int(1));
        defaults.insert("external_user".into(), Value::Null);

        // Query cache (removed in MySQL 8.0 but clients still ask for them)
        defaults.insert("query_cache_size".into(), Value::Int(0));
        defaults.insert("query_cache_type".into(), Value::String("OFF".into()));

        // Legacy alias for transaction_isolation (MySQL < 8.0)
        defaults.insert(
            "tx_isolation".into(),
            Value::String("READ-COMMITTED".into()),
        );

        GlobalVariables { defaults }
    }

    /// Get a global default variable value.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.defaults.get(&name.to_lowercase())
    }

    /// Set a global default variable value.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<Value>) {
        self.defaults
            .insert(name.into().to_lowercase(), value.into());
    }
}

/// Session-level variables that override global defaults.
#[derive(Debug, Clone)]
pub struct SessionVariables {
    globals: Arc<GlobalVariables>,
    overrides: HashMap<String, Value>,
}

impl SessionVariables {
    /// Create a new `SessionVariables` backed by the given global defaults.
    pub fn new(globals: Arc<GlobalVariables>) -> Self {
        SessionVariables {
            globals,
            overrides: HashMap::new(),
        }
    }

    /// Get a variable value. Session override takes precedence over global default.
    pub fn get(&self, name: &str) -> Option<&Value> {
        let key = name.to_lowercase();
        self.overrides.get(&key).or_else(|| self.globals.get(&key))
    }

    /// Set a session variable override.
    ///
    /// If `value` is "DEFAULT" or "NULL" (case-insensitive string), the override
    /// is removed so the global default is used again.
    pub fn set(&mut self, name: impl Into<String>, value: Value) {
        let key = name.into().to_lowercase();
        if let Value::Null = &value {
            // Setting to NULL restores default
            self.overrides.remove(&key);
            return;
        }
        self.overrides.insert(key, value);
    }

    /// Remove a session override, reverting to the global default.
    pub fn reset(&mut self, name: &str) {
        self.overrides.remove(&name.to_lowercase());
    }

    /// Remove all session overrides.
    pub fn reset_all(&mut self) {
        self.overrides.clear();
    }

    /// List all variables (merged global + session).
    pub fn list(&self) -> Vec<(String, Value)> {
        let mut result: HashMap<String, Value> = HashMap::new();
        // Start with global defaults
        for (k, v) in &self.globals.defaults {
            result.insert(k.clone(), v.clone());
        }
        // Apply session overrides
        for (k, v) in &self.overrides {
            result.insert(k.clone(), v.clone());
        }
        let mut list: Vec<_> = result.into_iter().collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_defaults() {
        let globals = GlobalVariables::new();
        assert_eq!(
            globals.get("version"),
            Some(&Value::String("8.0.0-mysql-mimic".into()))
        );
        assert_eq!(
            globals.get("character_set_client"),
            Some(&Value::String("utf8mb4".into()))
        );
    }

    #[test]
    fn test_session_overrides() {
        let globals = Arc::new(GlobalVariables::new());
        let mut session = SessionVariables::new(globals);

        // Default value
        assert_eq!(session.get("sql_mode"), Some(&Value::String("ANSI".into())));

        // Override
        session.set("sql_mode", Value::String("TRADITIONAL".into()));
        assert_eq!(
            session.get("sql_mode"),
            Some(&Value::String("TRADITIONAL".into()))
        );

        // Reset to default
        session.reset("sql_mode");
        assert_eq!(session.get("sql_mode"), Some(&Value::String("ANSI".into())));
    }

    #[test]
    fn test_case_insensitive() {
        let globals = Arc::new(GlobalVariables::new());
        let session = SessionVariables::new(globals);
        assert_eq!(session.get("SQL_MODE"), session.get("sql_mode"));
    }

    #[test]
    fn test_list_variables() {
        let globals = Arc::new(GlobalVariables::new());
        let session = SessionVariables::new(globals);
        let list = session.list();
        assert!(!list.is_empty());
        // Should be sorted
        let names: Vec<_> = list.iter().map(|(k, _)| k.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }
}
