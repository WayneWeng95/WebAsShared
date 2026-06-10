# Faasm WordCount port

Map-reduce WordCount as a Faaslet (WASM) function — the Faasm baseline for
[`../../README.md`](../../README.md). **This is a new port** (Faasm shipped no WordCount).

## Files (canonical copies; installed into the Faasm tree)

| File | Installed to (in `../../../../../../compare_system/faasm/`) |
|------|------------------------------------------------------------|
| `wordcount.cpp`   | `func/wordcount/wordcount.cpp` |
| `CMakeLists.txt`  | `func/wordcount/CMakeLists.txt` |
| —                 | wired via `add_subdirectory(wordcount)` in `func/CMakeLists.txt` |

One WASM module, three funcs selected by idx (Faasm's `FAASM_FUNC(name, idx)`):

| idx | func | role |
|-----|------|------|
| 0 | `faasmMain` | splitter/driver: read `corpus` state → N newline-aligned `chunk_<i>` → chain N mappers → await → chain reducer |
| 1 | `wc_mapper` | read `chunk_<i>`, count words, write `partial_<i>` |
| 2 | `wc_reducer`| merge `partial_0..partial_{N-1}` → `result` |

Fan-out is `faasmChainThisInput`; state moves through the Faasm KV
(`faasmReadState`/`faasmWriteState`). The serialized `partial_<i>` blobs are the
inter-stage transfer we measure against our zero-copy path.

## Build

```bash
cd /opt/myapp/compare_system/faasm
# inside the Faasm toolchain container / env (see faasm docs/getting_started)
inv compile wordcount wordcount            # FAASM_USER=wordcount, func=wordcount
#   → wasm at wasm/wordcount/wordcount/function.wasm
```

The pure word-counting/serialization logic is also covered by a standalone host
test (`g++ -std=c++17`) — run during development to validate tokenization and
the partial→merge round-trip without the toolchain.

## Run

```bash
# 1. upload the corpus as Faasm state under user=wordcount, key=corpus
inv state.set wordcount corpus --path /path/to/corpus.txt      # (or the driver/ upload pattern)
# 2. invoke; the string input = number of mappers N
inv invoke wordcount wordcount --input 8
#   driver prints: "WordCount done: 8 mappers + reduce over <bytes> bytes"
#   reducer prints: "WordCount reduce: <unique> unique words across 8 partials"
#   final counts in state key "result"
```

> Use the **same corpus and the same N** as the other three systems. Metric:
> Faasm's billable memory + per-function exec time; the `partial_<i>` writes are
> the serialized state-transfer cost (contrast: ours = 0).
