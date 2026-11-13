use crate::{config, discovery};
use serde::Deserialize;
use serde_saphyr::Spanned;
use std::collections::HashSet;
use std::path::PathBuf;

// Match the layout node limit: profiles are bounded plans for interactive construction.
const MAX_WORKSPACES: usize = 256;
const MAX_WORKSPACE_NUMBER: u64 = 999_999_999;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(deserialize_with = "config::unquoted_integer")]
    version: u64,
    pub workspaces: Vec<Spanned<WorkspaceLayout>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLayout {
    #[serde(deserialize_with = "config::unquoted_integer")]
    pub workspace: u64,
    pub layout: String,
}

/// Resolve and validate the profile and every referenced layout, entirely offline.
pub fn read_named(
    name: &str,
    profiles_dir: Option<PathBuf>,
    layouts_dir: Option<PathBuf>,
) -> Result<Profile, String> {
    let path = find_profile(name, profiles_dir)?;
    let input = config::read_yaml(&path, "profile")?;
    let profile = parse(&input).map_err(|error| format!("{}: {error}", path.display()))?;

    let layout_directory = discovery::LayoutDirectory::resolve(layouts_dir)?;
    let layouts = layout_directory.discover()?;
    for (entry_index, entry) in profile.workspaces.iter().enumerate() {
        let layout_name = &entry.value.layout;
        let location = entry.referenced;
        let context = format!(
            "{}: workspaces[{entry_index}].layout (workspace {}, line {}, column {})",
            path.display(),
            entry.value.workspace,
            location.line(),
            location.column()
        );
        if let Some(error) = layouts.ambiguities.get(layout_name) {
            return Err(format!("{context}: {error}"));
        }
        let Some(layout_path) = layouts.layouts.get(layout_name) else {
            return Err(format!(
                "{context}: layout {layout_name:?} not found in {}; use list to see available layouts",
                layout_directory.path.display()
            ));
        };

        config::read(layout_path)
            .map_err(|error| format!("{context}: invalid layout {layout_name:?}: {error}"))?;
    }

    Ok(profile)
}

fn find_profile(name: &str, profiles_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    if !discovery::valid_name(name) {
        return Err(format!(
            "invalid profile name {name:?}; use [A-Za-z0-9][A-Za-z0-9_-]*"
        ));
    }

    let is_explicit = profiles_dir.is_some();
    let directory = match profiles_dir {
        Some(path) => {
            if path.as_os_str().is_empty() {
                return Err("--profiles-dir must not be empty".into());
            }
            path
        }
        None => discovery::config_home()?.join("sway-stage/profiles"),
    };

    let mut matching_paths = Vec::new();
    for path in discovery::yaml_paths(&directory, !is_explicit)? {
        if path.file_stem().and_then(|stem| stem.to_str()) == Some(name) {
            matching_paths.push(path);
        }
    }

    match matching_paths.as_slice() {
        [] => Err(format!(
            "profile {name:?} not found in {}",
            directory.display()
        )),
        [path] => Ok(path.clone()),
        _ => {
            let filenames = matching_paths
                .iter()
                .map(|path| format!("{:?}", path.file_name().expect("entry has a filename")))
                .collect::<Vec<_>>()
                .join(" and ");
            Err(format!(
                "ambiguous profile name {name:?}: {filenames}; rename one file"
            ))
        }
    }
}

