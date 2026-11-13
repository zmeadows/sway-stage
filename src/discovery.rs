use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub struct LayoutDirectory {
    pub path: PathBuf,
    is_explicit: bool,
}

/// Recoverable filename problems stay separate from fatal directory I/O errors.
#[derive(Default)]
pub struct Discovery {
    pub layouts: BTreeMap<String, PathBuf>,
    pub ambiguities: BTreeMap<String, String>,
    invalid_filenames: Vec<String>,
}

impl Discovery {
    pub fn warnings(&self) -> impl Iterator<Item = &str> {
        self.invalid_filenames
            .iter()
            .chain(self.ambiguities.values())
            .map(String::as_str)
    }
}

impl LayoutDirectory {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self, String> {
        if let Some(path) = explicit {
            if path.as_os_str().is_empty() {
                return Err("--layouts-dir must not be empty".into());
            }

            return Ok(Self {
                path,
                is_explicit: true,
            });
        }

        Ok(Self {
            path: config_home()?.join("sway-stage/layouts"),
            is_explicit: false,
        })
    }

    pub fn discover(&self) -> Result<Discovery, String> {
        let layout_paths = yaml_paths(&self.path, !self.is_explicit)?;

        let mut discovery = Discovery::default();
        let mut paths_by_name: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for path in layout_paths {
            let name = path.file_stem().and_then(|stem| stem.to_str());
            let Some(name) = name.filter(|name| valid_name(name)) else {
                discovery.invalid_filenames.push(format!(
                    "skipping invalid layout filename {:?}; use [A-Za-z0-9][A-Za-z0-9_-]* with .yaml or .yml",
                    path.file_name().expect("directory entry has a filename")
                ));
                continue;
            };

            paths_by_name.entry(name.to_owned()).or_default().push(path);
        }

        for (name, mut paths) in paths_by_name {
            if paths.len() == 1 {
                discovery.layouts.insert(name, paths.remove(0));
            } else {
                let filenames = paths
                    .iter()
                    .map(|path| format!("{:?}", path.file_name().expect("entry has a filename")))
                    .collect::<Vec<_>>()
                    .join(" and ");
                discovery.ambiguities.insert(
                    name.clone(),
                    format!("ambiguous layout name {name:?}: {filenames}; rename one file"),
                );
            }
        }

        Ok(discovery)
    }
}

/// Immediate regular YAML files in deterministic order; never follows file symlinks.
pub(crate) fn yaml_paths(directory: &Path, missing_is_empty: bool) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && missing_is_empty => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("read directory {}: {error}", directory.display())),
    };

    // Sort eligible paths so filename diagnostics do not depend on enumeration order.
    let mut yaml_paths = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("read directory {}: {error}", directory.display()))?;
        let path = entry.path();
        let filename = entry.file_name();
        let filename_bytes = filename.as_encoded_bytes();
        if !filename_bytes.ends_with(b".yaml") && !filename_bytes.ends_with(b".yml") {
            continue;
        }

        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", path.display()))?;
        if file_type.is_file() {
            yaml_paths.push(path);
        }
    }
    yaml_paths.sort();

    Ok(yaml_paths)
}

/// XDG configuration base containing layout and profile directories.
pub(crate) fn config_home() -> Result<PathBuf, String> {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(xdg);
        if !path.is_absolute() {
            return Err("XDG_CONFIG_HOME must be absolute".into());
        }

        return Ok(path);
    }

    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or("neither XDG_CONFIG_HOME nor HOME is usable; set one")?;
    let path = PathBuf::from(home);
    if !path.is_absolute() {
        return Err("HOME must be absolute".into());
    }

    Ok(path.join(".config"))
}

pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
