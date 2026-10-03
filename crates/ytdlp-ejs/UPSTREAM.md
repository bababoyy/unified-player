# Vendored Source

This directory is a vendored snapshot of `ahaoboy/ytdlp-ejs` at commit
`f2a266960642c3e2a15a359a7e9f43d4faa800e1`.

The snapshot is kept local so `unified-player` can use the exact reviewed Rust
engine instead of resolving a same-version crates.io release. The local
integration changes the SWC parser quartet to the maintained 42/26/29/24 set
to remove the `smartstring` dependency, and adds QuickJS interruption and a
64 MiB heap limit while retaining the reviewed 16 MiB stack limits and adding
challenge/result log redaction. The
provider enum and registry expose the caller-owned interruption boundary. The
application-owned native worker enables the crate's `parallel` feature.

The available upstream license metadata and the standard MIT terms are
recorded in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). The pinned
upstream commit does not contain a standalone license or copyright notice, so
that notice deliberately does not invent a copyright year or assign copyright
ownership.

The upstream EJS assets and JavaScript protocol remain pinned separately in
`unified-player/src/client/youtube/ejs/manifest.toml`.
