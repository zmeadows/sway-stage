use std::os::unix::ffi::OsStringExt;

use super::*;

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn the_interface_is_no_arguments_or_one_filter() {
    assert_eq!(parse_filter(&[]).unwrap(), None);
    assert_eq!(
        parse_filter(&arguments(&["--filter", "pair"]))
            .unwrap()
            .as_deref(),
        Some("pair")
    );
}

#[test]
fn removed_unknown_and_incomplete_arguments_are_refused() {
    for refused in [
        &["--filter"][..],
        &["--filter", ""],
        &["--filter=pair"],
        &["--filter", "pair", "--filter", "single"],
        &["--filter", "pair", "extra"],
        &["pair"],
        &["--unknown"],
        &["--help"],
        // The live-desktop options are gone, not ignored.
        &["--workspace", "9"],
        &["--foreign-workspace", "8"],
        &["--profile-workspace", "10"],
        &["--binary", "/a path/sway-stage"],
    ] {
        assert!(parse_filter(&arguments(refused)).is_err(), "{refused:?}");
    }

    let non_utf8_filter = [
        OsString::from("--filter"),
        OsString::from_vec(b"pair\xff".to_vec()),
    ];
    assert!(
        parse_filter(&non_utf8_filter)
            .unwrap_err()
            .contains("UTF-8")
    );
}

#[test]
fn the_runner_role_is_a_private_flag_outside_the_interface() {
    assert!(parse_filter(&arguments(&[RUNNER_FLAG])).is_err());
    assert_ne!(RUNNER_FLAG, crate::process::PROBE_FLAG);
}
