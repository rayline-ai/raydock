# rayline-arc-encoder on Windows

**Status: parked; not shipped.** Validated once on an NVIDIA L4 under Windows Server 2022 (build 20348, NVIDIA
driver 596.86 in WDDM mode, CUDA 12.9). Windows work resumes after the macOS and Linux releases. Nothing here is
built by CI or released.

## Results

- **Parity:** the CUDA build matched the Linux build on the same GPU: 258/265 cases take the same action (the bar is
  256). It flipped the same 7 near-ties, and every per-case score delta equals the Linux run's. Token counts matched
  on all cases.
- **`encoder/smoke_test.py`:** 38/38 on the CUDA build and the MSVC CPU build. POSIX-only checks are skipped.
- **`wintest.py`** (this directory): `--parent-pipe` shutdown exits 0 and saves sessions. `--state-dir` save and
  restore work. A re-save works while another process holds a saved file open. A second encoder on the same state
  dir warns and runs without saved sessions (`LockFileEx`). Non-ASCII and >260-character state dirs work with the
  manifest (below) and fail without it.

Speed (`bench.py` method, same GPU; seconds):

| Context | Windows first | Windows +2k | Linux first | Linux +2k |
|---|---|---|---|---|
| 1.5k | 0.07 | 0.10 | 0.05 | 0.08 |
| 8k | 0.34 | 0.12 | 0.28 | 0.09 |
| 16k | 0.70 | 0.14 | 0.59 | 0.10 |
| 21.5k | 1.01 | 0.15 | 0.84 | 0.11 |
| 64k (ctx 270k) | 4.57 | 0.28 | 4.0 | 0.23 |
| 262k (ctx 270k) | 55.1 | 0.89 | 50.1 | 0.83 |

Windows is 10–20% slower, consistent with WDDM's per-launch overhead (TCC mode was not tried).

| `--ctx` | Peak working set | Peak commit | GPU memory |
|---|---|---|---|
| 24,576 | 1.66 GB | 4.12 GB | 1.62 GB |
| 270,000 | 2.64 GB | 7.42 GB | 4.02 GB |

GPU memory matches Linux. Commit charge is well above the working set (pinned host buffers), so leave ~7.5 GB of
commit headroom at 270k.

Sizes and dependencies (`dumpbin /dependents`):

- `rayline-arc-encoder.exe` (CUDA, one arch): 57.6 MB. CPU-only: 5.2 MB (MSVC), 7.4 MB (clang).
- Imports: `KERNEL32`, `ADVAPI32`, `WS2_32`, plus `nvcuda.dll` (the driver) and `cublas64_12.dll` for CUDA.
- **No VC++ redist** (`/MT`), no OpenMP DLL, no `cudart64_12.dll` (cudart is static).
- Windows has no static cuBLAS, so `cublas64_12.dll` (102.5 MB) and `cublasLt64_12.dll` (668.7 MB) ship beside the
  exe. The CUDA bundle is ≈ 829 MB uncompressed (Linux static: 857 MB).
- CPU-only is not viable for routing (≈ 7–12 s for 1.5k tokens on 8 vCPUs). Clang is ~1.6× faster than MSVC there.

## Build

Toolchain: VS 2022 Build Tools (VCTools workload; MSVC 19.44 tested), CMake ≥ 3.21, Ninja, Git, and for CUDA the
toolkit from NVIDIA's **redist zips** (`developer.download.nvidia.com/compute/cuda/redist/`), each checked against
the sha256 in `redistrib_<version>.json`. Components: cudart, nvcc, libcublas, cccl, nvtx, profiler_api. Unpack them
into one directory and point `CUDA_PATH` at it. The CUDA network installer failed unattended (exit 0xE0E00019,
nothing installed); the zips need no installer and no VS integration with Ninja.

```bat
set CUDA_PATH=C:\path\to\cuda-12.9
encoder\windows\cuda-build.cmd -CudaArch "75-virtual;80-virtual;86-real;89-real;120a-real"
encoder\windows\clang-build.cmd     :: optional CPU-only build
```

`build.ps1` mirrors `encoder/build.sh`: it verifies `release/encoder-source.json` and the patch hashes, fetches the
pinned llama.cpp, applies the patches, configures `CMakeLists.txt` here and copies the exe (plus the cuBLAS DLLs) to
`encoder/build/dist-win-<backend>`. The flags:

```
-G Ninja -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF
-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded          # /MT everywhere: no VC++ redist
-DGGML_NATIVE=OFF -DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON   # AVX2 baseline
-DGGML_OPENMP=OFF -DGGML_STATIC=ON                  # ggml threadpool; cudart_static + dynamic cuBLAS
-DGGML_CUDA=ON -DCMAKE_CUDA_ARCHITECTURES=<explicit list>
-DARC_MANIFEST=ON                                   # embeds rayline-arc-encoder.manifest
```

- Without the explicit `GGML_AVX*` flags, `GGML_NATIVE=OFF` gives an SSE-only CPU backend.
- Set the CUDA arch list explicitly. One arch took 22 min on 6 shared cores, so allow 1.5–2.5 h for the five-arch
  list, or use a larger runner. Compiling needs no GPU.
- Keep `encoder/patches/*.patch -text` in `.gitattributes`: Git for Windows enables `core.autocrlf`, and the
  patches are pinned by sha256.

## Source changes

Already on this branch: `_putenv_s` instead of `setenv` (MSVC has no `setenv`), `NOMINMAX` and
`WIN32_LEAN_AND_MEAN` before `windows.h`, and `.string()` when printing a `std::filesystem::path` (on Windows
`path::c_str()` is `wchar_t*`, so `%s` printed garbage; MSVC warning C4477). The `LockFileEx` state-dir lock compiles
with MSVC 19.44 and clang 23.

