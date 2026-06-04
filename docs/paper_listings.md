# Paper Listings — Auto-Placement DAG & Slot-Based Data Movement

Two Overleaf-ready listings. Each fenced block below is a self-contained
`lstlisting` environment; paste it directly into a LaTeX document that loads the
`listings` package (`\usepackage{listings}`). A suggested preamble is given at
the end.

---

## Listing 1 — Declaring the DAG input (framework setup)

The user supplies a *machine-count-agnostic* symbolic DAG. Only the logical
nodes, their dependencies, and three placement directives are written by hand;
the framework synthesizes slot numbers, machine assignments, and all
cross-machine transfer edges.

```latex
\begin{lstlisting}[language=json,caption={Symbolic DAG input. Slot numbers, machine assignment, and inter-machine transfers are inferred by the framework.},label={lst:dag}]
{
  "shm_path_prefix": "/dev/shm/job",
  "total_nodes": 2,             // cluster size; topology is derived, not hard-coded
  "transfer": true,            // enable inter-machine RDMA / TCP
  "placement_policy": "pack",  // pack | balanced | spread | {type:weighted,...}

  "nodes": [
    { "id": "load",       "placement": "all",  "deps": [],
      "kind": { "Input":  { "path": "data/corpus.txt" } } },

    { "id": "distribute", "placement": "all",  "deps": ["load"],
      "kind": { "Func":   { "func": "distribute", "out_base": 50 } } },

    { "id": "map_n",      "placement": "all", "fanout": 8, "deps": ["distribute"],
      "kind": { "Func":   { "func": "map", "output_offset": 100 } } },

    { "id": "aggregate",  "placement": "all",  "deps": ["map_n"],
      "kind": { "Aggregate": {} } },

    { "id": "reduce",                          "deps": ["aggregate"],
      "kind": { "Func":   { "func": "reduce" } } },

    { "id": "save",                            "deps": ["reduce"],
      "kind": { "Output": { "path": "out/result.txt" } } }
  ]
}
\end{lstlisting}
```

**The only three placement directives an author writes:**

| Directive | Meaning | Framework expansion |
|---|---|---|
| `placement: "all"` | replicate per machine | `load` $\rightarrow$ `load_0, load_1, ...`, each pinned |
| `fanout: N` | N parallel copies, same machine | `map_n` $\rightarrow$ `map_n_i_{0..N-1}` |
| *(omitted)* | auto-placed | machine chosen by `placement_policy` + dep-affinity |

---

## Listing 2 — Data movement between functions (framework API only)

Functions never call each other and never pass arguments directly. They are
decoupled through **numbered shared-memory slots**: a function's sole argument
is *which slot to read*; it appends results to an output slot. The runtime's
`Aggregate` and `RemoteSend`/`RemoteRecv` nodes route slots between functions
and across machines. Workload logic is elided — only the framework data-movement
calls are shown.

```latex
\begin{lstlisting}[language=Rust,caption={Slot-based data movement. Each function reads an input slot and appends to an output slot; the framework wires the slots together.},label={lst:flow}]
// Slot channels are assigned by the framework, not the function author:
//   IO 0          raw input            (host Input node fills it)
//   stream 50..   per-worker shards     (distribute, out_base = 50)
//   stream 150..  per-worker results    (map, input slot + offset 100)
//   stream 200    merged result         (host Aggregate node)
//   IO 1          final output          (host Output node drains it)

// STAGE 1 -- scatter input records across N worker slots.
#[no_mangle]
pub extern "C" fn distribute(n_workers: u32) {
    let mut i = 0;
    ShmApi::for_each_input(|_origin, record| {       // <- read IO slot 0
        let slot = 50 + (i % n_workers);
        ShmApi::append_stream_data(slot, record);     // -> write stream slot 50+k
        i += 1;
    });
}

// STAGE 2 -- map: consume one shard, emit per-shard results.
#[no_mangle]
pub extern "C" fn map(slot: u32) {                    // arg = assigned input slot
    let records = ShmApi::read_all_stream_records(slot);          // <- read slot k
    /* ... workload logic produces `output` bytes ... */
    ShmApi::append_stream_data(slot + 100, output);              // -> write slot k+100
}

// STAGE 3 -- reduce: consume the aggregated slot, write final output.
#[no_mangle]
pub extern "C" fn reduce(stream_slot: u32) {          // arg = aggregated slot (200)
    let records = ShmApi::read_all_stream_records(stream_slot);   // <- read slot 200
    /* ... workload logic produces `result` ... */
    ShmApi::write_output_str(&result);                          // -> write IO slot 1
}
\end{lstlisting}
```

