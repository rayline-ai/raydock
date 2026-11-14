# rayline-arc-encoder

The local ARC encoder. Rayline's local routing starts it to read the ARC encoder's final-layer state at chosen token
offsets, on this Mac, instead of asking a hosted router. It is a small HTTP server on llama.cpp (`libllama`, static,
Metal) that runs the public ARC encoder GGUF (`rayline-ai/ARC-1.0`). Releases ship it for macOS arm64 only;
`build.sh` also builds it on Linux (CUDA or CPU) for development.

- `arc_encoder_server.cpp`: the server.
- `patches/`: changes to the pinned llama.cpp (below).
- `build.sh`: fetches the pinned llama.cpp, verifies it and the patches, builds.
- `smoke_test.py`: starts a built binary with any small GGUF and checks the protocol.
- `../release/encoder-source.json`: the llama.cpp repository, exact commit and patch sha256s.

## Build

macOS arm64 with Xcode command line tools, CMake and Python 3 (Ninja optional):

```bash
encoder/build.sh                       # writes encoder/build/dist/rayline-arc-encoder
python3 encoder/smoke_test.py encoder/build/dist/rayline-arc-encoder [model.gguf]
```

`build.sh` refuses a checkout at another commit, local changes other than the pinned patches, and any patch whose
sha256 is not the pinned one, and untracked files in the checkout. The binary links only system frameworks (Metal, Foundation, Accelerate), has the Metal
shaders embedded, and needs macOS 13 or later. It is built without curl or OpenSSL: it never downloads anything.

On Linux (not shipped in releases):

```bash
encoder/build.sh --backend cuda --cuda-arch "89;121"   # explicit CMAKE_CUDA_ARCHITECTURES; needs the CUDA toolkit
encoder/build.sh --backend cpu                         # pass --threads at run time (libllama defaults to 4)
```

The CUDA build links the CUDA runtime and cuBLAS statically, so it needs only the NVIDIA driver at run time (and is
large, ~700 MB).

## Running it

```bash
rayline-arc-encoder --model arc-encoder-bf16.gguf --port 0 --budget 8192 --ctx 131072 \
  --state-dir ~/.rayline/local/arc-sessions --parent-pipe
```

`--port 0` binds a free loopback port and prints `{"listening": PORT}` as the first stdout line. `--parent-pipe` stops
the server (saving its sessions) when its stdin closes. `--budget` is the tokens per decode call and must equal the
encoder release's chunk size (8192 for ARC 1.0); it may not exceed `--ctx`, or the encoder refuses to start. `--threads N` sets the CPU threads. `--help` lists every flag.

`rayline` finds the encoder beside its own executable; `RAYLINE_ARC_ENCODER` names another binary.

On Apple M4 chips, llama.cpp leaves its Metal tensor-API kernels off by default; this model's flash attention
(head_dim 256) is faster with them, so a launcher may set `GGML_METAL_TENSOR_ENABLE=1` there. M5 and later use them
already. `--no-tensor` turns them off everywhere.

## Protocol

All requests and errors are JSON; a successful `/encode` answers raw rows. A `POST` without
`Content-Type: application/json` is refused with 415, so a web page cannot reach the loopback port without a CORS
preflight (which the server never answers). Requests whose `Host` is not the bound loopback address and port, or
that carry an `Origin`, are refused with 403 (DNS rebinding). Bodies are capped near `--ctx` × 32 bytes, and token
ids, offsets and `base` must be in-range integers (422 `invalid_body` otherwise).

`GET /health` answers `{"engine" (llama.cpp tag, backend such as `metal`, `cuda` or `cpu`, and options), "depth", "budget", "rope", "max_model_len", "layer", "dim", "sessions",
"capabilities", "checkpoint"}`.

`POST /encode`:

| Field | Meaning |
|---|---|
| `tokens` | Token ids at positions `base`, `base + 1`, ... |
| `offsets` | Absolute positions to read, strictly ascending, each in `[base, base + len(tokens))` |
| `base` | Tokens the session already holds (default 0) |
| `session` | Retained session id: `base` 0 starts or replaces it; `base` N appends to a session holding exactly N tokens |
| `retain: false` | Instead of `session`: a one-shot encode that keeps nothing (`base` must be 0) |
| `owner` | Optional; `/close` drops an owner's sessions |

It answers `200` with `len(offsets)` rows of little-endian float32 (`x-rows` rows of `x-dim` floats, plus `x-timing-ms`
and `x-restored-tokens`), `409 {"error": "unknown_session"}` for an append to a session it does not hold at `base`,
`422 {"error": code}` for `invalid_body`, `offsets_out_of_range`, `invalid_retain`, `images_not_served` (text only) or
`exceeds_kv_budget` (longer than `--ctx`), and `503 {"error": "decode_failed"}`.

