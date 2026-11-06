# GitHub Actions performance: release Docker and Rust jobs

## Baseline (November 6, 2026)

The full checks must remain required: `test`, `embedded-native`,
`docker-build`, and `docker-arm64-low-memory`. Logs from the successful pre-optimization run
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

## Native BoringSSL FIPS compilation and low-memory Docker hosts

A failed `btls-sys` build may report
`c++: fatal error: Killed signal terminated program cc1plus` while compiling
`crypto/fipsmodule/bcm.cc`, followed by a CMake error and Cargo exit 101.
This strongly indicates the OS/container killed the C++ compiler after memory
pressure; it is not a BoringSSL patch failure. Lines such as `trailing
whitespace`, `Applied patch ... cleanly`, or `libunwind not found` are
nonfatal in this context.

Dockerfile release builds set `CARGO_BUILD_JOBS=1` and
`CMAKE_BUILD_PARALLEL_LEVEL=1`, limiting simultaneous Rust/native compiler
workers. The build still compiles the exact locked features, keeps FIPS
implementation and crypto tests intact, and runs the same Docker smoke tests.
This may make **cold** builds slower; warm BuildKit cache reuse still applies.
On a machine with sufficient RAM, users can explicitly raise the Cargo limit
with `docker build --build-arg CARGO_BUILD_JOBS=2 ...`; the CMake concurrency
limit remains one to prevent simultaneous BoringSSL C++ workers.

If a single C++ compilation is still OOM-killed on Docker Desktop, raise its
memory allocation (ideally 6–8 GiB or more for this dependency graph), check
available system memory and whether other parallel builds are running.
Do not disable BoringSSL/FIPS features, weaken cryptographic checks, or
skip CI to work around memory exhaustion.

## Generic Rust cache reduction (issue #671)

The test job in run 42243205149 restored an archive of 2,953,182,050 bytes
(~2.75 GiB), with ~20 seconds downloading and ~30 seconds unpacking.
The workflow previously cached the entire `target` directory using
`actions/cache@v4`, including stale/incremental build artifacts.

The `test` job now uses `Swatinem/rust-cache@v2` **after** selecting the Rust
toolchain. This action caches Cargo dependencies and trims workspace,
incremental, and stale build artifacts before upload. It retains library
and dependency compilations needed by Clippy, tests, and benchmark smoke.
The separate `embedded-native` job is unchanged until its cache behavior
is separately measured. No test, audit, or vendor-integrity validation was
removed. Cold builds may be slower while populating the new cache.

Validation requires a full green workflow on the changed configuration,
followed by a warm same-commit re-run; record the restored archive size,
restore time, and total test job duration versus the 2.75 GiB baseline.


## ARM64 release-link regression (November 6, 2026)

The reported failure was at the final application link, after compilation:
`collect2: fatal error: ld terminated with signal 9 [Killed]`.
This is distinct from the earlier BoringSSL compiler OOM. Serial Cargo/CMake
jobs reduce concurrent compilation but do not cap a single linker's memory.
The vendored cosmic-text lifetime messages are warnings and are not its cause.

Why previous checks missed it:

- Docker CI ran on an x86-64 hosted runner with much more RAM than the owner's
  ~2 GiB ARM64 Colima VM, without a build memory limit.
- Native browser acceptance built the debug profile, not the release Docker
  artifact. The local native compilation used temporary swap. Runtime E2E
  cannot detect a release build that it never performs.
- The helper tests mocked Docker, so they could not detect the owner's missing
  Buildx plugin or measure linker memory.

All Rust targets now use Clang/LLD with one linker thread, including dependency
build scripts. A cold local test also reproduced GNU ld OOM in the Obscura/V8
build-script link, so fixing only the final executable would have left that
path exposed. LLD is installed before compiling dependencies; the linker
change intentionally invalidates the old dependency cache once. This does not disable render,
stealth, V8, BoringSSL/FIPS, audits, or any test.

The native `ubuntu-24.04-arm` job bounds the entire BuildKit builder to **2 GiB
RAM and no swap**, verifies those limits, invokes the actual
`scripts/docker-run.sh build` helper, checks the loaded image's ARM64
architecture, and runs its entrypoint and isolated Docker HTTP/shutdown smoke.
It uses a distinct cache scope populated only by that bounded builder.
It also loads the cached build stage and forces the shared production link
command to run again with **1 GiB and no swap**, offline. The original GNU-linker control still failed with an OOM kill at 1 GiB.
The initial 768 MiB relink passed locally but failed on the hosted ARM runner;
1 GiB leaves cross-host headroom while retaining a budget that rejects the
original linker. The production LLD command must pass this hosted gate. A roomy 2 GiB container alone might have allowed the old
linker to pass while a whole 2 GiB VM lacked enough free memory; the separate
final-link gate addresses that distinction.
The existing x86 Docker, native browser/SSH and Rust checks remain intact.

The local helper explicitly uses Buildx with `--load`; absent Buildx fails
before building with installation instructions. Builder failures retain their
exit status. New helper tests cover missing plugins, image loading, argument
forwarding and failure propagation. See Docker's official
[bounded builder options](https://docs.docker.com/build/builders/drivers/docker-container/).

A 2 GiB build-container budget is not a promise that a whole 2 GiB VM can run
that build alongside other services. If the host has insufficient available
memory, give the VM more RAM or stop other workloads before a cold build.
Do not use temporary swap in a no-swap regression test or report debug-runtime
acceptance as proof that the local release Docker build succeeded.
