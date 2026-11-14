// rayline-arc-encoder: a loopback HTTP server that runs the ARC encoder (a GGUF model) on llama.cpp and returns
// its final-layer state at requested token offsets. Rayline's local routing starts it and reads those rows.
//
// Protocol (JSON requests; every response but a successful /encode is JSON):
//
//   GET  /health  -> {"engine","depth","budget","rope","max_model_len","layer","dim","sessions"}
//   POST /encode  {"tokens": [id...], "offsets": [abs...], "base": N,
//                  "session": "id" | "retain": false, ["owner": "id"], ["images": [...]]}
//                 `tokens` are the token ids at positions base..; `offsets` are absolute positions in
//                 [base, base + len(tokens)), strictly ascending. With "session", the tokens are kept as a retained
//                 session: base 0 starts (or replaces) it, base N appends to a session that holds exactly N tokens.
//                 With "retain": false (base 0 only), nothing is kept.
//                 -> 200 raw little-endian float32 rows, one per offset (headers x-rows, x-dim, x-timing-ms);
//                    409 {"error":"unknown_session"}: an append to a session not held at that base;
//                    422 {"error": code}: invalid_body, offsets_out_of_range, invalid_retain, images_not_served,
//                    exceeds_kv_budget (input longer than --ctx);
//                    503 {"error":"decode_failed"}.
//   POST /close   {"owner"} -> {"closed": n}: drop that owner's sessions.
//
// The server has no authentication, so it binds only loopback (--host 127.0.0.1, ::1 or localhost). A POST without
// Content-Type: application/json gets 415 (no CORS preflight is ever answered), and a request with an Origin or a Host
// other than the bound loopback address gets 403 (DNS rebinding). /health never waits for an encode in progress.
//
// With --port 0 the server binds a free port and prints {"listening": PORT} as its first stdout line. With
// --parent-pipe it stops when its stdin closes, so it never outlives the process that started it.
//
// Readouts are llama.cpp's per-token embeddings with pooling NONE: the final norm's output, read at each requested
// absolute offset. A retained session is one llama sequence id holding both the full-attention KV and the
// GatedDeltaNet recurrent state, so an append continues it; an unretained encode uses a scratch sequence that is
// cleared afterwards. Sessions share one KV cache of --ctx tokens: a request that would not fit beside the sessions
// held evicts the least recently used ones first. Text only: images are refused (images_not_served).

#ifdef _WIN32
#define NOMINMAX             // before any header: httplib.h includes windows.h
#define WIN32_LEAN_AND_MEAN
#endif

#include "llama.h"
#include "ggml-backend.h"
#include "httplib.h"
#include "nlohmann/json.hpp"

#include <cctype>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <atomic>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <vector>

using json = nlohmann::json;

namespace {

struct Session {
    llama_seq_id seq = -1;
    int32_t n_past = 0;          // tokens held: the next append's base
    std::string owner;
    uint64_t last_used = 0;
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
    bool parent_pipe = false;    // exit when stdin closes (a supervising parent went away)
    int threads = 0;             // CPU threads; 0: libllama's default (4)
    bool tensor = true;          // false: never use Metal tensor-API kernels (GGML_METAL_TENSOR_DISABLE)
};

struct Server {
    Args args;
    llama_model * model = nullptr;
    llama_context * ctx = nullptr;
    int dim = 0;
    std::mutex mu;
    std::map<std::string, Session> sessions;
    std::atomic<int> held{0};  // sessions.size() as of the last request, for /health without the lock
    std::vector<bool> seq_used;
    uint64_t clock = 0;
    std::string engine;

    llama_seq_id scratch() const { return args.sessions; }

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

void refuse(httplib::Response & res, int status, const std::string & code) {
    res.status = status;
    res.set_content(json{{"error", code}}.dump(), "application/json");
}

// A supervisor reads this (`--help`) to learn which optional flags the build takes.
void usage() {
    std::fprintf(stderr, "usage: rayline-arc-encoder --model F.gguf [--port P] [--ctx N] [--budget N] "
                         "[--ubatch N] [--sessions N] [--kv-type f16|bf16|f32] [--no-flash-attn] [--no-tensor]\n"
                         "       [--threads N] [--host H (loopback only)] [--parent-pipe]\n");
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
        else if (k == "--no-tensor") a.tensor = false;
        else if (k == "--threads") a.threads = std::atoi(val().c_str());
        else if (k == "--parent-pipe") a.parent_pipe = true;
        else if (k == "--help") { usage(); return false; }
        else { std::fprintf(stderr, "unknown argument %s\n", k.c_str()); usage(); return false; }
    }
    // No authentication: the encoder serves only this machine.
    if (a.host != "127.0.0.1" && a.host != "::1" && a.host != "localhost") {
        std::fprintf(stderr, "--host must be a loopback address (127.0.0.1, ::1 or localhost)\n");
        return false;
    }
    if (a.model.empty() || a.budget <= 0 || a.sessions <= 0 || a.n_ctx <= 0 || a.budget > a.n_ctx) {
        usage();
        return false;
    }
    return true;
}

void set_env(const char * name, const char * value) {
#ifdef _WIN32
    _putenv_s(name, value);
#else
    setenv(name, value, 1);
#endif
}

// Publishes the session count for /health when a request handler leaves, still under the lock.
struct PublishCount {
    Server & s;
    ~PublishCount() { s.held.store((int) s.sessions.size()); }
};

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
    s.engine = "llamacpp@b11669-arc+" + backend_name() + "+kv-" + a.kv_type + (a.flash_attn ? "+fa" : "") + a.engine_suffix;
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

