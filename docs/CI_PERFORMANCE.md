# GitHub Actions performance: release Docker and Rust jobs

## Baseline (November 6, 2026)

The full checks must remain required: `test`, `embedded-native`, and
`docker-build`. Logs from the successful pre-optimization run
[42243205149](https://github.com/GavinDu/Chat-Gateway/actions/runs/42243205149)
showed:

- `docker-build`: the `cargo build --release` portion took about 5m10s;
  the image step as a whole took about 5m50s. Previously measured Docker
  release builds ranged from roughly five to seven minutes.
- `test`: Cargo cache restoration took ~53s; Clippy ~21s, dependency
  audit ~27s, Rust tests ~35s. Do not disable audit, Clippy, formatting,
  benches, or runtime smoke to make CI green.
- `embedded-native`: installing packages, npm dependencies and Chromium
  took ~40s. The native SSH and browser sandbox smoke tests must remain.

## Changes and cache behavior

The release Dockerfile first compiles the *same* Cargo.lock/vendor dependency
graph using a dummy local binary, then overlays the real `src/` and
`benches/` files and builds the actual release executable. This separates
source edits from the expensive dependency layer.

The Docker job uses Buildx with `type=gha` import/export
(`mode=max`) so that dependency layer can survive fresh ephemeral GitHub
runners. The job still loads the exact image into Docker and executes the
existing entrypoint and isolated HTTP/graceful-shutdown smoke tests. This
does **not** publish an image and does not alter the local Mac Docker helper.

Because cache exports can be large or unavailable (GitHub cache quota, API
throttling, cold cache, upstream base image changes), cache export errors do
not mask build correctness. Cache transfer has a bounded timeout. An initial
build after the Dockerfile changes may be no faster. Measure subsequent runs
with source-only changes before claiming an improvement.

### How to measure

In each run's `docker-build` job, inspect:

1. `Build container image with reusable layer cache`: look for an imported
   `type=gha` cache and `CACHED` on the dependency build step.
2. Time between the start and end of the image-build step, and note whether
   the cache exporter completed. Compare cold vs warm builds at the same
   Cargo.lock, vendor revisions and base Rust image.
3. Confirm **all three** required jobs pass; compare overall workflow
   wall-clock time rather than adding parallel job durations.

If cache export frequently times out or storage use becomes excessive,
investigate cache scope/size and consider moving the dependency build cache
into a separately managed base image. Do not skip tests, sandboxing, provider
terms checks, or local-only security checks.
