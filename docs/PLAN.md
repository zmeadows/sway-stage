# Release roadmap

Loading and capture support all four native group layouts. Stage 4 capture and offline
serialization round-trip checks are complete. Behavior is defined in the
[specification](SPECIFICATION.md); verification procedures are in
[development](DEVELOPMENT.md). Remaining work:

- **Native groups, manual verification.** Guarded construction, recursive split sizing,
  and postorder first-child tab/stack selection with final global focus are implemented.
  Manually verify inactive selection retention and explicit focus precedence, plus
  root and nested groups, singleton ancestry, sibling insertion, and empty workspaces with
  initial tabbed/stacked layouts against the installed Sway version. Offline tree/event
  fixtures verify reactions, not the compositor's actual construction behavior.
- **Native groups, Stage 5.** Extend the manual integration harness with native-group
  construction and capture/reload scenarios after review. Verify mixed nesting, singleton
  preservation, split proportions, first-child defaults, and global focus against Sway;
  offline serialization checks do not establish compositor reconstruction behavior.

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