    // /health never waits for an encode in progress (that can take minutes at long context): everything but the
    // session count is fixed at start, and the count is published after each request.
    const json health = {{"engine", s.engine}, {"depth", llama_model_n_layer(s.model)}, {"budget", a.budget},
                         {"rope", "yarn-f4"}, {"max_model_len", (int) llama_n_ctx(s.ctx)}, {"dim", s.dim},
                         {"layer", "block_" + std::to_string(llama_model_n_layer(s.model) - 1)}};
    http.Get("/health", [&](const httplib::Request &, httplib::Response & res) {
        json h = health;
        h["sessions"] = s.held.load();
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
        const PublishCount publish{s};
        if (base + (int64_t) tokens.size() > (int64_t) llama_n_ctx(s.ctx)) {
            res.status = 422;
            res.set_content(json{{"error", "exceeds_kv_budget"}, {"tokens", base + (int) tokens.size()}}.dump(),
                            "application/json");
            return;
        }
        // Refuse what cannot be served before changing anything: a refused request must leave every session as it was.
        const std::string id = retained ? body["session"].get<std::string>() : "";
        if (retained && base > 0) {
            const auto it = s.sessions.find(id);
            if (it == s.sessions.end() || it->second.n_past != base) return refuse(res, 409, "unknown_session");
        }
        const auto t0 = std::chrono::steady_clock::now();
        s.make_room(id, base + (int64_t) tokens.size());
        llama_seq_id seq;
        Session * session = nullptr;
        if (retained) {
            if (base == 0) {
                auto it = s.sessions.find(id);
                if (it != s.sessions.end()) { s.drop(it->second); s.sessions.erase(it); }
                Session fresh;
                fresh.seq = s.take_seq();
                fresh.owner = owner;
                session = &(s.sessions[id] = fresh);
            } else {
                session = &s.sessions.at(id);  // held at base: checked above, and make_room keeps `id`
            }
            seq = session->seq;
        } else {
            seq = s.scratch();
            llama_memory_seq_rm(llama_get_memory(s.ctx), seq, -1, -1);
        }

        auto rows = s.run(seq, base, tokens, offsets);
        if (!retained) llama_memory_seq_rm(llama_get_memory(s.ctx), seq, -1, -1);
        if (!rows) {
            if (session != nullptr) {
                s.drop(*session);
                s.sessions.erase(id);
            }
            return refuse(res, 503, "decode_failed");
        }
        if (session != nullptr) {
            session->n_past = base + (int32_t) tokens.size();
            session->last_used = ++s.clock;
        }
        const double ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count();
        res.set_header("x-rows", std::to_string(offsets.size()));
        res.set_header("x-dim", std::to_string(s.dim));
        res.set_header("x-timing-ms", std::to_string(ms));
        res.set_content(std::string(reinterpret_cast<const char *>(rows->data()), rows->size() * sizeof(float)),
                        "application/octet-stream");
    });

    http.Post("/close", [&](const httplib::Request & req, httplib::Response & res) {
        if (!is_json(req)) return refuse(res, 415, "json_required");
        json body = json::parse(req.body, nullptr, false);
        const std::string owner =
            body.is_object() && body.contains("owner") && body["owner"].is_string() ? body["owner"].get<std::string>() : "";
        std::lock_guard<std::mutex> lock(s.mu);
        const PublishCount publish{s};
        int closed = 0;
        for (auto it = s.sessions.begin(); it != s.sessions.end();) {
            if (!owner.empty() && it->second.owner == owner) {
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
        // The parent holds stdin open; EOF means it is gone, so stop instead of lingering.
        std::thread([] {
            char buf[64];
            while (std::fread(buf, 1, sizeof buf, stdin) > 0) {}
            if (g_http) g_http->stop();
        }).detach();
    }
    http.listen_after_bind();
    llama_free(s.ctx);
    llama_model_free(s.model);
    llama_backend_free();
    return 0;
}
