# Release roadmap

Loading and capture support all four native group layouts, including active selections.
Behavior is defined in the [specification](SPECIFICATION.md); verification procedures are in
[development](DEVELOPMENT.md). Remaining work:

- **Native groups, visual verification.** Check tab/stack header order and singleton
  headers against the installed Sway version, following the developer guide's separate
  visual acceptance procedure.
- **Final CI and repository audit.** Confirm Linux CI covers the required offline checks
  and locked MSRV build; review repository contents, dependencies, licensing, and release
  metadata for public distribution.
- **Release packaging and distribution.** Produce reproducible tagged Linux archives and
  checksums, reusing CI checks. Start with x86_64; document the architecture/libc baseline
  demonstrated by build and run verification. Do not claim static linking or untested
  platforms.
- **Installation verification.** Smoke-test the packaged executable and documented
  installation on the chosen baseline, including understandable failures for missing
  optional tools. Record the Sway version actually verified without implying broader
  compatibility.
- **Final acceptance.** Complete the optional unified-menu acceptance procedure, keybinding
  launch-context and loader interference checks, and additional manual profile variants
  in the developer guide. Real multi-monitor verification remains incomplete; output-migration safeguards are implemented
  and covered offline. Hardware checks are additional acceptance evidence, not a prerequisite
  for continued development.
- **Publish v0.1.0.** After acceptance, tag and publish the validated archives, checksums,
  installation instructions, and release notes.

No additional architecture changes are committed for this release.