`POST /close` `{"owner"}` drops (and, with `--state-dir`, saves) that owner's sessions and answers `{"closed": n}`.

Readouts are llama.cpp's per-token embeddings with pooling `NONE`: the final norm's output at each offset. A retained
session is one llama sequence holding the full-attention KV cache and the GatedDeltaNet recurrent state, so an append
continues it. Sessions share one KV cache of `--ctx` tokens: besides the `--sessions` limit, a request that would not
fit beside the sessions held evicts the least recently used ones (saving them, with `--state-dir`) first.

### Saved sessions

With `--state-dir DIR` (and `--persist-min`, default 16,384 tokens; `--state-max-gb`, default 32), a retained session
is saved when it is evicted, closed, or the server stops: its sequence state, its tokens and the rows it returned.
`--state-max-gb` counts every saved file; the oldest entries go first, and a session larger than the whole budget is
not saved.
Trust model: `--state-dir` is private to the user who runs the encoder; it is never a shared directory (`rayline`
passes one under `~/.rayline`). Saved sessions hold token ids, which encode the prompt:
- **Checks at start:** on POSIX, `--state-dir` must be the user's own directory, not a symlink, and not writable by
  group or others, or the encoder refuses to start. On Windows it must be under `%USERPROFILE%` (no mode check).
- **Location:** they live in a subdirectory of `--state-dir` named by the model file (its size, mtime and first and
  last 4 MB) and the encode configuration (backend, KV type, flash attention, whether the tensor kernels are
  actually on, `--budget`, `--ubatch`), so another model or configuration never restores them.
- **Permissions:** on POSIX that subdirectory must also be the user's own and not a symlink, and is made `0700`
  (checked at start). Every file in it is `0600`. On Windows it inherits the profile's ACL.
- **Atomic saves:** each save writes its state and rows under new file names, then publishes its index (`.json`) with
  an atomic rename. An interrupted save leaves only unpublished files, which the next start removes.
- **Restore checks:** an entry whose files are missing, malformed, the wrong size, or hold other tokens is skipped
  and re-encoded, never restored.
- **File names:** they come from hashes, never from request text.
- **Restarted sessions:** restarting a session id at `base` 0 re-publishes its saved entries under an anonymous id.
  A later append to that id therefore never resumes the old content, though a full encode whose tokens match can
  still reuse it as a prefix.
- **One owner per directory:** one encoder holds an exclusive lock on the directory (`flock`; `LockFileEx` on Windows).
  A second encoder started with the same `--state-dir` and configuration logs a warning and runs without saved
  sessions.
- **Refusals change nothing:** a refused request (unknown session, bad `base`) leaves every session as it was. An
append to a session the process does not hold resumes the saved state that ends at its `base`; a full encode whose
tokens start with a saved entry's tokens restores the longest such entry and encodes only the rest. The recurrent
state cannot be truncated, so an entry only resumes from its exact end. A process killed without a signal loses
unsaved sessions.

