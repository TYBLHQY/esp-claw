# Sanitized ESP-Claw archive

Date: 2026-10-29. Development is paused. This branch is a standalone source snapshot of the local work, including unpublished C fixes and the independent Rust firmware. It intentionally has no inherited commit history. The upstream Apache-2.0 license and attribution remain in LICENSE and the original readmes.

## Source and documentation

- `application/edge_agent/`: original C/ESP-IDF application and board configuration.
- `application/qq_deepseek_rust/`: independent Rust firmware, locked dependencies, protocol tests, board tooling and detailed implementation/validation notes in AGENTS.md.
- `components/claw_capabilities/cap_im_platform/`: QQ authentication/media/share changes and native host tests.
- `.agents/spec/rust-qq-ai-design.md` and `rust-qq-message-formats.md`: design and message-format specifications.
- `docs/archive/rust-qq-ai-architecture.html`: self-contained architecture diagram; open the downloaded file in a browser.
- `docs/src/content/docs/{en,zh-cn}/reference-project/qq-deepseek-rust.mdx`: Rust application reference documentation.

Use the pinned toolchain versions in the Rust application's `toolchain-versions.txt`. Local absolute setup paths have been replaced with portable example paths.

## Privacy and reproducibility

Private device configuration, raw flash/NVS backups, capture/log directories, firmware build output, caches and generated validation-provenance files are excluded. Recovery identity, behavior and router-rule seeds are reset to public upstream defaults. Configure credentials and device state locally when restoring the project; none is required for the host protocol tests.

The fixed CH340 USB device-class name and RFC1918 addresses in parser/SSRF tests are generic test fixtures, not personal device identifiers. Cargo checksums are unchanged.

Validation results for this snapshot are recorded in `docs/archive/VALIDATION.md`. They do not establish a new on-device firmware acceptance run. Existing documented limitations remain applicable.

## ESP-IDF changes required to reproduce the local setup

The SDK also had local driver changes outside the main repository. Their exact base revision, replay instructions and patch are preserved in [docs/archive/ESP-IDF-PATCH.md](docs/archive/ESP-IDF-PATCH.md). Apply this patch before rebuilding the archived firmware.
