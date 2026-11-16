# Release roadmap

Loading and capture support all four native group layouts, including active selections.
Behavior is defined in the [specification](SPECIFICATION.md); verification procedures are in
[development](DEVELOPMENT.md). Remaining work:

- **Native groups, visual verification.** Check tab/stack header order and singleton
  headers against the installed Sway version, following the developer guide's
  native-group visual check.
- **Final CI and repository audit.** Confirm Linux CI covers the required offline checks
  and locked MSRV build, and that the headless integration job passes on GitHub's hosted
  runner; review repository contents, dependencies, licensing, and release metadata for
  public distribution.
- **Release packaging and distribution.** Produce reproducible tagged Linux archives and
  checksums, reusing CI checks. Start with x86_64; document the architecture/libc baseline
  demonstrated by build and run verification. Do not claim static linking or untested
  platforms.
- **Installation verification.** Smoke-test the packaged executable and documented
  installation on the chosen baseline, including understandable failures for missing
  optional tools. Record the Sway version actually verified without implying broader
  compatibility.
- **Final acceptance.** Complete the developer guide's manual application acceptance with
  applications other than Foot, which is the only application the headless suite launches.
  Also complete the optional unified-menu acceptance procedure, keybinding launch-context
  and loader interference checks (including in-workspace focus restoration), and additional
  manual profile variants. Real multi-monitor verification remains incomplete; the
  output-migration safeguard is covered offline and in the headless two-output suite, which
  also covers standalone and menu loading on a second output.
  Hardware checks are
  additional acceptance evidence, not a prerequisite for continued development.
- **Publish v0.1.0.** After acceptance, tag and publish the validated archives, checksums,
  installation instructions, and release notes.

No additional architecture changes are committed for this release.