`--checkpoint N` (a multiple of `--budget`, with `--state-dir`) also saves the state after the first N tokens of a
longer full encode, so later conversations that share that prefix (an agent's system prompt and tools) restore it.

## Patches

`0001-embeddings-skip-lm-head-and-honor-output-flags.patch`: an embeddings context never reads logits, so the graph
drops the LM head for per-token (pooling `NONE`) embeddings too, and those contexts honor the batch's output flags
instead of outputting every token. Unpatched llama.cpp runs the 248,320-token vocabulary head on every token and
holds an `n_batch x vocab` float buffer (about 8 GB at 8,192 tokens); the patch removes that work and memory.

## Adding a platform

A new platform (for example Windows) ships only after all seven steps below pass on its target hardware.

1. **Build.** Add the backend to `build.sh` (`--backend`) behind the same pin checks: llama.cpp at the pinned commit,
   patches by sha256, no untracked files, static libraries, no curl or OpenSSL. GPU builds take an explicit
   architecture list (for example `--cuda-arch 89`). Compiling needs no GPU. Record the build time and binary size:
   CUDA takes about 24 minutes per architecture on 4 vCPUs and produces about 0.9 GB.
2. **Smoke test.** `smoke_test.py` passes with a small GGUF. It covers:
   - health and encode;
   - KV-space eviction;
   - the Host/Origin checks and input validation;
   - saved sessions: owner-only files, restore across restarts, and no restore under another configuration.
3. **Parity, on the target hardware.** Maintainers run the release's recorded-decision parity replay; the recordings
   aren't public. The bar:
   - the same action on at least 256 of 265 recorded decisions;
   - flips only on near-ties (top-2 score gap ≤ 0.001);
   - every clear-margin decision agrees;
   - identical token counts.

   Report the score-delta median, p95 and max. The M4 sits exactly at 256, so a dependency bump can tip it: re-run
   parity on every llama.cpp or patch change.
4. **Speed and memory.** Measure the first encode and a +2k-token append at 1.5k, 8k, 21.5k, 64k and 262k tokens,
   with RSS and GPU memory. A backend must do a 21.5k first encode in about 2 s or less. CPU-only bf16 fails this
   (77 s at 8k).
5. **Process integration.** `rayline` starts `rayline-arc-encoder[.exe]` from beside its own executable.
   - Shutdown must work through `--parent-pipe` (stdin closing), because Windows does not deliver SIGTERM.
   - Saved-session files must be replaced atomically on that OS (the server renames temporary files into place).
   - Document any per-hardware environment flag the launcher must set, such as `GGML_METAL_TENSOR_ENABLE=1` on
     Apple M4.
6. **Release.**
   - The release builds it in a job for that platform tag.
   - The asset `rayline-arc-encoder-<platform>` goes in that platform's archive and in `SHA256SUMS`.
   - The installers and `rayline update` install it only when `SHA256SUMS` lists it.
   - The binary is code-signed: notarized on macOS, Authenticode on Windows.
7. **End to end.** Run real `claude -p` and Pi turns in local mode through the sibling binary, with no
   `RAYLINE_ARC_ENCODER`. Then restart offline, from the cache.

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

### Performance reference

These numbers are indicative, not guarantees. They come from 2026-11-14 runs of this build (L4: the same source),
with the public ARC 1.0 encoder GGUF (bf16), `--budget 8192 --ubatch 2048`, and random tokens.
- **First encode** is one `/encode` call that encodes N tokens into a new retained session (rows read every 8,192
  tokens).
- **+2k** is one `/encode` call that appends 2,000 tokens to that session.

Both are timed by the client over loopback, so they include the local HTTP hop (a few ms) but not tokenization.
Values are the mean of 2–4 calls (1 call at 262k), with `--ctx 24576` up to 21.5k and `--ctx 270000` above it. The
M4 Max runs had `GGML_METAL_TENSOR_ENABLE=1`, as `rayline` sets on M4. The Mac was shared, at a load average of 6–13,
which accounts for its ranges.

| Context | M4 Max first / +2k | GB10 first / +2k | L4 first / +2k |
|---|---|---|---|
| 1.5k | 0.10 s / 0.14 s | 0.05 s / 0.07 s | 0.05 s / 0.08 s |
| 8k | 0.56 s / 0.17 s | 0.28 s / 0.08 s | 0.28 s / 0.09 s |
| 21.5k | 1.8 s / 0.23 s (2.5 s at `--ctx 270000`) | 0.73 s / 0.08 s | 0.84 s / 0.11 s |
| 64k | 8.7–12.5 s / 0.47–0.63 s | 2.9 s / 0.13 s | 4.0 s / 0.23 s |
| 262k | 134 s / 2.5 s | 22.0 s / 0.31 s | 50.1 s / 0.83 s |

The first-encode time scales with the context the server allocates (`--ctx`), not only the input: `rayline` starts
the encoder at `--ctx 131072`, where an M4 Max 21.5k first encode takes 1.8–2.8 s.

**Saved sessions (M4 Max, 21.5k tokens, `--state-dir`):**

| Step | Time | Size |
|---|---|---|
| Save | 0.20 s | 155 MB |
| Restore | 16 ms | |
| Restore plus a +2k append | 0.24 s | |
| Re-encoding the session instead | 1.8 s | |

**Tensor kernels on M4 Max:** in back-to-back runs at `--ctx 270000` and equal load, they cut the 64k first encode
from about 20.3 s to 12.3 s, and the +2k append from 1.3 s to 0.62 s. At 21.5k the difference was within noise.

**Memory, fixed mostly by `--ctx`:**

| `--ctx` | M4 Max RSS / footprint | GB10 RSS / GPU | L4 RSS / GPU |
|---|---|---|---|
| 24,576 | 1.72 GB / 0.86 GB | 1.3 GB / 1.6 GB | 1.35 GB / 1.6 GB |
| 131,072 | 2.72 GB / 1.86 GB | | |
| 270,000 | 5.22 GB / 4.37 GB (after a 262k session) | 2.4 GB / 4.0 GB | 2.27 GB / 4.0 GB |

On the M4 Max:
- **RSS** includes the 1 GB of memory-mapped weights.
- **Footprint** is macOS's peak memory footprint (unified memory; it includes Metal buffers), from `/usr/bin/time -l`.

## License

Rayline's code here (`arc_encoder_server.cpp`, `build.sh`, `smoke_test.py`, and the patch) is under the repository's
Apache License 2.0. The binary statically links llama.cpp and ggml (MIT, the ggml authors), cpp-httplib (MIT,
yhirose) and nlohmann/json (MIT, Niels Lohmann), all from the pinned llama.cpp checkout. `build.sh` copies their
license texts into `rayline-arc-encoder-LICENSE`, which ships beside the binary.
