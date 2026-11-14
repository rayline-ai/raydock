# rayline-arc-encoder

The local ARC encoder. Rayline's local routing starts it to read the ARC encoder's final-layer state at chosen token
offsets, on this Mac, instead of asking a hosted router. It is a small HTTP server on llama.cpp (`libllama`, static,
Metal) that runs the public ARC encoder GGUF (`rayline-ai/ARC-1.0`). It ships for macOS arm64 only.

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
sha256 is not the pinned one. The binary links only system frameworks (Metal, Foundation, Accelerate), has the Metal
shaders embedded, and needs macOS 13 or later. It is built without curl or OpenSSL: it never downloads anything.

## Running it

```bash
rayline-arc-encoder --model arc-encoder-bf16.gguf --port 0 --budget 8192 --ctx 131072 \
  --state-dir ~/.rayline/local/arc-sessions --parent-pipe
```

`--port 0` binds a free loopback port and prints `{"listening": PORT}` as the first stdout line. `--parent-pipe` stops
the server (saving its sessions) when its stdin closes. `--budget` is the tokens per decode call and must equal the
encoder release's chunk size (8192 for ARC 1.0). `--help` lists every flag.

`rayline` finds the encoder beside its own executable; `RAYLINE_ARC_ENCODER` names another binary.

On Apple M4 chips, llama.cpp leaves its Metal tensor-API kernels off by default; this model's flash attention
(head_dim 256) is faster with them, so a launcher may set `GGML_METAL_TENSOR_ENABLE=1` there. M5 and later use them
already. `--no-tensor` turns them off everywhere.

## Protocol

All requests and errors are JSON; a successful `/encode` answers raw rows.

`GET /health` answers `{"engine", "depth", "budget", "rope", "max_model_len", "layer", "dim", "sessions",
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
continues it.

### Saved sessions

With `--state-dir DIR` (and `--persist-min`, default 16,384 tokens; `--state-max-gb`, default 32), a retained session
is saved when it is evicted, closed, or the server stops: its sequence state, its tokens and the rows it returned. An
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

## License

Rayline's code here (`arc_encoder_server.cpp`, `build.sh`, `smoke_test.py`, and the patch) is under the repository's
Apache License 2.0. The binary statically links llama.cpp and ggml (MIT, the ggml authors), cpp-httplib (MIT,
yhirose) and nlohmann/json (MIT, Niels Lohmann), all from the pinned llama.cpp checkout. `build.sh` copies their
license texts into `rayline-arc-encoder-LICENSE`, which ships beside the binary.
