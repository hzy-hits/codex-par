use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const MAX_VALUE_BYTES: usize = 1024;
const MAX_PREAMBLE_BYTES: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_and_read_fact() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        store
            .write_fact("repo", "my-value".to_string(), None)
            .unwrap();

        let facts = store.read_all().unwrap();
        assert_eq!(facts["repo"].value, "my-value");
    }

    #[test]
    fn overwrite_same_key() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        store.write_fact("k", "v1".to_string(), None).unwrap();
        store.write_fact("k", "v2".to_string(), None).unwrap();

        let facts = store.read_all().unwrap();
        assert_eq!(facts["k"].value, "v2");
        assert_eq!(facts.len(), 1);
    }

    #[test]
    fn truncates_value_over_1kib() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        let big = "x".repeat(2000);
        store.write_fact("k", big, None).unwrap();

        let facts = store.read_all().unwrap();
        assert!(facts["k"].value.len() <= 1035);
        assert!(facts["k"].value.ends_with("[truncated]"));
    }

    #[test]
    fn empty_store_returns_none_preamble() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());

        assert!(store.build_preamble().unwrap().is_none());
    }

    #[test]
    fn preamble_contains_key_value_and_path() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        store
            .write_fact("model", "o4-mini".to_string(), None)
            .unwrap();

        let preamble = store.build_preamble().unwrap().unwrap();
        assert!(preamble.contains("model: o4-mini"));
        assert!(preamble.contains("facts.json"));
        assert!(preamble.starts_with("=== Shared Context ==="));
    }

    #[test]
    fn preamble_truncates_at_4kib_total() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());

        for i in 0..20 {
            store
                .write_fact(&format!("key{:02}", i), "x".repeat(300), None)
                .unwrap();
        }

        let preamble = store.build_preamble().unwrap().unwrap();
        assert!(preamble.contains("[additional facts truncated"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactEntry {
    pub value: String,
    pub artifact_path: Option<PathBuf>,
    pub updated_at: DateTime<Local>,
}

pub struct FactsStore {
    run_dir: PathBuf,
}

impl FactsStore {
    pub fn new(run_dir: &Path) -> Self {
        Self {
            run_dir: run_dir.to_path_buf(),
        }
    }

    pub fn write_fact(
        &self,
        key: &str,
        value: String,
        artifact_path: Option<PathBuf>,
    ) -> Result<()> {
        let shared_dir = self.run_dir.join("shared");
        std::fs::create_dir_all(&shared_dir)?;

        let mut facts = self.read_all()?;
        facts.insert(
            key.to_string(),
            FactEntry {
                value: truncate_value(value),
                artifact_path,
                updated_at: Local::now(),
            },
        );

        let facts_path = self.facts_path();
        let tmp_path = facts_path.with_extension("tmp");
        std::fs::write(&tmp_path, serde_json::to_vec_pretty(&facts)?)?;
        std::fs::rename(&tmp_path, &facts_path)?;
        Ok(())
    }

    pub fn read_all(&self) -> Result<BTreeMap<String, FactEntry>> {
        let facts_path = self.facts_path();
        if !facts_path.exists() {
            return Ok(BTreeMap::new());
        }

        let bytes = std::fs::read(facts_path)?;
        let facts = serde_json::from_slice(&bytes)?;
        Ok(facts)
    }

    pub fn build_preamble(&self) -> Result<Option<String>> {
        let facts = self.read_all()?;
        if facts.is_empty() {
            return Ok(None);
        }

        let facts_path = self.facts_path();
        let absolute_facts_path = facts_path.canonicalize().unwrap_or(facts_path);
        let mut lines = Vec::new();
        let mut total_bytes = 0usize;
        let truncation_line = "[additional facts truncated — see facts.json]";

        for (key, entry) in facts {
            let line = match entry.artifact_path {
                Some(path) => format!("{}: {} [artifact: {}]", key, entry.value, path.display()),
                None => format!("{}: {}", key, entry.value),
            };

            let line_bytes = line.len() + 1;
            if total_bytes + line_bytes > MAX_PREAMBLE_BYTES {
                lines.push(truncation_line.to_string());
                break;
            }

            total_bytes += line_bytes;
            lines.push(line);
        }

        let mut preamble = Vec::with_capacity(lines.len() + 3);
        preamble.push("=== Shared Context ===".to_string());
        preamble.extend(lines);
        preamble.push(format!(
            "[Full facts at: {}]",
            absolute_facts_path.display()
        ));
        preamble.push("======================".to_string());
        Ok(Some(preamble.join("\n")))
    }

    fn facts_path(&self) -> PathBuf {
        self.run_dir.join("shared").join("facts.json")
    }
}

fn truncate_value(value: String) -> String {
    if value.len() <= MAX_VALUE_BYTES {
        return value;
    }

    const SUFFIX: &str = "[truncated]";
    let mut end = MAX_VALUE_BYTES.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }

    let mut truncated = String::with_capacity(end + SUFFIX.len());
    truncated.push_str(&value[..end]);
    truncated.push_str(SUFFIX);
    truncated
}