fn parse(input: &str) -> Result<Profile, String> {
    if input.len() > config::MAX_INPUT_BYTES {
        return Err("input exceeds the 1 MiB limit".into());
    }

    let mut field_path = String::new();
    let profile: Profile = serde_saphyr::with_deserializer_from_str_with_options(
        input,
        config::yaml_options(),
        |deserializer| {
            serde_path_to_error::deserialize(deserializer).map_err(|error| {
                // Spanned contributes a synthetic `value` field to Serde's path.
                field_path = error.path().to_string().replace("].value", "]");
                error.into_inner()
            })
        },
    )
    .map_err(|error| {
        if field_path.is_empty() {
            field_path = "document".into();
        }
        format!("{field_path}: {error}")
    })?;

    if profile.version != 1 {
        return Err("version: expected format version 1".into());
    }
    if profile.workspaces.is_empty() || profile.workspaces.len() > MAX_WORKSPACES {
        return Err(format!(
            "workspaces: expected from 1 to {MAX_WORKSPACES} entries"
        ));
    }

    let mut workspace_numbers = HashSet::new();
    for (entry_index, entry) in profile.workspaces.iter().enumerate() {
        let location = entry.referenced;
        let fail = |field: &str, message: &str| {
            format!(
                "workspaces[{entry_index}].{field} (line {}, column {}): {message}",
                location.line(),
                location.column()
            )
        };

        let workspace_number = entry.value.workspace;
        if !(1..=MAX_WORKSPACE_NUMBER).contains(&workspace_number) {
            return Err(fail(
                "workspace",
                &format!("expected a workspace number from 1 to {MAX_WORKSPACE_NUMBER}"),
            ));
        }
        if !workspace_numbers.insert(workspace_number) {
            return Err(fail(
                "workspace",
                &format!("duplicate workspace number {workspace_number}"),
            ));
        }
        if !discovery::valid_name(&entry.value.layout) {
            return Err(fail(
                "layout",
                "expected a layout name matching [A-Za-z0-9][A-Za-z0-9_-]*",
            ));
        }
    }

    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROFILE: &str = "version: 1\nworkspaces:\n  - workspace: 3\n    layout: development\n  - workspace: 1\n    layout: browser\n";

    fn rejects(input: &str, expected: &str) {
        let error = parse(input).expect_err(input);
        assert!(
            error.contains(expected),
            "expected {expected:?}, got {error:?}"
        );
    }

    #[test]
    fn preserves_workspace_order_and_allows_repeated_layouts() {
        let profile = parse(PROFILE).unwrap();
        let entries: Vec<_> = profile
            .workspaces
            .iter()
            .map(|entry| (entry.value.workspace, entry.value.layout.as_str()))
            .collect();
        assert_eq!(entries, [(3, "development"), (1, "browser")]);
        parse(&PROFILE.replace("browser", "development")).unwrap();
        parse("version: 1\nworkspaces: [{workspace: 999999999, layout: '123'}]").unwrap();
    }

    #[test]
    fn requires_unquoted_version_one_and_workspace_integers() {
        for version in ["0", "2", "999999999", "'1'", "1.0", "true", "null", "-1"] {
            rejects(
                &PROFILE.replace("version: 1", &format!("version: {version}")),
                "version",
            );
        }
        for workspace in [
            "0",
            "1000000000",
            "-1",
            "'3'",
            "3.0",
            "true",
            "null",
            "18446744073709551616",
        ] {
            rejects(
                &PROFILE.replace("workspace: 3", &format!("workspace: {workspace}")),
                "workspaces[0].workspace",
            );
        }
    }

    #[test]
    fn rejects_duplicate_workspaces_with_the_second_entry_location() {
        let error = parse(&PROFILE.replace("workspace: 1", "workspace: 3")).unwrap_err();
        for expected in [
            "workspaces[1].workspace",
            "duplicate workspace number 3",
            "line 5",
            "column 5",
        ] {
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn rejects_missing_fields_wrong_shapes_and_bad_layout_names() {
        for input in [
            "",
            "version: 1",
            "workspaces: []",
            "version: 1\nworkspaces: []",
            "version: 1\nworkspaces: null",
            "version: 1\nworkspaces: {}",
            "version: 1\nworkspaces: [{workspace: 1}]",
            "version: 1\nworkspaces: [{layout: development}]",
            "version: 1\nworkspaces: [null]",
        ] {
            assert!(parse(input).is_err(), "accepted {input}");
        }
        for layout in [
            "''",
            "null",
            "123",
            "true",
            "'../escape'",
            "'bad name'",
            "'layout.yaml'",
            "'_bad'",
            "'é'",
        ] {
            rejects(
                &PROFILE.replace("layout: development", &format!("layout: {layout}")),
                "workspaces[0].layout",
            );
        }
    }

    #[test]
    fn rejects_unknown_fields_and_duplicate_keys_at_both_levels() {
        for (input, expected) in [
            (format!("{PROFILE}extra: true\n"), "unknown field"),
            (
                PROFILE.replace("workspace: 3", "extra: true\n    workspace: 3"),
                "unknown field",
            ),
            (format!("version: 1\n{PROFILE}"), "duplicate"),
            (format!("{PROFILE}workspaces: []\n"), "duplicate"),
            (
                PROFILE.replace("workspace: 3", "workspace: 3\n    workspace: 3"),
                "duplicate",
            ),
            (
                PROFILE.replace(
                    "layout: development",
                    "layout: development\n    layout: browser",
                ),
                "duplicate",
            ),
        ] {
            rejects(&input, expected);
        }
    }

    #[test]
    fn rejects_malformed_yaml_and_unsupported_constructs() {
        for input in [
            "version: 1\nworkspaces: [unterminated".into(),
            format!("{PROFILE}---\n{PROFILE}"),
            format!("{PROFILE}---\n"),
            format!("!custom\n{PROFILE}"),
            PROFILE.replace("workspace: 3", "workspace: !custom 3"),
            PROFILE.replace("layout: development", "!custom layout: development"),
            PROFILE
                .replace("layout: development", "layout: &name development")
                .replace("layout: browser", "layout: *name"),
            "version: 1\nworkspaces: [{<<: {workspace: 1}, layout: development}]".into(),
        ] {
            assert!(parse(&input).is_err(), "accepted {input}");
        }
        let error =
            parse("version: 1\nworkspaces:\n  - workspace: 1\n    layout: null\n").unwrap_err();
        for expected in ["workspaces[0].layout", "line", "column"] {
            assert!(error.contains(expected), "{error}");
        }
        assert!(!error.contains(".value"), "{error}");
    }

    #[test]
    fn bounds_input_and_entry_count() {
        rejects(&" ".repeat(config::MAX_INPUT_BYTES + 1), "1 MiB");
        let mut input = String::from("version: 1\nworkspaces:\n");
        for workspace in 1..=MAX_WORKSPACES {
            input.push_str(&format!(
                "  - {{workspace: {workspace}, layout: development}}\n"
            ));
        }
        assert_eq!(parse(&input).unwrap().workspaces.len(), MAX_WORKSPACES);
        input.push_str("  - {workspace: 257, layout: development}\n");
        rejects(&input, "from 1 to 256 entries");
    }

    #[test]
    fn reads_profile_and_validates_every_referenced_layout() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "sway-stage-profile-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let layouts_dir = directory.join("layouts");
        fs::create_dir(&layouts_dir).unwrap();
        let profile_path = directory.join("work.yaml");
        fs::write(&profile_path, PROFILE).unwrap();
        fs::write(
            layouts_dir.join("development.yaml"),
            "version: 1\nlayout: {id: t, command: [not-installed], match: {app_id: t}}\n",
        )
        .unwrap();

        let read = || read_named("work", Some(directory.clone()), Some(layouts_dir.clone()));
        let error = read().unwrap_err();
        assert!(error.contains("workspaces[1]"), "{error}");
        assert!(error.contains("layout \"browser\" not found"), "{error}");

        fs::write(
            layouts_dir.join("browser.yml"),
            "version: 1\nlayout: {id: t, command: [], match: {app_id: t}}\n",
        )
        .unwrap();
        let error = read().unwrap_err();
        for expected in [
            "work.yaml",
            "workspaces[1]",
            "browser.yml",
            "command",
            "line",
            "column",
        ] {
            assert!(error.contains(expected), "{error}");
        }

        fs::copy(
            layouts_dir.join("development.yaml"),
            layouts_dir.join("browser.yml"),
        )
        .unwrap();
        assert_eq!(read().unwrap().workspaces[0].value.workspace, 3);
        fs::write(layouts_dir.join("browser.yaml"), "").unwrap();
        assert!(
            read()
                .unwrap_err()
                .contains("ambiguous layout name \"browser\"")
        );

        fs::write(&profile_path, " ".repeat(config::MAX_INPUT_BYTES + 1)).unwrap();
        assert!(read().unwrap_err().contains("1 MiB"));
        fs::write(&profile_path, [0xff]).unwrap();
        assert!(read().unwrap_err().contains("UTF-8"));
        fs::remove_dir_all(directory).unwrap();
    }
}
