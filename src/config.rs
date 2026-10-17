use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Deserializer};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE_NAME: &str = "ron.toml";

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    /// Map glob pattern to file path
    #[serde(default)]
    types: Vec<TypePattern>,

    #[serde(default)]
    pub root_dir: Option<PathBuf>,
}

impl Config {
    /// Load config from the ron.toml file, if present. Returns the default config otherwise
    pub fn try_load_from_dir<P: AsRef<Path>>(dir: P) -> anyhow::Result<Option<Self>> {
        let path = dir.as_ref().join(CONFIG_FILE_NAME);

        let config = if path.exists() && path.is_file() {
            let text = fs::read_to_string(&path)?;
            let mut config: Config = toml::from_str(&text)?;
            config.root_dir.get_or_insert(PathBuf::from(dir.as_ref()));
            Some(config)
        } else {
            None
        };

        Ok(config)
    }

    pub fn match_module_path<P: AsRef<Path>>(&self, file_path: P) -> Option<&String> {
        let file_path = file_path.as_ref();
        let rel = self
            .root_dir
            .as_ref()
            .and_then(|root| file_path.strip_prefix(root).ok())
            .unwrap_or(file_path);

        self.types.iter().find_map(
            |TypePattern { glob, path }| {
                if glob.is_match(rel) {
                    Some(path)
                } else {
                    None
                }
            },
        )
    }
}

#[derive(Debug, Deserialize)]
struct TypePattern {
    #[serde(deserialize_with = "deserialize_glob")]
    glob: GlobMatcher,
    #[serde(rename = "type")]
    path: String,
}

fn deserialize_glob<'de, D>(deserializer: D) -> Result<GlobMatcher, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer)
        .and_then(|glob| Glob::new(&glob).map_err(serde::de::Error::custom))
        .map(|glob| glob.compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create an empty temporary directory unique to `name` and this process.
    fn temp_dir(name: &str) -> PathBuf {
        let dir_name = format!("ron-lsp-config-{}-{}", name, std::process::id());
        let dir = std::env::temp_dir().join(dir_name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_try_load_from_dir_without_config_file() {
        let dir = temp_dir("missing-file");

        assert!(Config::try_load_from_dir(&dir).unwrap().is_none());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_try_load_from_dir_ignores_directory_named_ron_toml() {
        let dir = temp_dir("ron-toml-is-dir");
        fs::create_dir(dir.join(CONFIG_FILE_NAME)).unwrap();

        assert!(Config::try_load_from_dir(&dir).unwrap().is_none());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_try_load_from_dir_defaults_root_dir_to_config_dir() {
        let dir = temp_dir("default-root");
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            "[[types]]\nglob = \"root.ron\"\ntype = \"crate::models::Root\"\n",
        )
        .unwrap();

        let config = Config::try_load_from_dir(&dir).unwrap().unwrap();

        // A config without root_dir is rooted at the directory it was loaded from
        assert_eq!(config.root_dir.as_deref(), Some(dir.as_path()));
        assert_eq!(
            config.match_module_path(dir.join("root.ron")),
            Some(&"crate::models::Root".to_string())
        );
        assert_eq!(config.match_module_path(dir.join("item.ron")), None);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_try_load_from_dir_keeps_configured_root_dir() {
        let config_dir = temp_dir("explicit-root-config");
        let project_dir = temp_dir("explicit-root-project");
        // Forward slashes keep the temporary path out of TOML escape sequences
        let root = project_dir.display().to_string().replace('\\', "/");
        fs::write(
            config_dir.join(CONFIG_FILE_NAME),
            format!(
                "root_dir = \"{}\"\n\n[[types]]\nglob = \"root.ron\"\ntype = \"crate::models::Root\"\n",
                root
            ),
        )
        .unwrap();

        let config = Config::try_load_from_dir(&config_dir).unwrap().unwrap();

        assert_eq!(config.root_dir.as_deref(), Some(project_dir.as_path()));
        // Paths under the configured root are matched relative to it
        assert_eq!(
            config.match_module_path(project_dir.join("root.ron")),
            Some(&"crate::models::Root".to_string())
        );
        // Paths outside the root are matched as-is, so a relative glob misses
        assert_eq!(config.match_module_path(config_dir.join("root.ron")), None);

        fs::remove_dir_all(&config_dir).unwrap();
        fs::remove_dir_all(&project_dir).unwrap();
    }

    #[test]
    fn test_empty_config_matches_nothing() {
        let dir = temp_dir("empty-config");
        fs::write(dir.join(CONFIG_FILE_NAME), "").unwrap();

        let config = Config::try_load_from_dir(&dir).unwrap().unwrap();

        assert_eq!(config.root_dir.as_deref(), Some(dir.as_path()));
        assert_eq!(config.match_module_path(dir.join("root.ron")), None);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_try_load_from_dir_rejects_invalid_glob() {
        let dir = temp_dir("invalid-glob");
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            "[[types]]\nglob = \"[\"\ntype = \"crate::models::Root\"\n",
        )
        .unwrap();

        assert!(Config::try_load_from_dir(&dir).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_match_module_path() {
        // Use PathBuf::from components to build platform-appropriate paths
        let root_dir: PathBuf = ["my", "project"].iter().collect();
        let config = Config {
            types: vec![TypePattern {
                glob: Glob::new("**/post.ron").unwrap().compile_matcher(),
                path: "crate::models::Post".to_string(),
            }],
            root_dir: Some(root_dir.clone()),
        };

        let matching_path: PathBuf = root_dir.join("example").join("data").join("post.ron");
        let result = config.match_module_path(&matching_path);
        assert_eq!(result, Some(&"crate::models::Post".to_string()));

        let non_matching_path: PathBuf = root_dir.join("example").join("data").join("user.ron");
        let result = config.match_module_path(&non_matching_path);
        assert_eq!(result, None);
    }
}
