use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "sway-stage-profile-capture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn partial_publication_keeps_complete_layouts_and_never_exposes_profile() {
    for blocked_index in [1, 2] {
        let directory = Directory::new();
        let files: Vec<_> = ["work-001", "work-002", "work"]
            .into_iter()
            .map(|name| DraftFile {
                destination: directory.0.join(format!("{name}.yaml")),
                yaml: "version: 1\ncommand: null\n".into(),
            })
            .collect();
        preflight_files(&files).unwrap();
        let staging: Vec<_> = files
            .iter()
            .map(|file| draft_file::stage_draft(&directory.0, &file.yaml).unwrap())
            .collect();
        fs::write(&files[blocked_index].destination, "concurrent file").unwrap();
        let error = publish_prepared(&files, &staging).unwrap_err();
        assert!(error.contains("profile was not published"), "{error}");
        assert!(error.contains(files[0].destination.to_str().unwrap()));
        assert_eq!(
            fs::read_to_string(&files[0].destination).unwrap(),
            files[0].yaml
        );
        assert_eq!(
            fs::read_to_string(&files[blocked_index].destination).unwrap(),
            "concurrent file"
        );
        if blocked_index == 1 {
            assert!(!files[2].destination.exists());
        }
        for path in staging {
            assert!(path.exists());
        }
    }
}

#[test]
fn missing_staging_file_stops_publication_without_rollback() {
    let directory = Directory::new();
    let files: Vec<_> = ["layout", "profile"]
        .into_iter()
        .map(|name| DraftFile {
            destination: directory.0.join(format!("{name}.yaml")),
            yaml: "complete".into(),
        })
        .collect();
    let staging = vec![
        draft_file::stage_draft(&directory.0, "complete").unwrap(),
        directory.0.join("absent.partial"),
    ];
    let error = publish_prepared(&files, &staging).unwrap_err();
    assert!(error.contains("generated files may remain"));
    assert!(files[0].destination.exists());
    assert!(!files[1].destination.exists());
}

#[test]
fn explicit_workspace_count_limit_and_invalid_selection_methods_precede_observation() {
    let arguments = vec!["music".to_owned(); MAX_WORKSPACES + 1];
    assert!(
        parse_targets(&arguments, false)
            .unwrap_err()
            .contains("at most 256")
    );
    assert!(parse_targets(&[], false).is_err());
    assert!(parse_targets(&["music".into()], true).is_err());
}