**Data-flow (always *slot to slot*, never a function call):**

```
 Input --IO0--> distribute --str50..--> map x N --str150..--> Aggregate --str200--> reduce --IO1--> Output
```

When `map` and `reduce` land on different machines, the framework transparently
replaces the `str150.. -> 200` edge with an injected `RemoteSend`/`RemoteRecv`
pair. The function bodies are **never modified** for distribution — the slot
indirection is what makes placement automatic.

---

## Listing 3 — Declaring a streaming pipeline (framework setup)

Streaming workloads use a single `StreamPipeline` node instead of a chain of
one-shot `Func` nodes. The framework drives the listed stages in an *overlapping
wave schedule*: for `R` rounds and `D` stages the total ticks are `R + D - 1`
(adjacent rounds overlap) rather than `R * D`. Each stage consumes only the
records that arrived since its previous invocation, using a per-stage SHM cursor.
Adding `rdma_recv` / `rdma_send` turns the pipeline into a cross-machine stream
over a persistent RDMA connection reused across all rounds.

```latex
\begin{lstlisting}[language=json,caption={Streaming pipeline input. One node drives all stages in a pipelined wave schedule; an optional RDMA sink streams each round's output to a remote peer.},label={lst:stream-dag}]
{
  "shm_path_prefix": "/dev/shm/stream",
  "total_nodes": 2,
  "transfer": true,

  "nodes": [
    { "id": "pipeline", "node_id": 0, "deps": [],
      "kind": { "StreamPipeline": {
        "rounds": 64,
        "stages": [
          { "func": "source",    "arg0": 200, "arg1": null },  // null -> inject round #
          { "func": "filter",    "arg0": 200, "arg1": 201 },
          { "func": "transform", "arg0": 201, "arg1": 202 },
          { "func": "sink",      "arg0": 202, "arg1": 203 }
        ],
        "rdma_send": { "peer": 1, "slot": 203, "slot_kind": "Stream",
                       "free_after": true }  // stream each round's result to node 1
      }}
    },

    { "id": "consumer", "node_id": 1, "deps": [],
      "kind": { "StreamPipeline": {
        "rounds": 64,
        "stages": [
          { "func": "sink", "arg0": 300, "arg1": 301 }
        ],
        "rdma_recv": { "peer": 0, "slot": 300, "slot_kind": "Stream" }  // receive each round
      }}
    }
  ]
}
\end{lstlisting}
```

**Streaming directives the framework interprets:**

| Field | Meaning |
|---|---|
| `rounds` | number of streaming rounds (batches) to drive |
| `stages[].arg0` | input slot the stage reads new records from |
| `stages[].arg1` | output slot (`null` $\rightarrow$ framework injects the round number) |
| `rdma_send` / `rdma_recv` | stream a slot to/from a peer each round over the persistent mesh |
| `free_after` | reclaim the sent slot after each round so it never accumulates |

---

## Listing 4 — Streaming data movement between functions (framework API only)

A stream slot is an **append-only log**; consumers track a personal *cursor* (a
named SHM atomic) so each invocation sees only records appended since last time.
This windowing over the shared log — not message passing — is what moves data
between streaming stages. Workload logic is elided.

