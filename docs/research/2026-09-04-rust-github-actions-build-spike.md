# Rust GitHub Actions Build Spike

Date: 2026-09-04

## Scope

Research the first CI build spike for this repository:

- one GitHub Actions workflow
- triggered on pull requests
- builds self-contained Linux binaries for `x86_64` and `aarch64`
- builds an Apple Silicon macOS binary for `aarch64-apple-darwin`
- uploads all binaries as workflow run artifacts

This note intentionally keeps tag-to-Release publishing out of the spike implementation, but records the recommended extension path for later.

## Repo-specific observations

- The crate requires Rust `1.88` and the binary name is `immich-alt-text` in [`Cargo.toml`](../../Cargo.toml).
- The dependency graph currently resolves `reqwest` through `hyper-rustls` and `rustls`, not `native-tls`/OpenSSL, in [`Cargo.lock`](../../Cargo.lock). That makes Linux cross-builds materially easier because this spike does not appear to need OpenSSL target packages or custom `cross` images.

## Feasibility summary

This spike is feasible with a single `pull_request` workflow.

The lowest-friction implementation is a hybrid matrix:

- build Linux `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` on `ubuntu-latest` with `cargo-zigbuild` + Zig
- build macOS `aarch64-apple-darwin` natively on `macos-latest`
- upload one artifact per target with `actions/upload-artifact`

Why this shape:

