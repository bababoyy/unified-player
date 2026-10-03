# Third-Party Notice: `ytdlp-ejs`

This directory contains a vendored Rust source snapshot of
[`ahaoboy/ytdlp-ejs`](https://github.com/ahaoboy/ytdlp-ejs) at commit
[`f2a266960642c3e2a15a359a7e9f43d4faa800e1`](https://github.com/ahaoboy/ytdlp-ejs/commit/f2a266960642c3e2a15a359a7e9f43d4faa800e1).

The pinned upstream `Cargo.toml` declares:

```toml
license = "MIT"
authors = ["ahaoboy"]
```

The `authors` value is reproduced as upstream metadata only. It is not a
determination of copyright ownership. The pinned commit's Git tree contains no
`LICENSE`, `LICENSE.md`, `COPYING`, or `NOTICE` file, and no standalone upstream
copyright notice or copyright year was available to copy. The upstream
repository's license metadata is likewise unset. This notice therefore records
the available provenance and license declaration without inventing a copyright
year or assigning copyright to a named person.

The local integration differs from that snapshot as documented in
[`UPSTREAM.md`](UPSTREAM.md): the SWC dependency set was moved to the coherent
parser 42 / AST 26 / codegen 29 / common 24 set; the QuickJS path adds
caller-owned interruption, a 64 MiB heap cap, and redacts challenge/result
values from debug logs while retaining its 16 MiB stack cap; and the provider
enum/registry expose the interruption boundary. The `parallel` feature is
enabled by the application-owned worker. Formatting-only changes do not alter
the upstream behavior.

## Standard MIT Terms

The following is the standard MIT license text. The placeholders in its
copyright line are intentionally unresolved because the pinned upstream source
does not provide a copyright notice or year.

```text
Copyright <YEAR> <COPYRIGHT HOLDER>

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

The canonical MIT text is maintained by the [Open Source
Initiative](https://opensource.org/license/mit). This notice does not replace
the root project's own `LICENSE` or decide any copyright question that the
upstream project did not state.