```latex
\begin{lstlisting}[language=Rust,caption={Cursor-windowed streaming. Each stage reads only newly-arrived records from its input slot and appends results to its output slot; the per-stage cursor lives in shared memory so it survives across rounds.},label={lst:stream-flow}]
// STAGE 0 -- source: append a fresh batch to `out_slot` each round.
#[no_mangle]
pub extern "C" fn source(out_slot: u32, round: u32) {   // arg1=null -> round injected
    /* ... workload produces `record` bytes ... */
    ShmApi::append_stream_data(out_slot, record);         // -> append to stream slot
}

// STAGE k -- transform: consume only records that arrived since the last round.
#[no_mangle]
pub extern "C" fn transform(in_slot: u32, out_slot: u32) {
    let cursor = ShmApi::get_named_atomic("transform_cursor");   // per-stage SHM cursor
    let start  = cursor.load(Ordering::Acquire) as usize;
    let all    = ShmApi::read_all_stream_records(in_slot);       // <- the append-only log
    let fresh  = &all[start.min(all.len())..];                   // window of new records

    for (_origin, record) in fresh {
        /* ... workload produces `output` bytes ... */
        ShmApi::append_stream_data(out_slot, output);            // -> append downstream
    }
    cursor.store((start + fresh.len()) as u64, Ordering::Release); // advance cursor
}
\end{lstlisting}
```

**Streaming data-flow (per round; the log persists, the cursor advances):**

```
 source --append-->[ slot 200 log ]--window--> filter --append-->[ slot 201 log ]--window--> transform --> ... --rdma_send--> peer
```

Across machines, `rdma_send`/`rdma_recv` carry each round's output slot to the
peer over the persistent RDMA connection; the stage code is identical whether its
input slot is filled locally or by a remote receive.

---

## Suggested LaTeX preamble

```latex
\usepackage{listings}

% Define clean colors for the paper
\definecolor{keywordcolor}{HTML}{000000}   % Pure Black
\definecolor{functioncolor}{HTML}{0055AA}  % Strong Blue Accent
\definecolor{variablecolor}{HTML}{444444}  % Dark Grey
\definecolor{commentcolor}{HTML}{999999}   % Light Grey

\lstdefinelanguage{Rust}{
  keywords={fn, let, u32},
  keywordstyle=\color{keywordcolor}\bfseries,
  comment=[l]{//},
  morecomment=[s]{/*}{*/},
  commentstyle=\color{commentcolor}\itshape,
  emph={distribute, map, reduce, for_each_input, append_stream_data, read_all_stream_records, write_output_str},
  emphstyle=\color{functioncolor},
  emph={[2]n_workers, _origin, record, target_slot, slot, records, output_slot, output, stream_slot, result},
  emphstyle={[2]\color{variablecolor}},
  sensitive=true,
}

\lstset{
  basicstyle=\ttfamily\footnotesize,
  columns=fullflexible,
  keepspaces=true,
  showstringspaces=false,
  breaklines=true,
  captionpos=b,
  numbers=none,
  frame=none,
  xleftmargin=1em,
  xrightmargin=1em
}

% RTDSRed to avoid clashing with \partial, \no, \yes, etc.,
% and wrapped in \ensuremath so they work in tabular cells.
\newcommand{\fyes}{\ensuremath{\CIRCLE}}       % full support
\newcommand{\fpart}{\ensuremath{\LEFTcircle}}  % partial support
\newcommand{\fno}{\ensuremath{\Circle}}        % no support

\newcommand{\todo}[1]{\textcolor{red}{\textbf{[TODO: #1]}}}
\newcommand{\WasMem}[0]{WasMem}
\newcommand{\tocite}[0]{\textcolor{yellow}{\textbf{cite here}}}
\definecolor{added}{RGB}{0, 100, 150}
\newcommand{\added}[1]{\textcolor{added}{#1}}
\definecolor{revised}{RGB}{180, 50, 50}
\newcommand{\revised}[1]{{\color{revised}#1}}

\AtBeginDocument{%
  \providecommand\BibTeX{{%
    \normalfont B\kern-0.5em{\scshape i\kern-0.25em b}\kern-0.8em\TeX}}}
```