- Rust distributes both Linux musl targets through `rustup`, and `aarch64-unknown-linux-musl` can be cross-compiled from any host. Source: [rustup cross-compilation](https://rust-lang.github.io/rustup/cross-compilation.html), [Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html), [aarch64-unknown-linux-musl target page](https://doc.rust-lang.org/rustc/platform-support/aarch64-unknown-linux-musl.html).
- `cargo-zigbuild` is explicitly designed to use Zig as the linker for easier cross compilation. Source: [cargo-zigbuild README](https://github.com/rust-cross/cargo-zigbuild).
- Zig’s platform support docs state that libc is available for supported targets even when cross-compiling, which is the key reason it works well as the Linux linker/sysroot provider here. Source: [Zig platform support](https://ziglang.org/learn/platform-support/).
- GitHub currently offers Apple Silicon macOS hosted runners under `macos-latest` / `macos-14` / `macos-15`. Source: [GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## Target triples to use

Use these exact targets:

| OS | Architecture | Rust target triple | Why |
| --- | --- | --- | --- |
| Linux | x86_64 | `x86_64-unknown-linux-musl` | musl target for self-contained Linux builds; Rust documents this target as static by default among the musl exceptions. Source: [Rust Reference: linkage](https://doc.rust-lang.org/reference/linkage.html?highlight=ffi), [Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html). |
| Linux | aarch64 | `aarch64-unknown-linux-musl` | official Rust musl target for ARM64 Linux; Rust documents it as available through `rustup` and cross-compilable from any host. Source: [aarch64-unknown-linux-musl](https://doc.rust-lang.org/rustc/platform-support/aarch64-unknown-linux-musl.html). |
| macOS | Apple Silicon | `aarch64-apple-darwin` | official Rust target for ARM64 macOS. Source: [Apple Darwin targets](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html), [Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html). |

## Approach comparison

| Approach | Linux self-contained binaries | macOS arm64 binary | Pros | Cons | Verdict for this repo |
| --- | --- | --- | --- | --- | --- |
| Native runners only | Possible, but awkward if you want self-contained Linux artifacts | Yes, on `macos-latest` | simplest mental model; easiest native smoke testing | Linux native `*-gnu` builds are not self-contained; native ARM Linux runners exist but are still listed as public preview; musl on native runners still needs a linker/sysroot strategy | Not recommended for the spike |
| `cross` | Yes | Not a good fit for the macOS target in this spike | official Rust cross tool; same CLI as Cargo; supports cross testing | requires Docker or Podman; adds a container layer; still leaves macOS as a separate native build; extra image work if C deps appear later | Viable, but heavier than needed here |
| `cargo-zigbuild` + Zig for Linux, native macOS build | Yes | Yes | small workflow surface; good Linux portability story; no container engine; fits this repo’s current pure-Rust/rustls dependency graph | if future crates need system headers/libs, `cargo-zigbuild` may need explicit `CFLAGS`/`RUSTFLAGS`; macOS signing/notarization is still a separate concern | Recommended |

Sources:

- `cross` requires Docker or Podman and presents itself as a cross-compilation and cross-testing wrapper around Cargo: [cross README](https://github.com/cross-rs/cross).
- `cargo-zigbuild` installs via Cargo, uses Zig as the linker, and documents GNU-version targeting plus Apple SDK handling knobs like `SDKROOT`: [cargo-zigbuild README](https://github.com/rust-cross/cargo-zigbuild).
- GitHub-hosted runner labels and current arm64 availability: [GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## Recommended spike implementation

### Trigger and trust model

Use `pull_request`, not `pull_request_target`.

Reasons:

- GitHub documents that `pull_request` workflows run against the merge branch by default, and `actions/checkout` will therefore check out the merge result unless you override the ref. Source: [events that trigger workflows](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows?via=aivyx).
- GitHub documents that workflows triggered from fork pull requests under `pull_request` have read-only permissions and no access to secrets. Source: [workflow syntax permissions](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax), [compromised runners](https://docs.github.com/en/actions/concepts/security/compromised-runners).
- GitHub explicitly warns against using `pull_request_target` to build or run untrusted pull request code. Source: [events that trigger workflows](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows?via=aivyx), [secure use reference](https://docs.github.com/en/actions/reference/security/secure-use?learn=getting_started&learnProduct=actions).

For this spike, the workflow should do only build/test/upload work and should use no repository secrets.

### Permissions

Set top-level permissions to:

```yaml
permissions:
  contents: read
```

Why:

- `actions/checkout` explicitly recommends `contents: read`. Source: [actions/checkout README](https://github.com/actions/checkout/blob/main/README.md?plain=1).
- GitHub documents that once any permission is specified, all unspecified permissions become `none`. Source: [workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax).

I did not find an official GitHub document that requires additional repository token scopes for `actions/upload-artifact`; for this spike I would assume job-local artifact upload works with `contents: read` only.

### Checkout settings

Use `actions/checkout` with:

```yaml
with:
  persist-credentials: false
```

Recommended reasoning:

- `actions/checkout` persists credentials by default; this workflow does not need to push or fetch with authenticated Git commands. Source: [actions/checkout README](https://github.com/actions/checkout/blob/main/README.md?plain=1).
- The default fetch depth of 1 is acceptable for this build-only spike. Source: [actions/checkout README](https://github.com/actions/checkout/blob/main/README.md?plain=1).

### Matrix shape

Use one matrix job with `include`, because the build commands and runners differ by target:

| Runner | Target | Build command |
| --- | --- | --- |
| `ubuntu-latest` | `x86_64-unknown-linux-musl` | `cargo zigbuild --release --target x86_64-unknown-linux-musl` |
| `ubuntu-latest` | `aarch64-unknown-linux-musl` | `cargo zigbuild --release --target aarch64-unknown-linux-musl` |
| `macos-latest` | `aarch64-apple-darwin` | `cargo build --release --target aarch64-apple-darwin` |

Why this exact split:

- `aarch64-apple-darwin` is a first-class Rust macOS target and requires no special configuration when built natively. Source: [Apple Darwin targets](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html).
- `cargo-zigbuild` is a good fit for Linux cross-linking; using it for macOS would introduce SDK concerns the spike does not need. Source: [cargo-zigbuild README](https://github.com/rust-cross/cargo-zigbuild).
- GitHub’s hosted runner inventory already includes Apple Silicon macOS, so the macOS build does not need cross-compilation. Source: [GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

### Toolchain setup

Install the Rust targets with `rustup target add <target>`, because Rust documents that `rustup` installs only the host stdlib by default and extra targets must be added explicitly. Source: [rustup cross-compilation](https://rust-lang.github.io/rustup/cross-compilation.html).

For the Linux matrix entries:

- install Zig using the official Zig distribution path or another official Zig-documented method. Source: [Zig overview](https://ziglang.org/learn/overview/).
- install `cargo-zigbuild` using its documented Cargo install command:

```bash
cargo install --locked cargo-zigbuild
```

Source: [cargo-zigbuild README](https://github.com/rust-cross/cargo-zigbuild).

### Artifact upload

Upload one artifact per target, named with the binary and target triple, for example:

- `immich-alt-text-x86_64-unknown-linux-musl`
- `immich-alt-text-aarch64-unknown-linux-musl`
- `immich-alt-text-aarch64-apple-darwin`

Recommended upload path:

```text
target/<target>/release/immich-alt-text
```

Artifact notes:

- GitHub documents `retention-days` for `upload-artifact`; the value cannot exceed the repo/org/enterprise limit. Source: [store and share data with workflow artifacts](https://docs.github.com/en/actions/tutorials/store-and-share-data), [artifact retention settings](https://docs.github.com/en/organizations/managing-organization-settings/configuring-the-retention-period-for-github-actions-artifacts-and-logs-in-your-organization).
- GitHub documents a default artifact retention of 90 days. For PR build artifacts, I recommend setting `retention-days: 14` to keep storage bounded. Source: [artifact retention settings](https://docs.github.com/en/organizations/managing-organization-settings/configuring-the-retention-period-for-github-actions-artifacts-and-logs-in-your-organization).
- The official `upload-artifact` README documents `compression-level`; binaries are usually not very compressible, so `compression-level: 0` is a reasonable speed optimization but optional. Source: [actions/upload-artifact README](https://github.com/actions/upload-artifact/blob/main/README.md).

### Caching

Keep caching conservative in the first spike.

Recommended options:

1. Simplest: no cache at all.
2. If needed, cache only Cargo registries / Git dependencies and keep keys per OS + target + lockfile hash.

Why not start with aggressive caching:

- GitHub’s cache docs treat caches as untrusted input and explicitly warn not to store sensitive information there. Source: [dependency caching](https://docs.github.com/en/actions/concepts/workflows-and-actions/dependency-caching), [dependency caching reference](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching).
- GitHub documents special low-trust behavior for fork PR cache access and notes that cache writes from low-trust runs should not be assumed to refresh the default branch’s cache scope. Source: [dependency caching reference](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching).
- This workflow is `pull_request`-only, so cache ROI is lower until a trusted `push` workflow exists to keep shared caches warm.

For this reason, I would not cache `target/` in the spike.

## Portability and libc caveats

### Linux

For the spike, use musl targets for both Linux artifacts.

Why:

- Rust’s linkage reference documents that `x86_64-unknown-linux-musl` is one of the targets that is static by default. Source: [Rust Reference: linkage](https://doc.rust-lang.org/reference/linkage.html?highlight=ffi).
- Rust’s ARM64 musl target page documents that `aarch64-unknown-linux-musl` is an official target distributed through `rustup` and cross-compilable from any host. Source: [aarch64-unknown-linux-musl](https://doc.rust-lang.org/rustc/platform-support/aarch64-unknown-linux-musl.html).
- Zig documents libc availability for supported cross targets, which is why it is a practical linker/sysroot choice for both Linux entries. Source: [Zig platform support](https://ziglang.org/learn/platform-support/).

Important caveat:

- `cargo-zigbuild` also supports `*-gnu` builds and even lets you pin a minimum glibc version such as `.2.17`, but that still produces glibc-linked binaries rather than the self-contained Linux binaries requested for this spike. Source: [cargo-zigbuild README](https://github.com/rust-cross/cargo-zigbuild).

Another practical caveat:

- `cargo-zigbuild` documents that it uses `zig cc` with `-nostdinc`, so if future dependencies introduce crates that need system headers or libraries, this workflow may need explicit `CFLAGS` or `RUSTFLAGS`. Source: [cargo-zigbuild README](https://github.com/rust-cross/cargo-zigbuild).

### macOS

The macOS binary in this spike should be an ordinary native `aarch64-apple-darwin` release build, unsigned and not notarized.

Why:

- Rust documents `aarch64-apple-darwin` as the ARM64 macOS target and says these targets require no special configuration when distributed through `rustup`. Source: [Apple Darwin targets](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html).
- Apple documents that software distributed outside the App Store should be signed appropriately, and Developer ID-distributed software should be notarized. Source: [Creating distribution-signed code for the Mac](https://developer.apple.com/documentation/xcode/creating-distribution-signed-code-for-the-mac/), [Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution?changes=_5).
- GitHub documents that fork PR workflows do not receive secrets, so PR CI is the wrong place to introduce signing certificates or notarization credentials. Source: [workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax), [compromised runners](https://docs.github.com/en/actions/concepts/security/compromised-runners).

Runner caveat:

- GitHub’s arm64 macOS runners do not have a static UDID, and GitHub notes compatibility caveats for community actions on arm64 macOS. Source: [GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

Recommendation:

- build unsigned in PR CI now
- defer signing/notarization to a later trusted tag/release workflow

## How to test the produced artifacts

For the spike, artifact validation should be lightweight and platform-aware.

Recommended checks inside the build job:

1. Confirm the file exists at `target/<target>/release/immich-alt-text`.
2. Run `file` on every artifact to confirm target architecture / binary type.
3. Smoke-test only on native runners in this spike:
   - Linux x86_64 artifact: run `./target/x86_64-unknown-linux-musl/release/immich-alt-text --help`
   - macOS arm64 artifact: run `./target/aarch64-apple-darwin/release/immich-alt-text --help`
4. For `aarch64-unknown-linux-musl`, validate metadata now and add runtime execution later on either:
   - a native ARM Linux runner such as `ubuntu-24.04-arm` (currently listed by GitHub as public preview), or
   - QEMU-based verification

Why this is the right boundary for the spike:

- Rust’s target page for `aarch64-unknown-linux-musl` explicitly says testing can be done on ARM64 hardware or via QEMU emulation. Source: [aarch64-unknown-linux-musl](https://doc.rust-lang.org/rustc/platform-support/aarch64-unknown-linux-musl.html).
- GitHub documents artifact digest validation when an uploaded artifact is later downloaded in the same workflow. Source: [store and share data with workflow artifacts](https://docs.github.com/en/actions/tutorials/store-and-share-data).

If you want a separate verification job later, it can download the just-built artifacts with `actions/download-artifact`; GitHub documents that each downloaded artifact is placed in its own directory when all artifacts are downloaded together. Source: [store and share data with workflow artifacts](https://docs.github.com/en/actions/tutorials/store-and-share-data).

## Later extension: tags to GitHub Releases

Do not implement this in the spike.

Recommended later shape:

- keep the PR workflow artifact-only
- add a separate trusted workflow for tags, likely on `push.tags`
- rebuild or reuse the same target matrix
- add macOS signing/notarization only there, where secrets can be safely scoped
- attach packaged assets to a GitHub Release

Relevant GitHub docs:

- tag filters on `push`: [workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax)
- release management: [managing releases in a repository](https://docs.github.com/en/repositories/releasing-projects-on-github/managing-releases-in-a-repository?tool=cli)

## Concise recommendation

Implement the spike as one `pull_request` workflow with a three-entry matrix:

- `ubuntu-latest` + `cargo-zigbuild` for `x86_64-unknown-linux-musl`
- `ubuntu-latest` + `cargo-zigbuild` for `aarch64-unknown-linux-musl`
- `macos-latest` + native Cargo for `aarch64-apple-darwin`

Use `permissions: contents: read`, `actions/checkout` with `persist-credentials: false`, no secrets, one uploaded artifact per target, and only lightweight smoke tests in the same job. Defer release publishing, signing, and notarization to a later trusted tag workflow.

## Explicit assumptions

- “Self-contained Linux binaries” means musl-linked binaries that do not depend on the host glibc at runtime.
- Building the macOS artifact in PR CI does not require code signing or notarization yet.
- Using only official GitHub actions plus direct `rustup` / Zig / `cargo-zigbuild` installation is preferable to introducing third-party setup actions in the first spike.
- Because this repository currently resolves `reqwest` through Rustls rather than OpenSSL, the Linux cross-build does not presently require extra target-specific system packages.
- I did not find an official GitHub document stating that `upload-artifact` needs token permissions beyond `contents: read`; this recommendation assumes the normal artifact upload path works without broader repository permissions.
