// rayline-arc-encoder: a loopback HTTP server that runs the ARC encoder (a GGUF model) on llama.cpp and returns
// its final-layer state at requested token offsets. Rayline's local routing starts it and reads those rows.
//
// Protocol (JSON requests; every response but a successful /encode is JSON):
//
//   GET  /health  -> {"engine","depth","budget","rope","max_model_len","layer","dim","sessions",
//                     "capabilities","checkpoint"}
//   POST /encode  {"tokens": [id...], "offsets": [abs...], "base": N,
//                  "session": "id" | "retain": false, ["owner": "id"], ["images": [...]]}
//                 `tokens` are the token ids at positions base..; `offsets` are absolute positions in
//                 [base, base + len(tokens)), strictly ascending. With "session", the tokens are kept as a retained
//                 session: base 0 starts (or replaces) it, base N appends to a session that holds exactly N tokens.
//                 With "retain": false (base 0 only), nothing is kept.
//                 -> 200 raw little-endian float32 rows, one per offset (headers x-rows, x-dim, x-timing-ms,
//                    x-restored-tokens);
//                    409 {"error":"unknown_session"}: an append to a session not held at that base;
//                    422 {"error": code}: invalid_body, offsets_out_of_range, invalid_retain, images_not_served,
//                    exceeds_kv_budget (input longer than --ctx);
//                    503 {"error":"decode_failed"}.
//   POST /close   {"owner"} -> {"closed": n}: drop (and save, with --state-dir) that owner's sessions.
//
// With --port 0 the server binds a free port and prints {"listening": PORT} as its first stdout line. With
// --parent-pipe it stops when its stdin closes, so it never outlives the process that started it.
//
// With --state-dir, a retained session of at least --persist-min tokens is saved to disk when it is evicted, closed
// or the server stops (SIGINT/SIGTERM, or stdin closing with --parent-pipe): its llama sequence state (full-attention
// KV plus GatedDeltaNet state), its tokens, and the readout rows it returned. A later request restores instead of
// re-encoding:
//   - an append (base > 0) to a session this process does not hold resumes the saved state of that session at base;
//   - a full encode (base 0) whose tokens start with a saved entry's tokens restores the longest such entry and encodes
//     only the rest, provided every requested offset inside the saved prefix has a saved row.
// The recurrent state cannot be truncated, so an entry only ever resumes from its exact end.
//
// Readouts are llama.cpp's per-token embeddings with pooling NONE: the final norm's output, read at each requested
// absolute offset. A retained session is one llama sequence id holding both the full-attention KV and the
// GatedDeltaNet recurrent state, so an append continues it; an unretained encode uses a scratch sequence that is
// cleared afterwards. Text only: images are refused (images_not_served).

#include "llama.h"
#include "ggml-backend.h"
#include "httplib.h"
#include "nlohmann/json.hpp"

#include <cctype>
#include <chrono>
#include <csignal>
#include <filesystem>
#include <fstream>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <vector>
#ifdef _WIN32
#define NOMINMAX
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <cwctype>
#else
#include <fcntl.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <unistd.h>
#endif

using json = nlohmann::json;

namespace {

namespace fs = std::filesystem;

struct Session {
    llama_seq_id seq = -1;
    int32_t n_past = 0;          // tokens held: the next append's base
    std::string owner;
    uint64_t last_used = 0;
    std::vector<llama_token> tokens;           // every token held (kept for persistence)
    std::map<int32_t, std::vector<float>> rows; // readouts returned, by absolute offset
    int32_t saved_at = 0;                      // n_past when last persisted (0: never)
};

// A persisted session on disk: <stem>.state (llama sequence state + tokens), <stem>.rows, <stem>.json.
struct Entry {
    std::string stem;
    std::string session;
    std::vector<llama_token> tokens;
    std::vector<int32_t> offsets;  // offsets of the saved rows, ascending
    uint64_t bytes = 0;
    int64_t mtime = 0;
};

struct Args {
    std::string model;
    std::string host = "127.0.0.1";
    int port = 18555;
    int n_ctx = 65536;           // shared (unified) KV over every session
    int budget = 8192;           // tokens per llama_decode call (the encoder identity's chunk_tokens)
    int ubatch = 2048;           // physical batch llama.cpp splits each call into (compute buffer size)
    int sessions = 4;            // retained sessions; one more sequence is the unretained scratch
    std::string kv_type = "f16"; // f16 | bf16 | f32
    bool flash_attn = true;
    std::string engine_suffix;
    std::string state_dir;       // empty: no persistence
    int persist_min = 16384;     // smallest session worth saving
    double state_max_gb = 32.0;  // disk budget; the oldest entries go first
    bool parent_pipe = false;    // exit when stdin closes (a supervising parent went away)
    int threads = 0;             // CPU threads; 0: libllama's default (4)
    bool tensor = true;          // false: never use Metal tensor-API kernels (GGML_METAL_TENSOR_DISABLE)
    // > 0: a full encode longer than this many tokens saves its state at this length (a shared
    // prefix: an agent's system prompt and tools), so other sessions with the same first tokens restore it.
    // A multiple of `budget`, so the decode chunks are the ones an uninterrupted encode uses.
    int checkpoint = 0;
};

struct Server {
    Args args;
    llama_model * model = nullptr;
    llama_context * ctx = nullptr;
    int dim = 0;
    std::mutex mu;
    std::map<std::string, Session> sessions;
    std::vector<bool> seq_used;
    uint64_t clock = 0;
    std::string engine;

    std::vector<Entry> entries;

    llama_seq_id scratch() const { return args.sessions; }

    uint64_t generation = 0;
    // Every save gets its own file names: a save never overwrites the files of an entry that is still published.
    std::string stem_for(const std::string & session, int32_t n) {
        const auto now = std::chrono::system_clock::now().time_since_epoch().count();
        return std::to_string(std::hash<std::string>{}(session)) + "-" + std::to_string(n) + "-" +
               std::to_string((uint64_t) now) + "-" + std::to_string(++generation);
    }

