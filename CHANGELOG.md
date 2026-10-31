# Changelog

## [0.1.1](https://github.com/hudson-frontier/hudson/compare/v0.1.0...v0.1.1) (2026-10-31)


### ⚠ BREAKING CHANGES

* **context:** queue explicit bounded history compaction ([#105](https://github.com/hudson-frontier/hudson/issues/105))
* **runtime:** isolate Linux runtime filesystems and networking ([#103](https://github.com/hudson-frontier/hudson/issues/103))
* **runtime:** add native SDK sessions and host-owned tools ([#102](https://github.com/hudson-frontier/hudson/issues/102))
* **spending:** exhaustive event consumers must accept spend_reconciled and configured model operations can require approval. Rust ModelProfile literals require spend. Older readers reject spend-enabled state formats.
* **multimodal:** exhaustive Content/Input consumers must handle Image/StartImages, image-bearing transcripts use format 2, and Rust BridgeConfig literals require images. Existing JSON bridge defaults remain text-only.
* **runtime:** exhaustive preview event consumers must accept model_output_delta; Rust BridgeConfig and BridgeRequest literals must specify stream. Existing JSON configurations retain nonstreaming defaults.
* **harness:** Rust ModelProfile struct literals must initialize runtime; use None for existing provider profiles.
* **cli:** reserve operator commands for standalone client ([#85](https://github.com/hudson-frontier/hudson/issues/85))
* **sandbox:** enforce receipt limits before decoding ([#68](https://github.com/hudson-frontier/hudson/issues/68))
* **harness:** Rust embedders must handle queued input and initialize the new context/profile fields. Strict HTTP clients must accept the reviewed queue lifecycle events. Queue use promotes its PostgreSQL namespace to storage format 2; upgrade all database writers before use.

### Features

* **context:** queue explicit bounded history compaction ([#105](https://github.com/hudson-frontier/hudson/issues/105)) ([589d0f6](https://github.com/hudson-frontier/hudson/commit/589d0f616c43b11a3911586ea0769bc066e64c06))
* **core:** add portable conversation export and fork ([#93](https://github.com/hudson-frontier/hudson/issues/93)) ([811e8b1](https://github.com/hudson-frontier/hudson/commit/811e8b122f4aa66fcd934c6e72cc9ec3d63b88c3)), closes [#91](https://github.com/hudson-frontier/hudson/issues/91)
* **core:** support standalone local agent hosts ([#86](https://github.com/hudson-frontier/hudson/issues/86)) ([1b60368](https://github.com/hudson-frontier/hudson/commit/1b60368fa5354f030f426f60ea30a7847c141003))
* **harness:** add bounded external execution adapters ([#90](https://github.com/hudson-frontier/hudson/issues/90)) ([310a546](https://github.com/hudson-frontier/hudson/commit/310a5463128765a073e8e8c44761a5a0ef0caa75))
* **harness:** add bounded offline refinement experiments ([#60](https://github.com/hudson-frontier/hudson/issues/60)) ([24d95c8](https://github.com/hudson-frontier/hudson/commit/24d95c86d66ada6e91d6ce780f30b54ec7554666))
* **harness:** add durable input and provider controls ([b790f48](https://github.com/hudson-frontier/hudson/commit/b790f48a553edc32914deb9c83009e0ae2d473fd))
* **harness:** add tool correction and safe provider diagnostics ([#57](https://github.com/hudson-frontier/hudson/issues/57)) ([bdede94](https://github.com/hudson-frontier/hudson/commit/bdede94e9641008feef3e3b4e82cc8de6b40bf86))
* **multimodal:** add bounded inline image inputs ([#94](https://github.com/hudson-frontier/hudson/issues/94)) ([639a26a](https://github.com/hudson-frontier/hudson/commit/639a26a8a0c28eb66ddf4c1be83d0127aa5417ac))
* **runtime:** add native CLI file recovery and guarded execution ([#104](https://github.com/hudson-frontier/hudson/issues/104)) ([05a3334](https://github.com/hudson-frontier/hudson/commit/05a333458ecbbde37e65257b67f3d1159c60a860))
* **runtime:** add native SDK sessions and host-owned tools ([#102](https://github.com/hudson-frontier/hudson/issues/102)) ([1f7fb03](https://github.com/hudson-frontier/hudson/commit/1f7fb0333cb1f2b0fe49322df2debaa9f7a1b13c))
* **runtime:** isolate Linux runtime filesystems and networking ([#103](https://github.com/hudson-frontier/hudson/issues/103)) ([52fb87a](https://github.com/hudson-frontier/hudson/commit/52fb87a95bace95e11841049e1ba68678e252bc8))
* **runtime:** stream bounded output and contain Windows processes ([#92](https://github.com/hudson-frontier/hudson/issues/92)) ([76097c8](https://github.com/hudson-frontier/hudson/commit/76097c8a5b64a320e341f544b14fb091c45e5fcd))
* **sdk:** add embedded session facade and architecture contract ([#113](https://github.com/hudson-frontier/hudson/issues/113)) ([1c0e94c](https://github.com/hudson-frontier/hudson/commit/1c0e94c1b9578131eb63953f03050829dfa10d27))
* **spending:** persist model allocations and approval gates ([#95](https://github.com/hudson-frontier/hudson/issues/95)) ([30da2e7](https://github.com/hudson-frontier/hudson/commit/30da2e776fe0a9bebeb5012010278e90db6a97d9))


### Bug Fixes

* **ci:** prepare draft patch releases ([#62](https://github.com/hudson-frontier/hudson/issues/62)) ([9b92ca0](https://github.com/hudson-frontier/hudson/commit/9b92ca091545254a9b2c84cbb8e0f56a273fdec1))
* **cli:** bound ordinary response buffering ([#69](https://github.com/hudson-frontier/hudson/issues/69)) ([3f9b429](https://github.com/hudson-frontier/hudson/commit/3f9b4292136b8fad5ad576a4963ece14ab8429e1))
* **cli:** connect authenticated loopback requests directly ([#71](https://github.com/hudson-frontier/hudson/issues/71)) ([cb6536b](https://github.com/hudson-frontier/hudson/commit/cb6536bac892c8b2364cd8695e1f2cfb7327c894))
* **cli:** keep terminal fields on trusted lines ([#72](https://github.com/hudson-frontier/hudson/issues/72)) ([1f38465](https://github.com/hudson-frontier/hudson/commit/1f384653e779e5a142b6a28599192ae96a0dfc16))
* **cli:** validate JSON input file handles ([#74](https://github.com/hudson-frontier/hudson/issues/74)) ([1a6280a](https://github.com/hudson-frontier/hudson/commit/1a6280a471584fe0b969733be2d69b169773616d))
* **config:** reject unusable provider tool aliases ([#88](https://github.com/hudson-frontier/hudson/issues/88)) ([accaafa](https://github.com/hudson-frontier/hudson/commit/accaafab97d2e670aa206905814e90c1bd410974))
* **core:** bypass proxies for local service transports ([#73](https://github.com/hudson-frontier/hudson/issues/73)) ([0114a10](https://github.com/hudson-frontier/hudson/commit/0114a10bf297772e64e78017eb78f1ac6dbcec5a))
* **core:** ignore stale dispatch of owned settled operations ([#80](https://github.com/hudson-frontier/hudson/issues/80)) ([879b3cf](https://github.com/hudson-frontier/hudson/commit/879b3cf6246f83f214c924c96c8afbc181430065))
* **core:** preserve literal reference keys in schemas ([#70](https://github.com/hudson-frontier/hudson/issues/70)) ([0a7461d](https://github.com/hudson-frontier/hudson/commit/0a7461d1345a4f922e5358d78da1515d6521b4e6))
* **core:** validate operation identity for stopped runs ([#81](https://github.com/hudson-frontier/hudson/issues/81)) ([80bf6d2](https://github.com/hudson-frontier/hudson/commit/80bf6d2b945c5e73379a01540bbab1fa7753770c))
* **deps:** update constant-time digest primitives ([#78](https://github.com/hudson-frontier/hudson/issues/78)) ([220d240](https://github.com/hudson-frontier/hudson/commit/220d240dc7d6cdab19a3e51c22f33378b60ebb72))
* **deps:** update TLS verification dependencies ([#77](https://github.com/hudson-frontier/hudson/issues/77)) ([af9331a](https://github.com/hudson-frontier/hudson/commit/af9331a57082589b0db8c54e091dbc357ac1d876))
* **python:** reject incomplete HTTP response bodies ([#87](https://github.com/hudson-frontier/hudson/issues/87)) ([cc783a0](https://github.com/hudson-frontier/hudson/commit/cc783a062313bb0ecff6644868299fc9045988d2))
* **sandbox:** enforce receipt limits before decoding ([#68](https://github.com/hudson-frontier/hudson/issues/68)) ([44e81c6](https://github.com/hudson-frontier/hudson/commit/44e81c64a43cadf61cefe42b077e7eb6997c7875))
* **server:** enforce local request authority and origins ([#75](https://github.com/hudson-frontier/hudson/issues/75)) ([847c6c0](https://github.com/hudson-frontier/hudson/commit/847c6c08ef933312e58f11772f95a79ac84fb995))
* **server:** enforce scheduling scope on cancellation ([#79](https://github.com/hudson-frontier/hudson/issues/79)) ([4181960](https://github.com/hudson-frontier/hudson/commit/418196059881f71c93c63e795e65ca9a2c30e90f))
* **temporal:** bind configured activities to scheduling targets ([#76](https://github.com/hudson-frontier/hudson/issues/76)) ([3e80b6b](https://github.com/hudson-frontier/hudson/commit/3e80b6bcaf5b14b035f258071ba7e79ac017ba51))


### Code Refactoring

* **cli:** reserve operator commands for standalone client ([#85](https://github.com/hudson-frontier/hudson/issues/85)) ([dff6025](https://github.com/hudson-frontier/hudson/commit/dff6025fe6d33d8434286096719335213655cd63))

## [0.1.0](https://github.com/hudson-frontier/hudson/releases/tag/v0.1.0) (2026-10-17)


### Features

* **api:** qualify a bounded Python preview client ([f86180e](https://github.com/hudson-frontier/hudson/commit/f86180e02ec093cbd07a66a2c6e85578134eeed3))
* **sandbox:** persist fenced requests and qualify owned SDK receipts ([3bd7d47](https://github.com/hudson-frontier/hudson/commit/3bd7d478035420e11e2f1da047583039cd526f39))


### Bug Fixes

* **providers:** retain assistant context across tool continuations ([db59af5](https://github.com/hudson-frontier/hudson/commit/db59af58e2bbacc05dbf263a3ad8e6fb25a69bbc))
