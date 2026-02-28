use crate::error::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub struct ConfigReader;

impl ConfigReader {
    pub fn read(path: impl AsRef<Path>) -> Result<HashMap<String, String>> {
        let content = fs::read_to_string(path)?;
        let mut config = HashMap::new();

        for line in content.lines() {
            let line = line.trim();

            // Skip empty lines
            if line.is_empty() {
                continue;
            }

            // Handle "# CONFIG_XXX is not set" or "# XXX is not set"
            if line.starts_with('#') && line.ends_with(" is not set") {
                let name = line
                    .trim_start_matches("# ")
                    .trim_end_matches(" is not set");
                // Strip CONFIG_ prefix if present for backward compatibility
                let clean_name = name.strip_prefix("CONFIG_").unwrap_or(name);
                config.insert(clean_name.to_string(), "n".to_string());
                continue;
            }

            // Skip other comments
            if line.starts_with('#') {
                continue;
            }

            // Handle "CONFIG_XXX=value" or "XXX=value"
            if let Some(pos) = line.find('=') {
                let name = line[..pos].trim();
                let value = line[pos + 1..].trim();

                // Remove quotes from string values
                let value = value.trim_matches('"');

                // Strip CONFIG_ prefix if present for backward compatibility
                let clean_name = name.strip_prefix("CONFIG_").unwrap_or(name);

                config.insert(clean_name.to_string(), value.to_string());
            }
        }

        Ok(config)
    }
}
