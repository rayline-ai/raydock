# Persistence follow-up: notes

Measured 2026-11-14 on an M4 Max, ARC 1.0 GGUF, four real `claude -p` conversations (different first user turns,
same system prompt and tools) through rayline's local ARC decisions, pre-split encoder with `--checkpoint 8192` and
an empty `--state-dir`:

- First encode of a new conversation: 1.78 s (no checkpoint yet), then 1.94, 1.30 and 1.32 s with 8,192 tokens
  restored from the checkpoint. A full saved-entry hit (a repeated identical prompt) is what gives ~0.16 s.
- Each encoder input is ~20,750 tokens. Conversations share only the first 11,441 tokens; the user turn follows,
  then a 9.3k-token tail that is identical across conversations but cannot be reused after the differing turn.
- So a prefix snapshot at a chunk multiple (8,192) saves ~0.4 s per new conversation, the same as `--checkpoint`.
  A snapshot at the exact shared boundary (11,441) would save ~0.8 s but changes the decode chunk boundaries, so it
  needs a full parity check first.
- An in-memory alternative needs no disk: llama.cpp's `llama_memory_seq_cp` shares the recurrent (GatedDeltaNet)
  cell and the unified-KV cells, so a snapshot sequence costs one sequence slot and no copied KV.

Planned here: lookup by token prefix only (drop the session-id resume path), a disk limit that counts every file,
and deleting old identity directories.
