# Release roadmap

Loading and capture support all four native group layouts. Stage 4 capture/offline
serialization and Stage 5 independent manual-harness coverage are implemented; live
native-group acceptance remains outstanding. Behavior is defined in the
[specification](SPECIFICATION.md); verification procedures are in
[development](DEVELOPMENT.md). Remaining work:

- **Native groups, manual verification.** Guarded construction, recursive split sizing,
  and postorder configured tab/stack selection with final global focus are implemented.
  Manually verify inactive selection retention and explicit focus precedence, plus
  root and nested groups, singleton ancestry, and sibling insertion against the installed
  Sway version. Offline tree/event fixtures verify reactions, not the compositor's actual
  construction behavior.
- **Native groups, harness acceptance.** Run the nineteen-scenario standard suite,
  including eight native-group additions and two capture/reload cases. The independent
  harness checks exact mixed hierarchy, singleton preservation, split proportions,
  per-container child-focus history, final global focus, capture weights, and conservative
  success-only cleanup transitions. Verify these source-backed expectations against the
  installed Sway version. The `initially-tabbed` and `initially-stacked` cases establish and
  verify empty native workspace layouts before loading the existing mixed hierarchy. Offline
  fixtures do not establish compositor reconstruction or cleanup behavior; record manual
  results before marking this acceptance complete.

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
- **Final acceptance.** Complete the outstanding menu, keybinding launch-context, and loader
  interference checks in the developer guide. Real multi-monitor verification remains
  incomplete; output-migration safeguards are implemented and covered offline. Hardware
  checks are additional acceptance evidence, not a prerequisite for continued development.
- **Publish v0.1.0.** After acceptance, tag and publish the validated archives, checksums,
  installation instructions, and release notes.

No additional architecture changes are committed for this release.
