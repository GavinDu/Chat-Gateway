# Vendored wreq-util patch

## Upstream and scope

- Crate: `wreq-util`
- Upstream: [0x676e67/wreq-util](https://github.com/0x676e67/wreq-util)
- Vendored release: `3.0.0-rc.12`
- Local change: `vendor/wreq-util/Cargo.toml` selects `zstd = 0.14.3` instead of the upstream `0.13.3` dependency. The lockfile consequently drops `zstd 0.13.3` and `zstd-safe 7.3.0`, retaining the compatible `0.14.3` / `8.2.0` line.

The patch was introduced to remove the older vulnerable compression dependency chain while preserving the pinned Obscura/wreq-util source API. It is a dependency-manifest patch, not a gateway HTTP backend or a behavioral rewrite of wreq-util source.

## Verification and removal condition

CI downloads the exact published crate version, compares its files against the vendored copy while excluding the manifest that carries the intentional dependency change and crates.io's generated VCS metadata, and asserts the expected package and zstd versions. A source-file drift therefore fails CI. The exclusions cover crate packaging metadata and repository-only files, not the library source tree.

Remove this patch only after the pinned Obscura dependency can resolve a reviewed upstream wreq-util release whose dependency graph no longer pulls the vulnerable zstd 0.13 / zstd-safe 7 line. At that point, remove the `[patch.crates-io]` override and re-run the full dependency audit and Rust/Docker CI.
