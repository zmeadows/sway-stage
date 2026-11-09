use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

pub struct Directory {
    pub path: PathBuf,
    explicit: bool,
}

impl Directory {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self, String> {
        Self::from_bases(
            explicit,
            env::var_os("XDG_CONFIG_HOME"),
            env::var_os("HOME"),
        )
    }

    fn from_bases(
        explicit: Option<PathBuf>,
        xdg: Option<OsString>,
        home: Option<OsString>,
    ) -> Result<Self, String> {
        if let Some(path) = explicit {
            if path.as_os_str().is_empty() {
                return Err("--layouts-dir must not be empty".into());
            }
            return Ok(Self {
                path,
                explicit: true,
            });
        }

        let base = if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
            let path = PathBuf::from(xdg);
            if !path.is_absolute() {
                return Err("XDG_CONFIG_HOME must be absolute; set it or use --layouts-dir".into());
            }
            path
        } else {
            let home = home.filter(|value| !value.is_empty()).ok_or(
                "neither XDG_CONFIG_HOME nor HOME is usable; set one or use --layouts-dir",
            )?;
            let path = PathBuf::from(home);
            if !path.is_absolute() {
                return Err("HOME must be absolute; set it or use --layouts-dir".into());
            }
            path.join(".config")
        };
        Ok(Self {
            path: base.join("sway-stage/layouts"),
            explicit: false,
        })
    }

    pub fn discover(&self) -> Result<BTreeMap<String, PathBuf>, String> {
        let entries = match fs::read_dir(&self.path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !self.explicit => {
                return Ok(BTreeMap::new());
            }
            Err(error) => return Err(format!("read directory {}: {error}", self.path.display())),
        };

        // Sort before checking entries so even failure diagnostics are deterministic.
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("read directory {}: {error}", self.path.display()))?;
            let path = entry.path();
            let filename = entry.file_name();
            let bytes = filename.as_encoded_bytes();
            if !bytes.ends_with(b".yaml") && !bytes.ends_with(b".yml") {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|error| format!("inspect {}: {error}", path.display()))?;
            if file_type.is_file() {
                paths.push(path);
            }
        }
        paths.sort();

        let mut layouts: BTreeMap<String, PathBuf> = BTreeMap::new();
        for path in paths {
            let name = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                format!(
                    "invalid layout filename {}; names must be ASCII",
                    path.display()
                )
            })?;
            if !valid_name(name) {
                return Err(format!(
                    "invalid layout name in {}; use [A-Za-z0-9][A-Za-z0-9_-]*",
                    path.display()
                ));
            }
            if let Some(previous) = layouts.get(name) {
                return Err(format!(
                    "duplicate layout name {name:?}: {} and {}; rename one file",
                    previous.display(),
                    path.display()
                ));
            }
            layouts.insert(name.to_owned(), path);
        }
        Ok(layouts)
    }
}

pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
