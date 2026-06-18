// ─────────────────────────────────────────────────────────────────────────────
// WordCount — map-reduce on Faasm (Faaslet/WASM)                  user: "wordcount"
//
// Mirrors the splitter → mapper×N → reducer shape of the RMMap and WebAsShared
// WordCount workloads so the cross-system comparison is apples-to-apples.
//
// One WASM module, three Faasm funcs (selected by idx):
//   idx 0  faasmMain (driver/splitter) : read `corpus` state, split into N
//          contiguous, newline-aligned chunks → `chunk_<i>` state, chain N
//          mappers, await them, then chain the reducer.
//   idx 1  wc_mapper                    : read `chunk_<i>`, count words, write
//          `partial_<i>` state (serialized "word\x1fcount\n").
//   idx 2  wc_reducer                   : merge `partial_0..partial_{N-1}` →
//          `result` state.
//
// Faasm state keys (user "wordcount"):
//   corpus       input text  (upload once before running — see README)
//   chunk_<i>    i-th slice of the corpus (written by the splitter)
//   partial_<i>  mapper i output                         (the serialized state
//   result       final merged counts                     transfer we measure)
//
// The `partial_<i>` blobs are the inter-stage state that Faasm serializes into
// its KV; that serialize/deserialize cost is exactly what WebAsShared's
// zero-copy page-chain and RMMap-DMERGE's RDMA path avoid.
// ─────────────────────────────────────────────────────────────────────────────
#include <faasm/faasm.h>
#include <faasm/input.h>

#include <cctype>
#include <cstdint>
#include <map>
#include <stdio.h>
#include <string>
#include <vector>

static const char SEP = '\x1f'; // unit separator between word and count

// Serialize word→count as "word\x1fcount\n…" (one record per line).
static std::string serialize(const std::map<std::string, long>& m)
{
    std::string out;
    for (const auto& kv : m) {
        out += kv.first;
        out += SEP;
        out += std::to_string(kv.second);
        out += '\n';
    }
    return out;
}

// Merge a serialized partial back into `m`.
static void deserialize_into(const std::string& s, std::map<std::string, long>& m)
{
    size_t i = 0;
    while (i < s.size()) {
        size_t sep = s.find(SEP, i);
        if (sep == std::string::npos)
            break;
        size_t nl = s.find('\n', sep);
        if (nl == std::string::npos)
            break;
        m[s.substr(i, sep - i)] += std::stol(s.substr(sep + 1, nl - sep - 1));
        i = nl + 1;
    }
}

// Tokenize on non-alpha, lowercase — same tokenization as wc_map in
// Executor/guest/src/workloads/word_count.rs and the RMMap mapper.
static void count_words(const char* data, size_t len,
                        std::map<std::string, long>& counts)
{
    std::string word;
    for (size_t i = 0; i < len; i++) {
        unsigned char c = (unsigned char)data[i];
        if (std::isalpha(c)) {
            word += (char)std::tolower(c);
        } else if (!word.empty()) {
            counts[word]++;
            word.clear();
        }
    }
    if (!word.empty())
        counts[word]++;
}

// ── Mapper ──────────────────────────────────────────────────────────────────
FAASM_FUNC(wc_mapper, 1)
{
    int idx = 0;
    faasmGetInput((uint8_t*)&idx, sizeof(int));

    std::string chunkKey = "chunk_" + std::to_string(idx);
    size_t len = faasmReadStateSize(chunkKey.c_str());
    std::vector<uint8_t> buf(len);
    faasmReadState(chunkKey.c_str(), buf.data(), (long)len);

    std::map<std::string, long> counts;
    count_words((const char*)buf.data(), len, counts);

    std::string out = serialize(counts);
    std::string partKey = "partial_" + std::to_string(idx);
    faasmWriteState(partKey.c_str(), (const uint8_t*)out.data(), (long)out.size());
    return 0;
}

// ── Reducer ─────────────────────────────────────────────────────────────────
FAASM_FUNC(wc_reducer, 2)
{
    int n = 0;
    faasmGetInput((uint8_t*)&n, sizeof(int));

    std::map<std::string, long> total;
    for (int i = 0; i < n; i++) {
        std::string partKey = "partial_" + std::to_string(i);
        size_t len = faasmReadStateSize(partKey.c_str());
        std::vector<uint8_t> buf(len);
        faasmReadState(partKey.c_str(), buf.data(), (long)len);
        deserialize_into(std::string((const char*)buf.data(), len), total);
    }

    std::string out = serialize(total);
    faasmWriteState("result", (const uint8_t*)out.data(), (long)out.size());
    printf("WordCount reduce: %zu unique words across %d partials\n",
           total.size(), n);
    return 0;
}

// ── Driver / splitter ─────────────────────────────────────────────────────────
FAASM_MAIN_FUNC()
{
    // Number of mappers from the string input (default 4).
    std::string in = faasm::getStringInput("4");
    int n = in.empty() ? 4 : std::stoi(in);
    if (n < 1)
        n = 1;

    size_t corpusLen = faasmReadStateSize("corpus");
    if (corpusLen == 0) {
        printf("ERROR: state 'corpus' is empty — upload the input first\n");
        return 1;
    }
    std::vector<uint8_t> corpus(corpusLen);
    faasmReadState("corpus", corpus.data(), (long)corpusLen);

    // Split into N contiguous, newline-aligned chunks and fan out a mapper each.
    size_t approx = corpusLen / (size_t)n;
    size_t start = 0;
    std::vector<unsigned int> callIds;
    for (int i = 0; i < n; i++) {
        size_t end;
        if (i == n - 1) {
            end = corpusLen;
        } else {
            end = start + approx;
            while (end < corpusLen && corpus[end] != '\n')
                end++;
            if (end < corpusLen)
                end++; // keep the newline with the chunk
        }
        std::string chunkKey = "chunk_" + std::to_string(i);
        faasmWriteState(chunkKey.c_str(), corpus.data() + start, (long)(end - start));

        int idx = i;
        callIds.push_back(faasmChainThisInput(1, (uint8_t*)&idx, sizeof(int)));
        start = end;
    }

    for (unsigned int cid : callIds)
        faasmAwaitCall(cid);

    // Reduce stage.
    unsigned int rid = faasmChainThisInput(2, (uint8_t*)&n, sizeof(int));
    faasmAwaitCall(rid);

    printf("WordCount done: %d mappers + reduce over %zu bytes\n", n, corpusLen);
    return 0;
}