    void load_index() {
        if (args.state_dir.empty()) return;
        // Files no published index (.json) names are leftovers of an interrupted save: remove them.
        for (const auto & f : fs::directory_iterator(args.state_dir)) {
            const fs::path path = f.path();
            const fs::path stem = path.parent_path() / path.stem();
            const bool tmp = path.extension() == ".tmp";
            const bool orphan = (path.extension() == ".state" || path.extension() == ".rows") && !fs::exists(stem.string() + ".json");
            if (tmp || orphan) { std::error_code ec; fs::remove(path, ec); }
        }
        for (const auto & f : fs::directory_iterator(args.state_dir)) {
            if (f.path().extension() != ".json") continue;
            // A damaged or partial entry is skipped, never fatal.
            try {
                std::ifstream in(f.path());
                json j = json::parse(in, nullptr, false);
                const fs::path base = f.path().parent_path() / f.path().stem();
                if (j.is_discarded() || !fs::exists(base.string() + ".state") || !fs::exists(base.string() + ".rows")) continue;
                Entry e;
                e.stem = f.path().stem().string();
                e.session = j.at("session").get<std::string>();
                e.tokens = j.at("tokens").get<std::vector<llama_token>>();
                e.offsets = j.at("offsets").get<std::vector<int32_t>>();
                if (!rows_file_ok(base.string() + ".rows", e.offsets.size())) continue;
                e.bytes = stored_bytes(base, ".json");
                e.mtime = j.value("saved", (int64_t) 0);
                entries.push_back(std::move(e));
            } catch (const std::exception & ex) {
                std::fprintf(stderr, "skipping saved session %s: %s\n", f.path().filename().c_str(), ex.what());
            }
        }
        std::fprintf(stderr, "rayline-arc-encoder: %zu saved sessions in %s\n", entries.size(), args.state_dir.c_str());
    }

    // A rows file holds exactly one dim-wide float32 row per saved offset.
    bool rows_file_ok(const std::string & path, size_t n) const {
        std::error_code ec;
        const auto size = fs::file_size(path, ec);
        return !ec && size == n * (size_t) dim * sizeof(float);
    }

    // A session id restarted at base 0 holds new content: its saved entries no longer describe it. Re-publish them under
    // an id no request can name, so an append to the id never resumes the old content, while a full encode whose
    // tokens match can still reuse them as a prefix.
    void disown(const std::string & id) {
        for (auto & e : entries) {
            if (e.session != id) continue;
            const fs::path base = fs::path(args.state_dir) / e.stem;
            const std::string anonymous = "replaced-" + e.stem;
            std::error_code ec;
            {
                std::ofstream meta(base.string() + ".json.tmp");
                meta << json{{"session", anonymous}, {"tokens", e.tokens}, {"offsets", e.offsets}, {"saved", e.mtime}}.dump();
                meta.close();
                if (!meta) { fs::remove(base.string() + ".json.tmp", ec); fs::remove(base.string() + ".json", ec); }
                else fs::rename(base.string() + ".json.tmp", base.string() + ".json", ec);
            }
            if (ec) fs::remove(base.string() + ".json", ec);  // cannot re-publish: unpublish instead
            e.session = anonymous;
        }
    }

    void remove_entry(size_t i) {
        const fs::path base = fs::path(args.state_dir) / entries[i].stem;
        for (const char * ext : {".json", ".state", ".rows"}) { std::error_code ec; fs::remove(base.string() + ext, ec); }
        entries.erase(entries.begin() + (long) i);
    }

    // Save a retained session (if persistence is on and it is big enough and has grown since its last save).
    static bool is_checkpoint(const std::string & session) { return session.rfind("checkpoint-", 0) == 0; }

    // The session id of the checkpoint for these first tokens.
    static std::string checkpoint_id(const std::vector<llama_token> & tokens, int32_t n) {
        uint64_t h = 1469598103934665603ull;  // FNV-1a over the token ids
        for (int32_t i = 0; i < n; ++i) { h ^= (uint32_t) tokens[i]; h *= 1099511628211ull; }
        return "checkpoint-" + std::to_string(h) + "-" + std::to_string(n);
    }

    bool has_checkpoint(const std::vector<llama_token> & tokens, int32_t n) const {
        const std::string id = checkpoint_id(tokens, n);
        for (const auto & e : entries)
            if (e.session == id && (int32_t) e.tokens.size() == n &&
                std::equal(e.tokens.begin(), e.tokens.end(), tokens.begin())) return true;
        return false;
    }

    // Bytes an entry holds on disk: its state, rows and index (`json_ext`: ".json", or ".json.tmp" before publishing).
    static uint64_t stored_bytes(const fs::path & base, const char * json_ext) {
        uint64_t total = 0;
        for (const std::string ext : {std::string(".state"), std::string(".rows"), std::string(json_ext)}) {
            std::error_code ec;
            const auto size = fs::file_size(base.string() + ext, ec);
            if (!ec) total += size;
        }
        return total;
    }

    bool warned_too_big = false;

