# BoltFFI documentation snapshot manifest

Fetched: 2026-08-26 PDT (2026-08-26T18:06:40Z)

## Discovery

The six requested seed pages were fetched first. Discovery then used the site's `/llms.txt` index, cross-checked `/llms-full.txt`, scanned the rendered navigation for every indexed page, and recursively extracted every `/docs/` Markdown link from each captured page until no new paths were found. All discovery methods converged on the same 19 pages.

The `/docs/` entry point resolved to `/docs/overview` and returned HTTP 200. Conventional sitemap probes at `/sitemap.xml` and `/docs/sitemap.xml` returned HTTP 404. The site indexes at `/llms.txt` and `/llms-full.txt` returned HTTP 200 and were used for discovery; they are not `/docs/` pages and are therefore not included as snapshot rows.

## Documentation version evidence

**UNRESOLVED: not answered by the documentation.**

No captured page, rendered navigation page, `/llms.txt`, or `/llms-full.txt` declares an exact BoltFFI product, crate, or CLI version. The [official Installation page](https://www.boltffi.dev/docs/installation.md), preserved at `docs/boltffi-swift-bindings/docs-snapshot/installation.md`, uses unversioned `cargo install boltffi_cli` at line 15, an explicit `<VERSION>` placeholder at lines 33–38, and unversioned `cargo add boltffi` commands at lines 58–59. No release-notes or changelog page was discovered.

Pages checked: all 19 `/docs/` pages in the table below, every rendered navigation page corresponding to those paths, `/llms.txt`, and `/llms-full.txt`. Consequently, the audit's `0.30.1` source-behavior version is not corroborated by this documentation corpus and remains an exact-pin/compatibility item for the required Phase-0 probe.

## Pages

| Canonical URL | Local file | First heading | Bytes |
| --- | --- | --- | ---: |
| https://www.boltffi.dev/docs/async-internals.md | `docs/boltffi-swift-bindings/docs-snapshot/async-internals.md` | Async Internals | 7010 |
| https://www.boltffi.dev/docs/async.md | `docs/boltffi-swift-bindings/docs-snapshot/async.md` | Async | 10550 |
| https://www.boltffi.dev/docs/callbacks.md | `docs/boltffi-swift-bindings/docs-snapshot/callbacks.md` | Callbacks & Traits | 14823 |
| https://www.boltffi.dev/docs/classes.md | `docs/boltffi-swift-bindings/docs-snapshot/classes.md` | Classes | 28173 |
| https://www.boltffi.dev/docs/configuration.md | `docs/boltffi-swift-bindings/docs-snapshot/configuration.md` | Configuration | 25195 |
| https://www.boltffi.dev/docs/constants.md | `docs/boltffi-swift-bindings/docs-snapshot/constants.md` | Constants | 8016 |
| https://www.boltffi.dev/docs/custom-types.md | `docs/boltffi-swift-bindings/docs-snapshot/custom-types.md` | Custom Types | 6106 |
| https://www.boltffi.dev/docs/errors.md | `docs/boltffi-swift-bindings/docs-snapshot/errors.md` | Errors | 16371 |
| https://www.boltffi.dev/docs/experimental.md | `docs/boltffi-swift-bindings/docs-snapshot/experimental.md` | Experimental Features | 2032 |
| https://www.boltffi.dev/docs/functions.md | `docs/boltffi-swift-bindings/docs-snapshot/functions.md` | Functions | 25269 |
| https://www.boltffi.dev/docs/getting-started.md | `docs/boltffi-swift-bindings/docs-snapshot/getting-started.md` | Getting Started | 4917 |
| https://www.boltffi.dev/docs/installation.md | `docs/boltffi-swift-bindings/docs-snapshot/installation.md` | Installation | 3152 |
| https://www.boltffi.dev/docs/overview.md | `docs/boltffi-swift-bindings/docs-snapshot/overview.md` | BoltFFI | 6271 |
| https://www.boltffi.dev/docs/packaging.md | `docs/boltffi-swift-bindings/docs-snapshot/packaging.md` | Packaging | 27018 |
| https://www.boltffi.dev/docs/quick-start.md | `docs/boltffi-swift-bindings/docs-snapshot/quick-start.md` | Quick Start | 1691 |
| https://www.boltffi.dev/docs/records.md | `docs/boltffi-swift-bindings/docs-snapshot/records.md` | Records | 30159 |
| https://www.boltffi.dev/docs/streaming.md | `docs/boltffi-swift-bindings/docs-snapshot/streaming.md` | Streaming | 12524 |
| https://www.boltffi.dev/docs/tutorial.md | `docs/boltffi-swift-bindings/docs-snapshot/tutorial.md` | Tutorial | 6109 |
| https://www.boltffi.dev/docs/types.md | `docs/boltffi-swift-bindings/docs-snapshot/types.md` | Types | 28666 |

## Unreachable links and errors

None. Every documentation-page link discovered from the seed pages, recursively fetched pages, rendered navigation, and site indexes returned HTTP 200.

The two conventional sitemap discovery probes were unreachable:

| URL | Error |
| --- | --- |
| https://www.boltffi.dev/sitemap.xml | HTTP 404 (79-byte response) |
| https://www.boltffi.dev/docs/sitemap.xml | HTTP 404 (79-byte response) |
