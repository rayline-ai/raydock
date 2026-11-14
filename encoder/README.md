# rayline-arc-encoder

The local ARC encoder: a loopback HTTP server on llama.cpp (`libllama`, static) that runs the public ARC encoder
GGUF (`rayline-ai/ARC-1.0`), so Rayline's local routing reads the encoder's final-layer state on this machine instead
of asking a hosted router. Releases ship it for macOS arm64 only. `rayline` starts it from beside its own executable
(`RAYLINE_ARC_ENCODER` names another binary). The protocol (`/health`, `/encode`, `/close`) is documented at the top
of `arc_encoder_server.cpp`.

- `build.sh`: fetches llama.cpp at the commit pinned in `../release/encoder-source.json`, verifies it and the
  patches by sha256, and builds.
- `patches/0001-…`: an embeddings context skips the LM head and honors the batch's output flags (unpatched, it runs
  the 248k-token vocabulary head on every token and holds ~8 GB of logits at 8,192 tokens).
- `smoke_test.py`: starts a built binary with any small GGUF and checks the protocol.

## Build and smoke test

macOS arm64 with Xcode command line tools, CMake and Python 3:

```bash
encoder/build.sh                       # writes encoder/build/dist/rayline-arc-encoder
python3 encoder/smoke_test.py encoder/build/dist/rayline-arc-encoder [model.gguf]
```

On Linux (development only):

```bash
encoder/build.sh --backend cuda --cuda-arch "89;121"   # needs the CUDA toolkit; links cudart and cuBLAS statically
encoder/build.sh --backend cpu
```

`build.sh` refuses another commit, unpinned patches, local changes and untracked files. The macOS binary links only
system frameworks, embeds its Metal shaders, needs macOS 13+, and never downloads anything. The server binds only
loopback and refuses any other `--host`. `--budget` (tokens per decode call) must equal the release's chunk size
(8192 for ARC 1.0) and not exceed `--ctx`. On Apple M4, `GGML_METAL_TENSOR_ENABLE=1` speeds up flash attention.
`--state-dir` (saved sessions) must be a private directory of the user who runs the encoder, never a shared one.

## Adding a platform

A new platform ships only after all seven steps pass on its target hardware.

1. **Build.** Add the backend to `build.sh` behind the same pin checks, with static libraries and an explicit GPU
   architecture list. Record build time and size (CUDA: ~24 min per architecture on 4 vCPUs, ~0.9 GB).
2. **Smoke test.** `smoke_test.py` passes with a small GGUF.
3. **Parity, on the target hardware.** Maintainers run the release's recorded-decision replay (the recordings aren't
   public). The bar: the same action on ≥ 256 of 265 decisions, flips only on near-ties (top-2 gap ≤ 0.001), every
   clear-margin decision agrees, identical token counts. The M4 sits at 256, so re-run on every llama.cpp or patch
   change.
4. **Speed and memory.** Time the first encode and a +2k append at 1.5k–262k tokens, with RSS and GPU memory. A 21.5k
   first encode must take about 2 s or less (CPU-only bf16 fails: 77 s at 8k).
5. **Process integration.** Shutdown works through `--parent-pipe` (stdin closing); document any environment flag
   the launcher must set.
6. **Release.** A job for that platform tag; the asset goes in the archive and `SHA256SUMS`; installers and
   `rayline update` install it only when listed; the binary is code-signed (notarized or Authenticode).
7. **End to end.** Real `claude -p` and Pi turns in local mode through the sibling binary, then an offline restart.

### Windows (not yet shipped)

