use serde::Serialize;
use tabled::Tabled;

/// Output format for CLI commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Human-readable table (default).
    #[default]
    Table,
    /// JSON output.
    Json,
    /// YAML output.
    Yaml,
}

impl OutputFormat {
    /// Parse from CLI string argument.
    pub fn from_str_arg(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            _ => Self::Table,
        }
    }
}

/// Render a list of items in the specified output format.
pub fn render_list<T: Serialize + Tabled>(items: &[T], format: OutputFormat) {
    match format {
        OutputFormat::Table => {
            if items.is_empty() {
                println!("(none)");
            } else {
                let table = tabled::Table::new(items)
                    .with(tabled::settings::Style::rounded())
                    .to_string();
                println!("{}", table);
            }
        }
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(items).unwrap_or_default()
            );
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(items).unwrap_or_default());
        }
    }
}

/// Render a single item in the specified output format.
pub fn render_one<T: Serialize + Tabled>(item: &T, format: OutputFormat) {
    match format {
        OutputFormat::Table => {
            let table = tabled::Table::new(std::iter::once(item))
                .with(tabled::settings::Style::rounded())
                .to_string();
            println!("{}", table);
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(item).unwrap_or_default());
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(item).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_format_from_str() {
        assert_eq!(OutputFormat::from_str_arg("json"), OutputFormat::Json);
        assert_eq!(OutputFormat::from_str_arg("JSON"), OutputFormat::Json);
        assert_eq!(OutputFormat::from_str_arg("yaml"), OutputFormat::Yaml);
        assert_eq!(OutputFormat::from_str_arg("yml"), OutputFormat::Yaml);
        assert_eq!(OutputFormat::from_str_arg("table"), OutputFormat::Table);
        assert_eq!(OutputFormat::from_str_arg("anything"), OutputFormat::Table);
    }

    #[derive(Serialize, Tabled)]
    struct TestItem {
        name: String,
        count: u32,
    }

    #[test]
    fn test_render_list_json() {
        let items = vec![
            TestItem {
                name: "a".to_string(),
                count: 1,
            },
            TestItem {
                name: "b".to_string(),
                count: 2,
            },
        ];
        // Just verify it doesn't panic
        render_list(&items, OutputFormat::Json);
    }

    #[test]
    fn test_render_list_yaml() {
        let items = vec![TestItem {
            name: "x".to_string(),
            count: 42,
        }];
        render_list(&items, OutputFormat::Yaml);
    }

    #[test]
    fn test_render_list_table() {
        let items = vec![TestItem {
            name: "y".to_string(),
            count: 99,
        }];
        render_list(&items, OutputFormat::Table);
    }

    #[test]
    fn test_render_empty_list() {
        let items: Vec<TestItem> = vec![];
        render_list(&items, OutputFormat::Table);
    }

    #[test]
    fn test_render_one_json() {
        let item = TestItem {
            name: "z".to_string(),
            count: 7,
        };
        render_one(&item, OutputFormat::Json);
    }
}
