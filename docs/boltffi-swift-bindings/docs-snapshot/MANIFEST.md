# BoltFFI documentation snapshot manifest

Fetched: 2026-08-25 PDT (2026-08-26T00:14:32Z)

## Discovery

The six requested seed pages were fetched first. Discovery then used the site's `/llms.txt` index, cross-checked `/llms-full.txt`, and recursively extracted every `/docs/` Markdown link from each captured page until no new paths were found. The index and recursive crawl converged on the same 19 pages.

Conventional sitemap probes at `/sitemap.xml` and `/docs/sitemap.xml` returned HTTP 404. The site indexes at `/llms.txt` and `/llms-full.txt` returned HTTP 200 and were used for discovery; they are not `/docs/` pages and are therefore not included as snapshot rows.

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

## Unreachable `/docs/` links

None. Every `/docs/` link discovered from the seed pages, recursively fetched pages, and site indexes returned HTTP 200.