Follow upstream llama.cpp's Windows builds ([release.yml](https://github.com/ggml-org/llama.cpp/blob/master/.github/workflows/release.yml), [docs/build.md](https://github.com/ggml-org/llama.cpp/blob/master/docs/build.md)):
- **Build:** MSVC for CUDA, clang-cl for CPU variants, Ninja; `GGML_NATIVE=OFF`, `GGML_OPENMP=OFF`, static CRT (`/MT`).
- **CUDA:** install the toolkit from NVIDIA's redist zips ([windows-setup-cuda](https://github.com/ggml-org/llama.cpp/blob/master/.github/actions/windows-setup-cuda/action.yml)) and set an explicit `CMAKE_CUDA_ARCHITECTURES` list.
- **cuBLAS:** Windows has no static cuBLAS, so ship `cublas` and `cublasLt` DLLs beside the `.exe`.
- **Drivers:** CUDA 12 builds need driver ≥ 525, CUDA 13 builds ≥ 580 ([release notes](https://docs.nvidia.com/cuda/cuda-toolkit-release-notes/)).
- **Unicode paths:** use a UTF-8 argv (`wmain`/`GetCommandLineW`) or a UTF-8 `activeCodePage` manifest ([#18571](https://github.com/ggml-org/llama.cpp/issues/18571)). Expect long paths too.
- **Process:** the parent puts the encoder in a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; closing stdin (`--parent-pipe`) gives the graceful exit, since there is no SIGTERM.
- **State dir:** `--state-dir` must be under `%USERPROFILE%`; its ACL keeps saved sessions private, and no mode check applies.
- **Model cache:** Hugging Face cache symlinks need Developer Mode, or a fallback that copies files.
- **Signing:** Authenticode-sign the `.exe` and the DLLs, with a timestamp. EV certificates no longer bypass SmartScreen ([Microsoft Learn](https://learn.microsoft.com/windows/apps/package-and-deploy/smartscreen-reputation)).
- **Later:** Vulkan and Windows arm64 are phase 2.

### Validated platforms

| Platform | Backend | Hardware | Parity (same action) | Shipped |
|---|---|---|---|---|
| macOS arm64 | Metal | M4 Max | 256/265 | yes |
| Linux aarch64 | CUDA | GB10 | 258/265 | no |
| Linux x86_64 | CUDA | L4 (sm_89) | 258/265 | no |
| Linux | CPU | any | not run | no: 77 s for an 8k first encode |

## Performance reference

Indicative, from 2026-11-14 runs: the public ARC 1.0 GGUF (bf16), `--budget 8192 --ubatch 2048`, random tokens,
timed by a loopback client. **First** encodes N tokens into a new session; **+2k** appends 2,000 tokens to it. Means
of 2–4 calls (1 at 262k), `--ctx 24576` up to 21.5k and `--ctx 270000` above. The M4 Max had
`GGML_METAL_TENSOR_ENABLE=1` and was shared (load average 6–13).

| Context | M4 Max first / +2k | GB10 first / +2k | L4 first / +2k |
|---|---|---|---|
| 1.5k | 0.10 s / 0.14 s | 0.05 s / 0.07 s | 0.05 s / 0.08 s |
| 8k | 0.56 s / 0.17 s | 0.28 s / 0.08 s | 0.28 s / 0.09 s |
| 21.5k | 1.8 s / 0.23 s (2.5 s at `--ctx 270000`) | 0.73 s / 0.08 s | 0.84 s / 0.11 s |
| 64k | 8.7–12.5 s / 0.47–0.63 s | 2.9 s / 0.13 s | 4.0 s / 0.23 s |
| 262k | 134 s / 2.5 s | 22.0 s / 0.31 s | 50.1 s / 0.83 s |

Memory is fixed mostly by `--ctx`: at 24,576 the M4 Max uses 1.72 GB RSS (0.86 GB footprint) and GB10/L4 about
1.3 GB RSS plus 1.6 GB GPU; at 270,000, 5.2 GB RSS on the M4 Max and 4.0 GB GPU on GB10/L4. On M4 the tensor kernels
cut a 64k first encode from ~20 s to ~12 s. Restoring a saved 21.5k session plus a +2k append takes 0.24 s, against
1.8 s to re-encode it.

## License

Rayline's code here (`arc_encoder_server.cpp`, `build.sh`, `smoke_test.py`, and the patch) is under the repository's
Apache License 2.0. The binary statically links llama.cpp and ggml (MIT, the ggml authors), cpp-httplib (MIT,
yhirose) and nlohmann/json (MIT, Niels Lohmann), all from the pinned llama.cpp checkout. `build.sh` copies their
license texts into `rayline-arc-encoder-LICENSE`, which ships beside the binary.