    void persist(const std::string & id, Session & s) {
        if (args.state_dir.empty() || (s.n_past < args.persist_min && !is_checkpoint(id)) || s.n_past == s.saved_at) return;
        const auto t0 = std::chrono::steady_clock::now();
        const std::string stem = stem_for(id, s.n_past);
        const fs::path base = fs::path(args.state_dir) / stem;
        // The state and rows go under this save's own stem; the index (.json) is then published by an atomic rename. An
        // interrupted save leaves only unpublished files, which load_index() removes.
        auto discard = [&]() {
            for (const char * ext : {".state", ".rows", ".json.tmp"}) { std::error_code ec; fs::remove(base.string() + ext, ec); }
            std::fprintf(stderr, "persist %s failed\n", id.c_str());
        };
        if (llama_state_seq_save_file(ctx, (base.string() + ".state").c_str(), s.seq, s.tokens.data(), s.tokens.size()) == 0)
            return discard();
        Entry e;
        e.stem = stem;
        e.session = id;
        e.tokens = s.tokens;
        {
            std::ofstream rows(base.string() + ".rows", std::ios::binary);
            for (const auto & [off, row] : s.rows) {
                e.offsets.push_back(off);
                rows.write(reinterpret_cast<const char *>(row.data()), (std::streamsize) (row.size() * sizeof(float)));
            }
            rows.close();
            if (!rows) return discard();
        }
        e.mtime = std::chrono::duration_cast<std::chrono::seconds>(std::chrono::system_clock::now().time_since_epoch()).count();
        {
            std::ofstream meta(base.string() + ".json.tmp");
            meta << json{{"session", id}, {"tokens", e.tokens}, {"offsets", e.offsets}, {"saved", e.mtime}}.dump();
            meta.close();
            if (!meta) return discard();
        }
        e.bytes = stored_bytes(base, ".json.tmp");
        const uint64_t limit = (uint64_t) (args.state_max_gb * 1e9);
        if (e.bytes > limit) {
            // Larger than the whole disk budget: never saved, so the budget holds. Not retried until the session grows.
            for (const char * ext : {".state", ".rows", ".json.tmp"}) { std::error_code ec; fs::remove(base.string() + ext, ec); }
            if (!warned_too_big) {
                std::fprintf(stderr, "warning: a %.2f GB saved session exceeds --state-max-gb; such sessions are not saved\n",
                             (double) e.bytes / 1e9);
                warned_too_big = true;
            }
            s.saved_at = s.n_past;
            return;
        }
        {
            std::error_code ec;
            fs::rename(base.string() + ".json.tmp", base.string() + ".json", ec);
            if (ec) return discard();
        }
        // drop older entries of this session (a session only grows), then enforce the disk budget, oldest first
        for (size_t i = entries.size(); i-- > 0;) if (entries[i].session == id) remove_entry(i);
        entries.push_back(std::move(e));
        uint64_t total = 0;
        for (const auto & x : entries) total += x.bytes;
        while (total > limit && entries.size() > 1) {  // the newest entry fits on its own (checked above)
            size_t oldest = 0;
            for (size_t i = 1; i < entries.size(); ++i) if (entries[i].mtime < entries[oldest].mtime) oldest = i;
            total -= entries[oldest].bytes;
            remove_entry(oldest);
        }
        s.saved_at = s.n_past;
        const double ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count();
        std::fprintf(stderr, "persisted %s: %d tokens in %.0f ms\n", id.c_str(), s.n_past, ms);
    }

    // Restore entry `e` into `seq` for session `id` (empty: unretained), filling the session's tokens and rows. False if
    // llama.cpp refuses the state or its files do not match the index.
    bool restore(const Entry & e, llama_seq_id seq, Session & s, const std::string & id) {
        const auto t0 = std::chrono::steady_clock::now();
        const fs::path base = fs::path(args.state_dir) / e.stem;
        llama_memory_seq_rm(llama_get_memory(ctx), seq, -1, -1);
        std::vector<llama_token> toks(e.tokens.size());
        size_t n = 0;
        if (llama_state_seq_load_file(ctx, (base.string() + ".state").c_str(), seq, toks.data(), toks.size(), &n) == 0 ||
            n != e.tokens.size() || toks != e.tokens) {
            llama_memory_seq_rm(llama_get_memory(ctx), seq, -1, -1);
            std::fprintf(stderr, "restore %s failed\n", e.stem.c_str());
            return false;
        }
        std::map<int32_t, std::vector<float>> restored;
        std::ifstream rows(base.string() + ".rows", std::ios::binary);
        bool ok = rows_file_ok(base.string() + ".rows", e.offsets.size());
        for (size_t i = 0; ok && i < e.offsets.size(); ++i) {
            std::vector<float> row(dim);
            ok = (bool) rows.read(reinterpret_cast<char *>(row.data()), (std::streamsize) (dim * sizeof(float)));
            restored[e.offsets[i]] = std::move(row);
        }
        if (!ok) {
            llama_memory_seq_rm(llama_get_memory(ctx), seq, -1, -1);
            std::fprintf(stderr, "restore %s failed: its rows file is damaged\n", e.stem.c_str());
            return false;
        }
        s.rows = std::move(restored);
        s.tokens = e.tokens;
        s.n_past = (int32_t) e.tokens.size();
        // Saved only if the entry is this session's own; a session restored from another's entry is saved under its id.
        s.saved_at = e.session == id ? s.n_past : 0;
        const double ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count();
        std::fprintf(stderr, "restored %s: %d tokens in %.0f ms\n", e.stem.c_str(), s.n_past, ms);
        return true;
    }

    // The longest saved entry whose tokens prefix `tokens` and that holds a row for every offset below its end.
    const Entry * best_prefix(const std::vector<llama_token> & tokens, const std::vector<int32_t> & offsets) const {
        const Entry * best = nullptr;
        for (const auto & e : entries) {
            const size_t n = e.tokens.size();
            if (n > tokens.size() || (best && n <= best->tokens.size()) || ((int) n < args.persist_min && !is_checkpoint(e.session))) continue;
            if (!std::equal(e.tokens.begin(), e.tokens.end(), tokens.begin())) continue;
            bool covered = true;
            for (int32_t off : offsets) {
                if (off >= (int32_t) n) break;
                if (!std::binary_search(e.offsets.begin(), e.offsets.end(), off)) { covered = false; break; }
            }
            if (covered) best = &e;
        }
        return best;
    }

    void drop(const Session & s) { llama_memory_seq_rm(llama_get_memory(ctx), s.seq, -1, -1); seq_used[s.seq] = false; }