Rebuilding `argv` from `GetCommandLineW` was tried and rejected: argv becomes UTF-8, but `fs::path(std::string)` and
`std::ofstream` still decode narrow strings in the ANSI code page, so sessions saved to the wrong place.

## UTF-8 + longPathAware manifest

`rayline-arc-encoder.manifest` sets `activeCodePage=UTF-8` and `longPathAware=true`. CMake embeds it when it is
listed as a source. It fixes argv, `fs::path`, `fopen`/`ofstream` and llama.cpp's narrow paths at once. Without it,
a profile path such as `C:\Users\Nguyễn Thị\…` fails at start-up. It needs Windows 10 1903+ / Server 2022.
`longPathAware` only lifts MAX_PATH when `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled=1`
(off by default): the installer can set it (admin), or keep the state dir short under `%LOCALAPPDATA%`.

## Remaining work

1. **Job Object in the daemon.** Put the encoder (and core) in a Job Object with
   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so a crashed daemon cannot leave an encoder holding the GPU, the port and
   the state-dir lock. Orphaned children were observed in testing. On a planned stop, close the `--parent-pipe` and
   wait before `kill_on_drop` terminates it (this affects every platform: a kill saves nothing).
2. **Weights hash without Developer Mode.** Without symlink privilege the HF cache copies the snapshot (≈ 1 GB extra
   disk; the `rayline-hf` fallback passed under a basic-user token), and the encoder weights are re-hashed on every
   start (≈ 12 s for 1 GB). Cache a verified hash keyed by path, size and mtime.
3. **Release job on `windows-2022`:** MSVC + Ninja, CUDA from the sha-pinned redist zips, the flags above. Asset
   `rayline-arc-encoder-windows-x86_64-cuda12.zip` with the exe, both cuBLAS DLLs, the license file and NVIDIA's
   redistribution notice (check the CUDA EULA attachment A), listed in `SHA256SUMS`. Requires an NVIDIA driver for
   CUDA 12.9 (≥ 576); detect a missing `nvcuda.dll` and fall back with a clear error.
4. **Authenticode signing** of `rayline.exe`, the encoder and the DLLs, timestamped.
5. **Persistence follow-up** (Windows notes on `trusted_dir` / `lock_dir`):
   - the `%USERPROFILE%` prefix test has no separator boundary (`C:\Users\bob` accepts `C:\Users\bobby\x`):
     compare path components;
   - a refused `--state-dir` is created before it is checked: check first, then create (POSIX has the same order);
   - `lock_dir` leaks the `CreateFileW` handle when `LockFileEx` fails: `CloseHandle` before returning;
   - `getenv("USERPROFILE")` is narrow: rely on the manifest, or use `_wgetenv` and wide paths.
6. **End-to-end with a public release:** `rayline local start` with local decisions, a non-admin user without
   Developer Mode, a non-ASCII profile, stop/restart with saved sessions, killing the daemon, offline restart from
   the cache, two `rayline` processes on one home.

The daemon side already works on Windows by static reading: the sibling lookup appends `.exe`, the `\\?\` path from
`canonicalize` is accepted by `CreateProcessW`, and the loader finds the cuBLAS DLLs in the exe's directory.

## Upstream practice

- llama.cpp builds Windows CUDA with MSVC + Ninja on `windows-2022`, and CPU with clang via
  `cmake/x64-windows-llvm.cmake`, both `GGML_NATIVE=OFF`
  ([release.yml](https://github.com/ggml-org/llama.cpp/blob/master/.github/workflows/release.yml),
  [docs/build.md](https://github.com/ggml-org/llama.cpp/blob/master/docs/build.md)).
- It installs CUDA from NVIDIA's redist zips
  ([windows-setup-cuda](https://github.com/ggml-org/llama.cpp/blob/master/.github/actions/windows-setup-cuda/action.yml))
  and ships `cudart`/`cublas`/`cublasLt` DLLs in a separate zip; Windows has no static cuBLAS since CUDA 12.3.1
  ([ggml-cuda CMakeLists](https://github.com/ggml-org/llama.cpp/blob/master/ggml/src/ggml-cuda/CMakeLists.txt)).
- Upstream's portable CPU build uses `GGML_BACKEND_DL` + `GGML_CPU_ALL_VARIANTS`, which needs shared libs; a static
  sidecar picks one ISA baseline instead.
- Ollama builds with `/MT` (`CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded`,
  [CMakePresets.json](https://github.com/ollama/ollama/blob/main/CMakePresets.json)) and signs with `signtool`
  ([build_windows.ps1](https://github.com/ollama/ollama/blob/main/scripts/build_windows.ps1)).
- llama.cpp fixed non-ASCII argv only in `common_params_parse`
  ([#18571](https://github.com/ggml-org/llama.cpp/issues/18571)); this server parses its own args, hence the
  [UTF-8 manifest](https://learn.microsoft.com/en-us/windows/apps/design/globalizing/use-utf8-code-page).
- Child cleanup: Job Object with `KILL_ON_JOB_CLOSE`
  ([uv-windows job.rs](https://docs.rs/crate/uv-windows/0.0.45/source/src/job.rs)).
- EV certificates no longer bypass SmartScreen; every certificate builds reputation
  ([Microsoft Learn](https://learn.microsoft.com/windows/apps/package-and-deploy/smartscreen-reputation)).
- Vulkan (AMD/Intel) and Windows arm64 (clang, CPU backend) are phase 2.
