# Ropendal News

## Ropendal 0.0.1.9000

- Development version after 0.0.1.
- SFTP: added a native password-authenticated SFTP adapter built with pure-Rust
  `russh`/`russh-sftp`, while retaining Unix OpenDAL key/agent SFTP support.
  Both paths use strict host-key verification by default and have disposable
  local Docker integration coverage; wasm/webR builds reject SFTP explicitly.
- Testing: the writable local S3-compatible tests and development benchmarks
  now run against SeaweedFS (`weed mini`) instead of MinIO. Their environment
  variables move from `ROPENDAL_*_S3_MINIO_*` to `ROPENDAL_*_S3_SEAWEEDFS_*`.
- Documentation: expanded README examples for batched I/O, async Aio handles,
  serializers/codecs, Arrow IPC streams via nanoarrow, lower-level iterators,
  S3-compatible SeaweedFS benchmarking, and native C API scheduling.
- Documentation: aligned design notes, roxygen docs, and vignettes with the
  current async, credential, serializer, and native C API behavior.
- Documentation: documented R-universe Linux binary repository setup.
- Build: added a webR/wasm package-load fallback and local rwasm build helper.
- Build: Windows arm64 source builds now derive the Rust target from the C
  compiler, so Rtools' aarch64 clang/gnullvm toolchain builds
  `aarch64-pc-windows-gnullvm` instead of failing the DLL link against a
  hardcoded `x86_64-pc-windows-gnu` target directory.
- Reads: `fs_read()` / `fs_read_bytes()` now support multiple ranges from one
  path and list-of-ranges shapes across multiple paths, while preserving ordered
  flat or nested results. `byte_ranges()` adds a request-object form for
  index-heavy readers, `opendal_bytes_slice()` derives subrange `OpendalBytes`
  handles without first materializing full R raw vectors, and
  `as.raw.OpendalBytes()` now returns a raw ALTREP facade that supports element
  and region access before R-owned raw-vector materialization.
- Byte stores: `byte_store()` and `store_*()` helpers provide a small
  store-relative key-to-bytes adapter for Zarr-like chunk layouts. `store_read()`
  is now bytes-first for `OpendalBytes`/ALTREP-compatible flows, async `store_*_aio()`
  wrappers are available, `store_cache()` adds an explicit local full-object
  cache for chunk-key stores, and `store_block_cache()` adds an explicit local
  fixed-size cache for scalar `result = "auto"` complete/range store reads.
  New vignettes and tests show pure-R chunked-array and indexed range-reader
  patterns on top of the byte substrate.
- API: R-side `batch_concurrency = 0` is now rejected; use `NULL` for the
  default. Capability rows now include range/concurrency/recursive semantics.
- C API: write tuning options are wired through, unsupported write header
  options reject explicitly, `ropendal_aio_wait()` honors its timeout, callbacks
  now receive the completed Aio pointer, and prefix-scoped byte stores now expose
  async read/write/read-into/list/delete plus explicit full-object and fixed-size
  block cache adapters for downstream native code. The C ABI version is now 4.

## Ropendal 0.0.1

- Initial public release.
- Added byte-first filesystem primitives backed by Apache OpenDAL, including
  read, write/create, replace, append, stat, listing, copy, rename, delete, and
  directory creation where supported by the backend.
- Added Aio-based async operations and controls, including `fs_*_aio()`,
  `call_aio()`, `collect_aio()`, unresolved sentinels, condition variables, and
  monitor helpers.
- Added explicit serializer/deserializer and codec layers for materializing R
  objects and transforming raw bytes.
- Added an async-first native C API in `inst/include/ropendal.h` for downstream
  native packages that need direct byte I/O into caller-owned buffers.
- Added local filesystem, HTTP(S), S3-compatible, and Google Drive examples and
  test coverage.