    // The KV cache is shared by every sequence, so a session count alone does not bound it: before decoding up to
    // `total` tokens for `keep` (empty: the unretained scratch), evict least recently used sessions until the
    // tokens held fit in the context.
    void make_room(const std::string & keep, int64_t total) {
        for (;;) {
            int64_t held = total;
            auto lru = sessions.end();
            for (auto it = sessions.begin(); it != sessions.end(); ++it) {
                if (it->first == keep) continue;
                held += it->second.n_past;
                if (lru == sessions.end() || it->second.last_used < lru->second.last_used) lru = it;
            }
            if (held <= (int64_t) llama_n_ctx(ctx) || lru == sessions.end()) return;
            std::fprintf(stderr, "evicting %s (%d tokens) for KV space\n", lru->first.c_str(), lru->second.n_past);
            persist(lru->first, lru->second);
            drop(lru->second);
            sessions.erase(lru);
        }
    }

    // A free sequence id for a new retained session, evicting the least recently used one when all are taken.
    llama_seq_id take_seq() {
        for (int i = 0; i < args.sessions; ++i) {
            if (!seq_used[i]) { seq_used[i] = true; return i; }
        }
        auto lru = sessions.begin();
        for (auto it = sessions.begin(); it != sessions.end(); ++it) {
            if (it->second.last_used < lru->second.last_used) lru = it;
        }
        llama_seq_id seq = lru->second.seq;
        persist(lru->first, lru->second);
        drop(lru->second);
        sessions.erase(lru);
        seq_used[seq] = true;
        return seq;
    }

