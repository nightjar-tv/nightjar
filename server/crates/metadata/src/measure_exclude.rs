//! Library-name exclusion for **measure harnesses only**.
//!
//! Product drain never reads this. The exclude list is machine config
//! (`MEASURE_EXCLUDE_LIBRARY_NAMES`); the mechanism (filter proxy / skip drain)
//! is code. Dogfood default matches this host's synthetic libs.

/// Env var: comma-separated library names to omit when `EXCLUDE_TESTDATA=1`.
pub const MEASURE_EXCLUDE_LIBRARY_NAMES_ENV: &str = "MEASURE_EXCLUDE_LIBRARY_NAMES";

/// Dogfood default when the env var is unset or empty.
pub const MEASURE_EXCLUDE_LIBRARY_NAMES_DEFAULT: &str = "Test Data,DV,DV2";

/// Names from `MEASURE_EXCLUDE_LIBRARY_NAMES`, or the dogfood default.
pub fn measure_exclude_library_names() -> Vec<String> {
    measure_exclude_library_names_from(
        std::env::var(MEASURE_EXCLUDE_LIBRARY_NAMES_ENV)
            .ok()
            .as_deref(),
    )
}

fn measure_exclude_library_names_from(raw: Option<&str>) -> Vec<String> {
    let s = match raw {
        Some(v) if !v.trim().is_empty() => v,
        _ => MEASURE_EXCLUDE_LIBRARY_NAMES_DEFAULT,
    };
    s.split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// SQL `name IN (...)` fragment for a name list (harness-built constants only).
pub fn measure_exclude_libraries_sql_in(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("'{}'", n.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_list_is_dogfood_synthetic_libs() {
        let names = measure_exclude_library_names_from(None);
        assert_eq!(
            names,
            vec!["Test Data".to_string(), "DV".to_string(), "DV2".to_string()]
        );
    }

    #[test]
    fn env_overrides_default() {
        let names = measure_exclude_library_names_from(Some("Scratch, Other "));
        assert_eq!(names, vec!["Scratch".to_string(), "Other".to_string()]);
    }

    #[test]
    fn empty_env_falls_back_to_default() {
        let names = measure_exclude_library_names_from(Some("  "));
        assert_eq!(names.len(), 3);
    }
}
