"""Offline boundary checks: python3 -B -m unittest discover -s tools/ci -v.

No dependency installation, network access, build, or compositor operations.
"""

import contextlib
import copy
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import build_sway


class BuildSwayTests(unittest.TestCase):
    def test_cache_identity_changes_with_build_and_environment_inputs(self):
        inputs = {
            "sources": build_sway.SOURCES["1.10"], "architecture": "amd64",
            "prefix": "/test/prefix", "packages": ["libwayland-dev 1.24.0 amd64"],
            "common_options": build_sway.COMMON_OPTIONS, "runner_image": "image-1",
            "implementation_sha256": "script-hash", "dependencies": {"pixman": "0.46.4"},
        }
        original_key = build_sway.cache_key(inputs)
        self.assertEqual(original_key, build_sway.cache_key(dict(reversed(list(inputs.items())))))
        for field in inputs:
            changed = copy.deepcopy(inputs)
            if field == "sources":
                changed[field] = build_sway.SOURCES["1.11"]
            else:
                changed[field] = "changed"
            with self.subTest(field=field):
                self.assertNotEqual(original_key, build_sway.cache_key(changed))

    def test_source_identity_mismatch_stops_before_checkout(self):
        pin = build_sway.SOURCES["1.10"]["sway"]
        for revisions in (("wrong-tag-object",), (pin[1], "wrong-commit")):
            observed_revisions = iter(revisions)

            def fake_run(arguments, **kwargs):
                if arguments[1] == "rev-parse":
                    return next(observed_revisions)
                return ""

            with (
                self.subTest(revisions=revisions),
                patch.object(build_sway, "run", side_effect=fake_run) as run,
            ):
                with self.assertRaisesRegex(RuntimeError, "expected .* fetched"):
                    build_sway.fetch_source(Path("/unused/source"), "sway", pin)
                self.assertFalse(any(call.args[0][1] == "checkout" for call in run.call_args_list))

    def test_source_checkout_uses_pinned_commit_after_both_checks(self):
        pin = build_sway.SOURCES["1.12"]["wlroots"]
        with patch.object(build_sway, "run", side_effect=["", "", "", pin[1], pin[2], ""]) as run:
            build_sway.fetch_source(Path("/unused/source"), "wlroots", pin)
        self.assertEqual(run.call_args_list[1].args[0][-1], build_sway.ORIGINS["wlroots"])
        self.assertEqual(run.call_args_list[-1].args[0], ["git", "checkout", "--detach", pin[2]])

    def test_cold_build_refuses_existing_prefix(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(build_sway, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "refusing to mix installs"):
                build_sway.build({"prefix": temporary})
            run.assert_not_called()

    def test_cached_manifest_must_match_before_executing_anything(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(build_sway, "run") as run:
            (Path(temporary) / "build-inputs.json").write_text("{}\n")
            with self.assertRaisesRegex(RuntimeError, "differs"):
                build_sway.validate({"prefix": temporary})
            run.assert_not_called()

    def test_git_build_versions_require_the_release_and_pinned_revision(self):
        for sway_tag, sources in build_sway.SOURCES.items():
            inputs = {"sources": sources}
            commit = sources["sway"][2]
            for name in ("sway", "swaymsg"):
                for revision in (commit[:7], commit[:12], commit):
                    for date in ("Nov 17 2026", "Jan  3 2027"):
                        version = f"{name} version {sway_tag}-{revision} ({date}, branch 'HEAD')"
                        with self.subTest(version=version):
                            build_sway.verify_sway_version(name, version, inputs)

                invalid_versions = (
                    f"{name} version {sway_tag}",
                    f"{name} version {sway_tag}.1-{commit[:7]} (Nov 17 2026, branch 'HEAD')",
                    f"{name} version {sway_tag}-0000000 (Nov 17 2026, branch 'HEAD')",
                    f"{name} version {sway_tag}-{commit[:6]} (Nov 17 2026, branch 'HEAD')",
                    f"{name} version {sway_tag}-{commit[:7]} (Nov 17 2026, branch 'main')",
                    f"{name} version {sway_tag}-{commit[:7]} (Nov 17 2026, branch 'HEAD') extra",
                    f"other version {sway_tag}-{commit[:7]} (Nov 17 2026, branch 'HEAD')",
                )
                for version in invalid_versions:
                    with self.subTest(version=version):
                        with self.assertRaisesRegex(RuntimeError, "expected .* commit"):
                            build_sway.verify_sway_version(name, version, inputs)

    def test_runtime_validation_rejects_wrong_versions_and_library_resolution(self):
        # Exercise the same validator used on a cold build and an exact cache hit.
        cases = ("valid", "wrong-version", "wrong-wlroots-version", "system-pkgconfig",
                 "system-wlroots", "missing-library", "missing-runpath", "system-executable")
        with tempfile.TemporaryDirectory() as temporary:
            prefix = Path(temporary)
            inputs = {"prefix": temporary, "sway_tag": "1.10",
                      "sources": build_sway.SOURCES["1.10"], "architecture": "amd64"}
            (prefix / "build-inputs.json").write_text(build_sway.serialized(inputs))

            for case in cases:
                def fake_run(arguments, *, env):
                    if arguments[0] == "pkg-config":
                        if "--modversion" in arguments:
                            return "0.18.1" if case == "wrong-wlroots-version" else "0.18.2"
                        if case == "system-pkgconfig":
                            return "/usr/lib/pkgconfig"
                        return str(prefix / "lib/pkgconfig")

                    self.assertEqual(env, {"PATH": f"{prefix}/bin:/usr/bin:/bin"})
                    if arguments[-1] == "--version":
                        version = "1.10.1" if case == "wrong-version" else "1.10"
                        return (f"{arguments[0]} version {version}-c93ff73 "
                                "(Nov 17 2026, branch 'HEAD')")
                    if arguments[0] == "readelf":
                        if case == "missing-runpath":
                            return ""
                        return f"(RUNPATH) Library runpath: [{prefix}/lib]"
                    if case == "missing-library":
                        return "libwlroots-0.18.so => not found"
                    library_directory = "/usr/lib" if case == "system-wlroots" else f"{prefix}/lib"
                    return f"libwlroots-0.18.so => {library_directory}/libwlroots-0.18.so (0x1234)"

                def fake_which(name, *, path):
                    if case == "system-executable":
                        return f"/usr/bin/{name}"
                    return str(prefix / "bin" / name)

                with (
                    self.subTest(case=case),
                    contextlib.redirect_stdout(io.StringIO()),
                    patch.object(build_sway, "run", side_effect=fake_run),
                    patch.object(build_sway.shutil, "which", side_effect=fake_which),
                ):
                    if case == "valid":
                        build_sway.validate(inputs)
                    else:
                        with self.assertRaises(RuntimeError):
                            build_sway.validate(inputs)


if __name__ == "__main__":
    unittest.main()