    // Decode `tokens` at positions base.. on `seq`, returning the rows at `offsets` (absolute, sorted ascending,
    // each in [base, base + tokens.size())). Empty optional on a decode failure (the caller clears the sequence).
    std::optional<std::vector<float>> run(llama_seq_id seq, int32_t base, const std::vector<llama_token> & tokens,
                                          const std::vector<int32_t> & offsets) {
        std::vector<float> rows;
        rows.reserve(offsets.size() * dim);
        size_t next = 0;
        const int32_t n = (int32_t) tokens.size();
        llama_batch batch = llama_batch_init(args.budget, 0, 1);
        for (int32_t start = 0; start < n; start += args.budget) {
            const int32_t stop = std::min(n, start + args.budget);
            batch.n_tokens = 0;
            std::vector<int32_t> wanted;
            for (int32_t i = start; i < stop; ++i) {
                const int32_t k = batch.n_tokens++;
                batch.token[k] = tokens[i];
                batch.pos[k] = base + i;
                batch.n_seq_id[k] = 1;
                batch.seq_id[k][0] = seq;
                const bool out = next < offsets.size() && offsets[next] == base + i;
                batch.logits[k] = out;
                if (out) { wanted.push_back(k); ++next; }
            }
            if (llama_decode(ctx, batch) != 0) { llama_batch_free(batch); return std::nullopt; }
            for (int32_t k : wanted) {
                const float * e = llama_get_embeddings_ith(ctx, k);
                if (e == nullptr) { llama_batch_free(batch); return std::nullopt; }
                rows.insert(rows.end(), e, e + dim);
            }
        }
        llama_batch_free(batch);
        return rows;
    }
};

// Requests must say they are JSON: a web page cannot send that content type to this loopback port without a CORS
// preflight, which this server never answers.
bool is_json(const httplib::Request & req) {
    return req.get_header_value("Content-Type").rfind("application/json", 0) == 0;
}

// The backend that runs the model: the first GPU device's backend ("metal", "cuda", ...), else "cpu".
std::string backend_name() {
    for (size_t i = 0; i < ggml_backend_dev_count(); ++i) {
        ggml_backend_dev_t dev = ggml_backend_dev_get(i);
        const auto type = ggml_backend_dev_type(dev);
        if (type != GGML_BACKEND_DEVICE_TYPE_GPU && type != GGML_BACKEND_DEVICE_TYPE_IGPU) continue;
        std::string name = ggml_backend_reg_name(ggml_backend_dev_backend_reg(dev));
        for (auto & c : name) c = (char) std::tolower((unsigned char) c);
        return name == "mtl" ? "metal" : name;
    }
    return "cpu";
}

// `j` as an array of integers in [0, max]: false for anything else (floats, negatives, values that would wrap).
bool int_array(const json & j, int64_t max, std::vector<int32_t> & out) {
    if (!j.is_array()) return false;
    out.clear();
    out.reserve(j.size());
    for (const auto & v : j) {
        if (v.is_number_unsigned()) {
            if (v.get<uint64_t>() > (uint64_t) max) return false;
            out.push_back((int32_t) v.get<uint64_t>());
        } else if (v.is_number_integer()) {
            const int64_t x = v.get<int64_t>();
            if (x < 0 || x > max) return false;
            out.push_back((int32_t) x);
        } else {
            return false;
        }
    }
    return true;
}

// What saved state depends on: the model file and the encode configuration. The model is identified by its size, mtime
// and first and last 4 MB (the GGUF header and tensor data), which is cheap at every start; saved sessions live in a
// subdirectory named by this identity, so another model or configuration never restores them.
std::string state_identity(const std::string & model, const std::string & config) {
    uint64_t h = 1469598103934665603ull;
    auto mix = [&](const void * data, size_t n) {
        const auto * p = static_cast<const unsigned char *>(data);
        for (size_t i = 0; i < n; ++i) { h ^= p[i]; h *= 1099511628211ull; }
    };
    std::error_code ec;
    const uint64_t size = fs::file_size(model, ec);
    const auto mtime = fs::last_write_time(model, ec).time_since_epoch().count();
    mix(&size, sizeof size);
    mix(&mtime, sizeof mtime);
    std::ifstream in(model, std::ios::binary);
    std::vector<char> buf(4 << 20);
    in.read(buf.data(), (std::streamsize) buf.size());
    mix(buf.data(), (size_t) in.gcount());
    if (size > buf.size()) {
        in.clear();
        in.seekg((std::streamoff) (size - buf.size()));
        in.read(buf.data(), (std::streamsize) buf.size());
        mix(buf.data(), (size_t) in.gcount());
    }
    mix(config.data(), config.size());
    char hex[17];
    std::snprintf(hex, sizeof hex, "%016llx", (unsigned long long) h);
    return hex;
}

// One process owns a saved-session directory at a time: start-up cleanup and saves would otherwise race with another
// encoder's. The lock is held until the process exits. False if another process holds it.
bool lock_dir(const fs::path & dir) {
    const fs::path path = dir / "lock";
#ifdef _WIN32
    HANDLE h = CreateFileW(path.c_str(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr,
                           OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (h == INVALID_HANDLE_VALUE) return false;
    OVERLAPPED overlapped = {};
    return LockFileEx(h, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY, 0, 1, 0, &overlapped) != 0;
#else
    const int fd = open(path.c_str(), O_RDWR | O_CREAT | O_CLOEXEC, 0600);
    if (fd < 0) return false;
    if (flock(fd, LOCK_EX | LOCK_NB) != 0) { close(fd); return false; }
    return true;  // the descriptor stays open, holding the lock
#endif
}

// Trust model: --state-dir is private to the user running the encoder (the daemon passes one under ~/.rayline). It is
// not a directory shared with other users or processes, so it must be the user's own, not a symlink, and writable by
// nobody else. Saved sessions hold token ids (the prompt), so their identity subdirectory is also owner-only.
// `create` makes a missing directory first. On Windows the directory must be under the user's profile (its ACL keeps
// it private) and no mode check applies.
bool trusted_dir(const fs::path & dir, bool owner_only, std::string & why) {
    std::error_code ec;
    if (!fs::exists(fs::symlink_status(dir, ec))) {
        fs::create_directory(dir, ec);
        if (ec) { why = "cannot create it: " + ec.message(); return false; }
    }
#ifdef _WIN32
    (void) owner_only;
    const char * profile = std::getenv("USERPROFILE");
    const auto norm = [](const fs::path & p) {
        std::error_code e;
        std::wstring w = fs::weakly_canonical(p, e).wstring();
        for (auto & c : w) c = (wchar_t) towlower(c);
        return w;
    };
    if (profile == nullptr || norm(dir).rfind(norm(profile), 0) != 0) { why = "it is not under %USERPROFILE%"; return false; }
    if (fs::is_symlink(fs::symlink_status(dir, ec))) { why = "it is a symlink"; return false; }
    return true;
#else
    struct stat st;
    if (lstat(dir.c_str(), &st) != 0) { why = "cannot stat it"; return false; }
    if (S_ISLNK(st.st_mode)) { why = "it is a symlink"; return false; }
    if (!S_ISDIR(st.st_mode)) { why = "it is not a directory"; return false; }
    if (st.st_uid != getuid()) { why = "another user owns it"; return false; }
    if (owner_only && (st.st_mode & 077) != 0 && chmod(dir.c_str(), 0700) != 0) { why = "cannot make it owner-only"; return false; }
    if (lstat(dir.c_str(), &st) != 0) { why = "cannot stat it"; return false; }
    if ((st.st_mode & (owner_only ? 077 : 022)) != 0) {
        why = owner_only ? "it is not owner-only" : "other users can write to it";
        return false;
    }
    return true;
#endif
}

void refuse(httplib::Response & res, int status, const std::string & code) {
    res.status = status;
    res.set_content(json{{"error", code}}.dump(), "application/json");
}

// A supervisor reads this (`--help`) to learn which optional flags the build takes.
void usage() {
    std::fprintf(stderr, "usage: rayline-arc-encoder --model F.gguf [--port P] [--ctx N] [--budget N] "
                         "[--ubatch N] [--sessions N] [--kv-type f16|bf16|f32] [--no-flash-attn] [--no-tensor]\n"
                         "       [--threads N] [--host H]\n"
                         "       [--state-dir DIR] [--persist-min N] [--state-max-gb G] [--parent-pipe]\n"
                         "       [--checkpoint N (a multiple of --budget; needs --state-dir)]\n");
}

bool parse_args(int argc, char ** argv, Args & a) {
    for (int i = 1; i < argc; ++i) {
        std::string k = argv[i];
        auto val = [&]() -> std::string { return i + 1 < argc ? argv[++i] : ""; };
        if (k == "--model") a.model = val();
        else if (k == "--host") a.host = val();
        else if (k == "--port") a.port = std::atoi(val().c_str());
        else if (k == "--ctx") a.n_ctx = std::atoi(val().c_str());
        else if (k == "--budget") a.budget = std::atoi(val().c_str());
        else if (k == "--ubatch") a.ubatch = std::atoi(val().c_str());
        else if (k == "--sessions") a.sessions = std::atoi(val().c_str());
        else if (k == "--kv-type") a.kv_type = val();
        else if (k == "--no-flash-attn") a.flash_attn = false;
        else if (k == "--engine-suffix") a.engine_suffix = val();
        else if (k == "--state-dir") a.state_dir = val();
        else if (k == "--persist-min") a.persist_min = std::atoi(val().c_str());
        else if (k == "--state-max-gb") a.state_max_gb = std::atof(val().c_str());
        else if (k == "--no-tensor") a.tensor = false;
        else if (k == "--threads") a.threads = std::atoi(val().c_str());
        else if (k == "--parent-pipe") a.parent_pipe = true;
        else if (k == "--checkpoint") a.checkpoint = std::atoi(val().c_str());
        else if (k == "--help") { usage(); return false; }
        else { std::fprintf(stderr, "unknown argument %s\n", k.c_str()); usage(); return false; }
    }
    if (a.model.empty() || a.budget <= 0 || a.sessions <= 0 || a.n_ctx <= 0 || a.budget > a.n_ctx || a.checkpoint < 0 ||
        a.checkpoint % a.budget != 0 || (a.checkpoint > 0 && a.state_dir.empty())) {
        usage();
        return false;
    }
    return true;
}

// Whether llama.cpp's Metal backend enabled its tensor-API kernels. That depends on the chip, GGML_METAL_TENSOR_ENABLE
// / _DISABLE, --no-tensor and a runtime compile check. The pinned llama.cpp has no API for the outcome (it reports it
// only in its log, "has tensor = true|false"), so the log is read for it. If that line is missing (a log change), the
// state is unknown: the engine string, and so the saved-state identity, says so instead of guessing.
bool g_tensor_seen = false;
bool g_tensor = false;
void on_log(ggml_log_level, const char * text, void *) {
    if (std::strstr(text, "has tensor") != nullptr) {
        g_tensor_seen = true;
        g_tensor = std::strstr(text, "= true") != nullptr;
    }
    std::fputs(text, stderr);
}

std::string tensor_tag(const std::string & backend) {
    if (backend != "metal") return "";
    if (!g_tensor_seen) {
        std::fprintf(stderr, "warning: cannot tell whether the Metal tensor kernels are on; saved sessions use their own "
                             "identity\n");
        return "+tensor-unknown";
    }
    return g_tensor ? "+tensor" : "";
}

void set_env(const char * name, const char * value) {
#ifdef _WIN32
    _putenv_s(name, value);
#else
    setenv(name, value, 1);
#endif
}

httplib::Server * g_http = nullptr;
void on_signal(int) { if (g_http) g_http->stop(); }

}  // namespace

int main(int argc, char ** argv) {
    Server s;
    if (!parse_args(argc, argv, s.args)) return 2;
    const Args & a = s.args;

    // llama.cpp uses the Metal tensor API on M5 and later, or when GGML_METAL_TENSOR_ENABLE is set (a launcher sets
    // it on M4, where this model's flash attention is faster with it); --no-tensor turns it off everywhere.
    if (!a.tensor) set_env("GGML_METAL_TENSOR_DISABLE", "1");
    llama_log_set(on_log, nullptr);
    llama_backend_init();
    llama_model_params mp = llama_model_default_params();
    mp.n_gpu_layers = 999;
    s.model = llama_model_load_from_file(a.model.c_str(), mp);
    if (s.model == nullptr) { std::fprintf(stderr, "failed to load %s\n", a.model.c_str()); return 1; }
    s.dim = llama_model_n_embd(s.model);

    llama_context_params cp = llama_context_default_params();
    cp.n_ctx = a.n_ctx;
    cp.n_batch = a.budget;
    cp.n_ubatch = std::min(a.ubatch, a.budget);
    cp.n_seq_max = a.sessions + 1;
    cp.kv_unified = true;
    cp.embeddings = true;
    cp.pooling_type = LLAMA_POOLING_TYPE_NONE;
    cp.flash_attn_type = a.flash_attn ? LLAMA_FLASH_ATTN_TYPE_ENABLED : LLAMA_FLASH_ATTN_TYPE_DISABLED;
    const ggml_type kv = a.kv_type == "bf16" ? GGML_TYPE_BF16 : a.kv_type == "f32" ? GGML_TYPE_F32 : GGML_TYPE_F16;
    cp.type_k = kv;
    cp.type_v = kv;
    if (a.threads > 0) { cp.n_threads = a.threads; cp.n_threads_batch = a.threads; }
    s.ctx = llama_init_from_model(s.model, cp);
    if (s.ctx == nullptr) { std::fprintf(stderr, "failed to create the context\n"); return 1; }
    // llama.cpp clamps n_batch to the context; a decode call larger than n_batch aborts. Serve exactly --budget or refuse.
    if ((int) llama_n_batch(s.ctx) != a.budget) {
        std::fprintf(stderr, "--budget %d does not fit the context (n_batch %u); use --budget <= --ctx\n", a.budget,
                     llama_n_batch(s.ctx));
        return 1;
    }
    s.seq_used.assign(a.sessions, false);
    const std::string backend = backend_name();
    s.engine = "llamacpp@b11669-arc+" + backend + "+kv-" + a.kv_type + (a.flash_attn ? "+fa" : "") + tensor_tag(backend) +
               a.engine_suffix;
    if (!s.args.state_dir.empty()) {
#ifndef _WIN32
        umask(077);  // every saved file is created owner-only (0600)
#endif
        const std::string config = s.engine + " budget=" + std::to_string(a.budget) + " ubatch=" + std::to_string(a.ubatch);
        const fs::path root = s.args.state_dir;
        const fs::path dir = root / state_identity(a.model, config);
        std::string why;
        if (!trusted_dir(root, false, why)) {
            std::fprintf(stderr, "refusing --state-dir %s: %s\n", root.string().c_str(), why.c_str());
            return 1;
        }
        // Only the identity subdirectory is made owner-only: --state-dir itself may be a directory the user shares.
        if (!trusted_dir(dir, true, why)) {
            std::fprintf(stderr, "refusing %s: %s\n", dir.string().c_str(), why.c_str());
            return 1;
        }
        if (lock_dir(dir)) {
            s.args.state_dir = dir.string();
        } else {
            std::fprintf(stderr, "warning: another encoder is using %s; running without saved sessions\n",
                         dir.string().c_str());
            s.args.state_dir.clear();
        }
    }
    s.load_index();

    httplib::Server http;
    const int64_t n_vocab = llama_vocab_n_tokens(llama_model_get_vocab(s.model));
    // A request carries at most --ctx tokens and --ctx offsets, each a short decimal: cap the body near that, so a huge
    // array is refused before it is parsed.
    http.set_payload_max_length((size_t) llama_n_ctx(s.ctx) * 32 + 65536);
    // Only this machine's clients, by name: a Host other than the bound address (DNS rebinding) or any Origin (a web
    // page) is refused before routing.
    int port = 0;
    http.set_pre_routing_handler([&](const httplib::Request & req, httplib::Response & res) {
        const std::string host = req.get_header_value("Host");
        const std::string p = ":" + std::to_string(port);
        const bool local_host = host == "127.0.0.1" + p || host == "localhost" + p || host == "[::1]" + p ||
                                host == a.host + p;
        if (local_host && !req.has_header("Origin")) return httplib::Server::HandlerResponse::Unhandled;
        refuse(res, 403, "forbidden_origin");
        return httplib::Server::HandlerResponse::Handled;
    });

    http.Get("/health", [&](const httplib::Request &, httplib::Response & res) {
        std::lock_guard<std::mutex> lock(s.mu);
        json h = {{"engine", s.engine}, {"depth", llama_model_n_layer(s.model)}, {"budget", a.budget},
                  {"rope", "yarn-f4"}, {"max_model_len", (int) llama_n_ctx(s.ctx)}, {"dim", s.dim},
                  {"layer", "block_" + std::to_string(llama_model_n_layer(s.model) - 1)},
                  {"sessions", (int) s.sessions.size()},
                  // Optional flags this build accepts, so a supervisor passes only those.
                  {"capabilities", json::array({"checkpoint"})}, {"checkpoint", a.checkpoint}};
        res.set_content(h.dump(), "application/json");
    });

    http.Post("/encode", [&](const httplib::Request & req, httplib::Response & res) {
        if (!is_json(req)) return refuse(res, 415, "json_required");
        json body = json::parse(req.body, nullptr, false);
        if (body.is_discarded() || !body.is_object() || !body.contains("tokens") || !body.contains("offsets"))
            return refuse(res, 422, "invalid_body");
        if (body.contains("images") && !body["images"].empty()) return refuse(res, 422, "images_not_served");
        std::vector<llama_token> tokens;
        std::vector<int32_t> offsets;
        int32_t base = 0;
        std::string owner;
        bool retain = true;
        const int64_t n_ctx = llama_n_ctx(s.ctx);
        std::vector<int32_t> base_v;
        if (!int_array(body["tokens"], n_vocab - 1, tokens) || !int_array(body["offsets"], n_ctx, offsets) ||
            !int_array(json::array({body.value("base", json(0))}), n_ctx, base_v))
            return refuse(res, 422, "invalid_body");
        base = base_v[0];
        try {
            owner = body.value("owner", "");
            retain = body.value("retain", true);
        } catch (const std::exception &) {
            return refuse(res, 422, "invalid_body");
        }
        if (tokens.empty() || offsets.empty()) return refuse(res, 422, "invalid_body");
        const int64_t end = (int64_t) base + (int64_t) tokens.size();
        for (size_t i = 0; i < offsets.size(); ++i) {
            if (offsets[i] < base || offsets[i] >= end || (i && offsets[i] <= offsets[i - 1]))
                return refuse(res, 422, "offsets_out_of_range");
        }
        const bool retained = body.contains("session") && body["session"].is_string();
        if (retained && body["session"].get<std::string>().empty()) return refuse(res, 422, "invalid_body");
        if (!retained && retain) return refuse(res, 422, "invalid_retain");

        std::lock_guard<std::mutex> lock(s.mu);
        if (base + (int64_t) tokens.size() > (int64_t) llama_n_ctx(s.ctx)) {
            res.status = 422;
            res.set_content(json{{"error", "exceeds_kv_budget"}, {"tokens", base + (int) tokens.size()}}.dump(),
                            "application/json");
            return;
        }
        const auto t0 = std::chrono::steady_clock::now();
        llama_seq_id seq;
        Session * session = nullptr;
        Session scratch_session;  // an unretained encode's view of a restored prefix
        const std::string id = retained ? body["session"].get<std::string>() : "";
        // Refuse what cannot be served before changing anything: a refused request must leave every session as it was.
        if (!retained && base != 0) return refuse(res, 422, "invalid_retain");
        if (retained && base > 0) {
            const auto held_it = s.sessions.find(id);
            const bool held = held_it != s.sessions.end() && held_it->second.n_past == base;
            bool saved = false;
            for (const auto & e : s.entries) saved = saved || (e.session == id && (int32_t) e.tokens.size() == base);
            if (!held && !saved) return refuse(res, 409, "unknown_session");
        }
        // Free KV space first: a restore below needs it as much as a decode does.
        s.make_room(id, base + (int64_t) tokens.size());
        if (retained) {
            auto it = s.sessions.find(id);
            if (base == 0) {
                s.disown(id);
                if (it != s.sessions.end()) { s.drop(it->second); s.sessions.erase(it); }
                Session fresh;
                fresh.seq = s.take_seq();
                fresh.owner = owner;
                session = &(s.sessions[id] = fresh);
            } else if (it != s.sessions.end() && it->second.n_past == base) {
                session = &it->second;
            } else {
                // not held (or held at another length): resume a saved state of this session that ends at base
                // a copy: take_seq() may save an evicted session, which changes `entries`
                std::optional<Entry> saved;
                for (const auto & e : s.entries) if (e.session == id && (int32_t) e.tokens.size() == base) saved = e;
                if (!saved) return refuse(res, 409, "unknown_session");
                if (it != s.sessions.end()) { s.drop(it->second); s.sessions.erase(it); }
                Session fresh;
                fresh.seq = s.take_seq();
                fresh.owner = owner;
                if (!s.restore(*saved, fresh.seq, fresh, id)) { s.seq_used[fresh.seq] = false; return refuse(res, 409, "unknown_session"); }
                session = &(s.sessions[id] = std::move(fresh));
            }
            seq = session->seq;
        } else {
            if (base != 0) return refuse(res, 422, "invalid_retain");
            seq = s.scratch();
            llama_memory_seq_rm(llama_get_memory(s.ctx), seq, -1, -1);
            scratch_session.seq = seq;
        }
        Session & held = session != nullptr ? *session : scratch_session;

        // a full encode may start from a saved prefix
        int32_t start = base;
        if (base == 0) {
            if (const Entry * e = s.best_prefix(tokens, offsets)) {
                if (s.restore(*e, seq, held, id)) start = held.n_past;
            }
        }

        std::vector<float> out;
        out.reserve(offsets.size() * s.dim);
        std::vector<int32_t> fresh_offsets;
        for (int32_t off : offsets) {
            if (off < start) {
                const auto & row = held.rows.at(off);
                out.insert(out.end(), row.begin(), row.end());
            } else {
                fresh_offsets.push_back(off);
            }
        }
        // Decode `part` at positions from.. and return the rows at `offs` (none: no rows).
        auto decode = [&](int32_t from, const std::vector<llama_token> & part, const std::vector<int32_t> & offs) {
            if (!offs.empty()) return s.run(seq, from, part, offs);
            // llama.cpp needs at least one output per decode call chain; ask for the last token and discard it
            auto r = s.run(seq, from, part, {from + (int32_t) part.size() - 1});
            if (r) r->clear();
            return r;
        };
        std::optional<std::vector<float>> rows = std::vector<float>{};
        const int32_t ck = s.args.checkpoint;
        // Retained or not: either way the sequence holds exactly the first `ck` tokens when it is saved.
        const bool checkpoint = ck > 0 && base == 0 && start < ck &&
                                ck < (int32_t) tokens.size() && !s.has_checkpoint(tokens, ck);
        if (checkpoint) {
            // Decode up to the checkpoint, save the sequence there as its own entry, then decode the rest.
            const std::vector<llama_token> head(tokens.begin() + start, tokens.begin() + ck);
            const std::vector<llama_token> tail(tokens.begin() + ck, tokens.end());
            std::vector<int32_t> lo, hi;
            for (int32_t off : fresh_offsets) (off < ck ? lo : hi).push_back(off);
            auto first = decode(start, head, lo);
            if (first) {
                Session view;
                view.seq = seq;
                view.n_past = ck;
                view.tokens.assign(tokens.begin(), tokens.begin() + ck);
                for (const auto & [off, row] : held.rows) if (off < start) view.rows[off] = row;
                for (size_t i = 0; i < lo.size(); ++i)
                    view.rows[lo[i]] = std::vector<float>(first->begin() + (long) (i * s.dim), first->begin() + (long) ((i + 1) * s.dim));
                s.persist(Server::checkpoint_id(tokens, ck), view);
                auto second = decode(ck, tail, hi);
                if (second) first->insert(first->end(), second->begin(), second->end());
                rows = second ? first : std::nullopt;
            } else {
                rows = std::nullopt;
            }
        } else if (start < base + (int32_t) tokens.size()) {
            const std::vector<llama_token> rest(tokens.begin() + (start - base), tokens.end());
            rows = decode(start, rest, fresh_offsets);
        }
        if (!retained) llama_memory_seq_rm(llama_get_memory(s.ctx), seq, -1, -1);
        if (!rows) {
            if (session != nullptr) {
                s.drop(*session);
                s.sessions.erase(id);
            }
            return refuse(res, 503, "decode_failed");
        }
        out.insert(out.end(), rows->begin(), rows->end());
        if (session != nullptr) {
            session->tokens.resize(start);
            session->tokens.insert(session->tokens.end(), tokens.begin() + (start - base), tokens.end());
            for (size_t i = 0; i < fresh_offsets.size(); ++i)
                session->rows[fresh_offsets[i]] = std::vector<float>(rows->begin() + (long) (i * s.dim), rows->begin() + (long) ((i + 1) * s.dim));
            session->n_past = base + (int32_t) tokens.size();
            session->last_used = ++s.clock;
        }
        const double ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count();
        res.set_header("x-rows", std::to_string(offsets.size()));
        res.set_header("x-dim", std::to_string(s.dim));
        res.set_header("x-timing-ms", std::to_string(ms));
        res.set_header("x-restored-tokens", std::to_string(start - base));
        res.set_content(std::string(reinterpret_cast<const char *>(out.data()), out.size() * sizeof(float)),
                        "application/octet-stream");
    });

    http.Post("/close", [&](const httplib::Request & req, httplib::Response & res) {
        if (!is_json(req)) return refuse(res, 415, "json_required");
        json body = json::parse(req.body, nullptr, false);
        const std::string owner =
            body.is_object() && body.contains("owner") && body["owner"].is_string() ? body["owner"].get<std::string>() : "";
        std::lock_guard<std::mutex> lock(s.mu);
        int closed = 0;
        for (auto it = s.sessions.begin(); it != s.sessions.end();) {
            if (!owner.empty() && it->second.owner == owner) {
                s.persist(it->first, it->second);
                s.drop(it->second);
                it = s.sessions.erase(it);
                ++closed;
            }
            else ++it;
        }
        res.set_content(json{{"closed", closed}}.dump(), "application/json");
    });

    // --port 0 binds a free port; the bound port is the first stdout line, for a supervising parent.
    port = a.port == 0 ? http.bind_to_any_port(a.host) : (http.bind_to_port(a.host, a.port) ? a.port : -1);
    if (port <= 0) { std::fprintf(stderr, "cannot bind %s:%d\n", a.host.c_str(), a.port); return 1; }
    std::fprintf(stderr, "rayline-arc-encoder: %s dim=%d ctx=%u on %s:%d\n", s.engine.c_str(), s.dim,
                 llama_n_ctx(s.ctx), a.host.c_str(), port);
    std::printf("{\"listening\":%d}\n", port);
    std::fflush(stdout);
    g_http = &http;
    std::signal(SIGINT, on_signal);
    std::signal(SIGTERM, on_signal);
    if (a.parent_pipe) {
        // The parent holds stdin open; EOF means it is gone, so stop (saving sessions) instead of lingering.
        std::thread([] {
            char buf[64];
            while (std::fread(buf, 1, sizeof buf, stdin) > 0) {}
            if (g_http) g_http->stop();
        }).detach();
    }
    http.listen_after_bind();
    {
        std::lock_guard<std::mutex> lock(s.mu);
        for (auto & [id, session] : s.sessions) s.persist(id, session);
    }
    llama_free(s.ctx);
    llama_model_free(s.model);
    llama_backend_free();
    return 0;
}
