% systemoverview.md
% LaTeX source describing the WebAssembly stream-processing framework, organised
% as component views for use in a scientific paper. Drop the sections below into
% the paper body. Requires (typical) packages: amsmath, booktabs, hyperref,
% listings or minted for code. Replace \texttt{} with \lstinline if preferred.

\section{System Overview}
\label{sec:system-overview}

The framework executes data-processing pipelines expressed as JSON
\emph{directed acyclic graphs} (DAGs) over a cluster of nodes connected by
RDMA. Each DAG node is one of: a WebAssembly (WASM) function call, a Python
function call, a host-side zero-copy routing operation, a file I/O step, or an
RDMA transfer. A single shared-memory (SHM) substrate per node carries all
intermediate data as linked \emph{page chains}; data is moved between operators
by re-linking page chains (O(1) splices) rather than copying, and between nodes
by one-sided RDMA writes into the peer's SHM.

The system is organised into six component views: the
\emph{control plane} (\S\ref{sec:control-plane}) that schedules and supervises
jobs; the \emph{partitioner} (\S\ref{sec:partitioner}) that lowers a logical DAG
to a placed, sliced, per-node physical DAG; the \emph{execution engine}
(\S\ref{sec:engine}) that runs a per-node DAG; the \emph{shared-memory
substrate} (\S\ref{sec:shm}) that backs all data movement; the \emph{RDMA
transport} (\S\ref{sec:rdma}) for cross-node data exchange; and the
\emph{guest workloads} (\S\ref{sec:guest}) compiled to WASM.

\paragraph{End-to-end flow.}
A user submits a slot-free \texttt{SymbolicDag}. The coordinator runs the
partitioner against live cluster state to produce a \texttt{ClusterDag}
(per-node node lists, all slot numbers assigned, cross-node edges materialised
as \texttt{RemoteSend}/\texttt{RemoteRecv} pairs). A mode transform lowers
unified \texttt{Func} nodes to native kinds (\texttt{WasmVoid} for Rust,
\texttt{PyFunc} for Python). The coordinator stages shared input files to every
node, ships each node its sub-DAG, and launches one \emph{executor} process per
node. Each executor topologically orders its nodes, groups independent nodes
into parallel \emph{waves}, and runs them, exchanging slot data across nodes via
RDMA between waves.

\begin{figure}[t]
\centering
\begin{verbatim}
  SymbolicDag --[partitioner]--> ClusterDag --[transform]--> per-node DAGs
        |                                                          |
   (placement, fanout                                        (one executor
    apportionment, core cap,                                  process / node)
    data-parallel slicing,                                         |
    RemoteSend/Recv injection)                              topo sort -> waves
                                                                    |
                                                  WASM workers | routing | RDMA
                                                                    |
                                                            SHM page chains
\end{verbatim}
\caption{Compilation and execution pipeline.}
\label{fig:flow}
\end{figure}


\section{Control Plane (NodeAgent)}
\label{sec:control-plane}

The control plane is a single binary (\texttt{node-agent}) operating in three
modes: \texttt{run} (single-node), \texttt{start} (coordinator/worker daemon),
and \texttt{submit} (client). It owns cluster membership, job distribution,
input staging, and metrics; it does \emph{not} touch data --- all bulk data
lives in SHM and moves by RDMA.

\begin{description}
  \item[\texttt{coordinator.rs} --- job orchestration.] Accepts worker
    connections, snapshots live membership and capacity, invokes the partitioner
    in-process (\texttt{resolve\_dag}), applies the mode transform, stages shared
    inputs, distributes per-node DAGs, launches the node-0 executor, and
    aggregates per-node results. It maps the partitioner's dense logical node
    space $0..N$ onto the arbitrary set of physically live nodes.
  \item[\texttt{worker.rs} --- node agent.] Connects to the coordinator,
    receives staged files (\texttt{StageFiles}) and a per-node DAG
    (\texttt{AssignJob}), spawns the local executor, and reports
    \texttt{JobStarted}/\texttt{JobCompleted}/\texttt{JobFailed} plus periodic
    metrics. Staging is timed separately so it can be excluded from compute.
  \item[\texttt{executor.rs} --- process supervision.]
    \texttt{ExecutorHandle::spawn} launches \texttt{host dag <file>}, capturing
    or forwarding its output; \texttt{try\_wait}/\texttt{wait}/\texttt{pid}/
    \texttt{kill} drive non-blocking monitoring and metric sampling of the
    executor process.
  \item[\texttt{file\_staging.rs} --- input replication.] Replicates the input
    file(s) declared by \texttt{placement:"all"} \texttt{Input} nodes to every
    node before the job runs (the data-parallel slicing in \S\ref{sec:partitioner}
    then makes each node process only its shard).
  \item[\texttt{protocol.rs} --- control channel.] Length-prefixed JSON messages
    over per-peer TCP connections (\texttt{Ready}, \texttt{SubmitJob},
    \texttt{AssignJob}, \texttt{StageFiles}, \texttt{Metrics}, \texttt{Ping},
    \texttt{JobCompleted}, $\ldots$).
  \item[\texttt{dag\_transform.rs} --- mode lowering.] Rewrites unified
    \texttt{Func}/\texttt{Pipeline}/\texttt{Grouping} kinds into native
    \texttt{WasmVoid}/\texttt{StreamPipeline}/\texttt{WasmGrouping} (Rust) or
    \texttt{PyFunc}/\texttt{PyPipeline} (Python) per the submit mode, leaving
    \texttt{Input}/\texttt{Output}/routing/RDMA kinds untouched.
  \item[\texttt{cluster\_dag.rs} --- per-node split.] Splits a hand-authored
    \texttt{ClusterDag} into per-node DAGs and injects the live RDMA mesh
    configuration.
  \item[\texttt{metrics.rs} --- observability.] Samples CPU (\texttt{/proc/stat}),
    memory, and SHM utilisation each interval and logs JSON lines. Memory
    (\texttt{rss\_bytes}) is the summed \emph{private} resident memory
    ($\mathit{VmRSS}-\mathit{RssShmem}$) of the whole executor process tree
    (host plus every fanned-out \texttt{wasm-call} worker), with the shared SHM
    excluded per process and reported once via \texttt{shm\_bump\_offset}, so
    $\text{total} = \texttt{rss\_bytes} + \texttt{shm\_bump\_offset}$ without
    double counting.
\end{description}

\subsection{Scheduling Advisor (\texttt{scheduler} crate)}
Integrates Linux \texttt{sched\_ext} (SCX) and basic \texttt{/proc} metrics into
a cluster-wide view used for placement.
\begin{description}
  \item[\texttt{scx\_cluster.rs}] Maintains \texttt{ScxClusterView}: per-node
    snapshots (CPU busy, cores, RSS, running job) updated from worker metrics.
  \item[\texttt{advisor.rs}] Derives placement hints:
    \texttt{cluster\_capacity} (normalised per-host weights from load),
    \texttt{host\_limits} ($\lfloor \text{cores}\times(1-\text{busy})\rfloor$,
    the per-host sandbox cap), and \texttt{host\_cores} (raw per-host core count,
    used to bound fan-out).
\end{description}


\section{Partitioner}
\label{sec:partitioner}

The partitioner lowers a logical \texttt{SymbolicDag} (no slot numbers, node
placement optional) into a physical \texttt{ClusterDag}. It runs in the
coordinator so it can use live cluster size and load. Its duties are placement,
fan-out apportionment, fan-out capping, data-parallel input slicing, slot
assignment, and cross-node edge materialisation.

\begin{description}
  \item[\texttt{symbolic\_dag.rs} --- input model.] Defines \texttt{SymbolicNode}
    (\texttt{id}, \texttt{deps}, optional \texttt{node\_id}, \texttt{fanout},
    \texttt{placement}, raw \texttt{kind}) and the \texttt{SymbolicDag}
    (\texttt{placement\_policy}, \texttt{total\_nodes}, $\ldots$).
  \item[\texttt{splitter.rs} --- the lowering driver.] Orchestrates the whole
    pass via \texttt{partition()}:
    \begin{itemize}
      \item \texttt{expand\_all\_placements}: replicates a
        \texttt{placement:"all"} node to one pinned copy per machine and
        auto-derives \texttt{shared\_inputs} from \texttt{Input} nodes.
      \item \texttt{apportion\_fanout}: splits a node's total \texttt{fanout}
        across machines in proportion to per-host capacity using the
        largest-remainder (Hamilton) method, so the parts sum exactly to $N$.
      \item \texttt{cap\_fanout}: clamps the requested fan-out to the cluster
        core budget $\sum_h \max(1,\text{cores}_h-\textit{reserve})$ before
        apportionment, reserving cores per host for the control plane.
      \item \texttt{expand\_fanout}: materialises each (already per-machine)
        fan-out template into its worker copies.
      \item \texttt{assign\_input\_slices}: stamps each replicated \texttt{Input}
        with a line-aligned fractional slice $[\Sigma w_{<m}, \Sigma w_{\le m}]$
        weighted by the map-worker count placed on machine $m$, so the $N$ nodes
        cover the file exactly once (result $=1\times$, not $N\times$) and
        per-node SHM $\approx \text{corpus}/N$.
      \item Cross-node edge discovery: any dependency crossing a machine boundary
        is materialised as a \texttt{RemoteSend} (on the producer) and
        \texttt{RemoteRecv} (on the consumer) pair over a fresh slot.
    \end{itemize}
  \item[\texttt{placer.rs} --- node placement.] \texttt{assign\_nodes} assigns a
    \texttt{node\_id} to every node left unplaced: single-host packing when the
    auto nodes fit the most-capable host's limit (minimising cross-node edges),
    otherwise proportional spread by \texttt{capacity} weights with
    largest-remainder quotas and dep-affinity tie-breaking. Carries
    \texttt{PlacementHints} (\texttt{capacity}, \texttt{host\_limit},
    \texttt{cores}).
  \item[\texttt{policies.rs} --- named policies.] Resolves
    \texttt{placement\_policy} (\texttt{pack} / \texttt{balanced} /
    \texttt{spread} / \texttt{weighted}) to concrete \texttt{PlacementHints}.
  \item[\texttt{slot.rs}, \texttt{slot\_assigner.rs} --- slot numbering.] Scans
    the DAG for used slots and assigns conflict-free stream/I-O slot numbers,
    including fan-out base-slot packing.
\end{description}


\section{Execution Engine (Executor \texttt{host})}
\label{sec:engine}

The executor runs one per-node DAG to completion. It embeds a WASM runtime
(\texttt{wasmtime}), the host-side routing and I/O, the SHM allocator, and the
RDMA endpoints.

\subsection{DAG Runner (\texttt{runtime/dag\_runner/})}
\begin{description}
  \item[\texttt{plan.rs} --- scheduling.] \texttt{topo\_sort} (Kahn) orders
    nodes; \texttt{build\_waves} groups nodes with all dependencies in earlier
    waves into the same parallel wave; \texttt{validate\_barrier\_groups} and
    \texttt{build\_barrier\_assignments} bind intra-wave barriers. Slot
    lifetime: \texttt{build\_slot\_refcounts}, \texttt{node\_owned\_slots}
    (slots whose pages are freed by a node), and
    \texttt{node\_routed\_upstream\_slots} (slots whose metadata is cleared, not
    freed, because their pages were spliced downstream).
  \item[\texttt{mod.rs} --- the run loop.] Executes waves in order; within a
    wave it partitions nodes into subprocess WASM/Python workers, host routing
    nodes, and RDMA threads, runs them concurrently, joins, then performs
    per-wave slot reclamation. It times each wave (per-run, staging-excluded
    compute breakdown) and drives multi-run/chunked execution. Reclamation
    correctly keeps a \texttt{RemoteRecv} slot alive until its real consumer
    runs (any consumer kind), not just I/O-slot consumers.
  \item[\texttt{dispatch.rs} --- node execution.] Per-kind handlers: spawn WASM
    subprocesses, run \texttt{Aggregate}/\texttt{Bridge}/\texttt{Shuffle}/
    \texttt{Broadcast}, drive \texttt{StreamPipeline}, load \texttt{Input}
    (whole-file, prefetch, line-aligned slice, or chunked), and write
    \texttt{Output} (concatenated, or one file per record via
    \texttt{split\_records}).
  \item[\texttt{workers.rs} --- WASM worker spawn.] Launches each WASM node as a
    separate \texttt{host wasm-call <shm> <wasm> <func> <ret> <arg>} process so
    fan-out workers run concurrently against the shared SHM.
  \item[\texttt{pipeline.rs} --- streaming.] Executes a \texttt{StreamPipeline}
    as persistent per-stage workers, software-pipelining \texttt{rounds} across
    ticks (stage $s$ of round $r$ runs at tick $r+s$), with optional per-round
    cross-node \texttt{rdma\_recv}/\texttt{rdma\_send}.
\end{description}

\subsection{Routing (\texttt{routing/})}
Zero-copy operators that move data by re-linking page chains.
\begin{description}
  \item[\texttt{chain\_splicer.rs}] The shared primitive: \texttt{chain\_onto}
    (O(1) append of a source chain onto a destination via
    \texttt{writer\_tails}) and \texttt{merge\_into} (sequential or parallel
    tree-merge of $N$ upstreams).
  \item[\texttt{aggregate.rs}] $N\!\to\!1$ merge of upstream chains into one
    downstream slot.
  \item[\texttt{broadcast.rs}] $N\!\to\!M$ fan-out (each upstream linked to every
    downstream).
  \item[\texttt{shuffle.rs}] $N\!\to\!M$ partitioned routing under Modulo,
    RoundRobin, or FixedMap policies.
  \item[\texttt{stream.rs}, \texttt{dispatch.rs}] 1-to-1 bridge and file/owned
    work dispatch.
\end{description}

\subsection{I/O (\texttt{runtime/input\_output/})}
\begin{description}
  \item[\texttt{slot\_loader.rs}] Memory-maps an input file and writes records
    into an I/O slot: \texttt{load} (whole file), \texttt{load\_chunk}
    (offset/length, line-aligned), \texttt{load\_slice}/\texttt{prefetch\_slice}
    (fractional $[lo,hi)$ window using the MapReduce split rule --- a node owns
    the lines that \emph{begin} in its window, so adjacent nodes share the
    boundary and every line is read exactly once).
  \item[\texttt{slot\_flusher.rs}] Drains an I/O slot to disk: \texttt{save\_slot}
    (all records to one file) and \texttt{save\_slot\_split} (record $i$ to
    \texttt{paths}$[i]$, one file per record).
  \item[\texttt{persistence.rs}, \texttt{logger.rs}] Background slot/atomic
    snapshots and the guest diagnostic log arena.
\end{description}

\subsection{Memory Management (\texttt{runtime/mem\_operation/}, \texttt{extended\_pool/})}
\begin{description}
  \item[\texttt{reclaimer.rs}] Page allocator and slot reclamation:
    \texttt{alloc\_page} (free-list $\to$ direct bump $\to$ paged-mode fallback),
    \texttt{free\_stream\_slot}/\texttt{free\_io\_slot} (return a chain's pages
    and zero its head/tail), \texttt{clear\_stream\_slot} (zero metadata only,
    for routed-away chains), and free-list trimming.
  \item[\texttt{extended\_pool/}] Extends addressable memory beyond the
    32-bit/2\,GiB WASM direct window. Pages with \texttt{PageId} $\ge$
    \texttt{DIRECT\_LIMIT} live in a host-side \texttt{GlobalPool} outside SHM
    and are made visible to guests through a \texttt{ResolutionBuffer};
    \texttt{runtime::resolve} maps any \texttt{PageId} to a host pointer.
  \item[\texttt{organizer.rs}, \texttt{slicer.rs}] Page-chain reorganisation and
    record-aligned chain splitting (e.g. the zero-copy contiguous split used by
    distribution).
\end{description}


\section{Shared-Memory Substrate (\texttt{common})}
\label{sec:shm}

A single \texttt{mmap}'d file per node holds all state. The \texttt{Superblock}
header contains atomic counters, the bump allocator pointer, the
\texttt{writer\_heads}/\texttt{writer\_tails} arrays for stream slots, the
\texttt{io\_heads}/\texttt{io\_tails} arrays for I/O slots, sharded free-list
roots, and futex barrier words. Below it lie the registry arena (name$\to$index
map), an RDMA scratch region (one-sided atomic results), an atomic arena
(named CAS variables), a log arena, and the bump-allocated 4\,KiB stream pages.

\begin{description}
  \item[Page chains.] Each slot is a singly linked list of pages
    (\texttt{next\_offset}); a writer appends, a reader walks
    \texttt{head}$\to$\texttt{tail}. Routing re-links chains rather than copying.
  \item[Stream vs.\ I/O slots.] Stream slots ($2048$) carry inter-operator
    streams; I/O slots ($512$) carry file inputs/outputs. Both are
    head/tail-indexed page chains.
  \item[Growth.] Starts at \texttt{INITIAL\_SHM\_SIZE} $=64$\,MiB and grows
    geometrically on demand up to \texttt{CAPACITY\_HARD\_LIMIT} $=2$\,GiB via
    \texttt{try\_grow\_shm}/\texttt{host\_remap}.
  \item[Barriers.] Up to 64 futex words enable intra-wave, single-node
    barrier synchronisation (\texttt{ShmApi::barrier\_wait}); cross-node
    synchronisation between waves uses \texttt{RemoteSend}/\texttt{RemoteRecv}.
\end{description}


\section{RDMA Transport (\texttt{connect})}
\label{sec:rdma}

Cross-node data movement uses a full reliable-connection (RC) queue-pair mesh
with the SHM registered as a memory region, so payloads are written directly
into a peer's SHM with no TCP data copy.

\begin{description}
  \item[\texttt{mesh/} (\texttt{MeshNode}).] Establishes the QP mesh, owns the
    memory regions, and exposes per-peer send/recv control channels and
    one-sided atomic operations. Manages three lazily-created, idle-reclaimed
    overflow regions: \textbf{MR1} (the first 64\,MiB of SHM, the common case),
    \textbf{MR2} (a receiver-side host buffer for transfers that overflow MR1's
    bump budget, later CPU-copied into a fresh page chain), \textbf{MR-src-ext}
    (sender-side, covering SHM pages past the original MR1 boundary, zero-copy),
    and \textbf{MR-src-stage} (sender-side, for memcpy-staging paged-mode source
    pages before referencing them in an SGE).
  \item[\texttt{rdma/} (verbs FFI).] \texttt{context}, \texttt{queue\_pair},
    \texttt{memory\_region}, and \texttt{exchange} (TCP handshake of QP info and
    per-transfer destination replies) wrap \texttt{libibverbs}.
  \item[Transfer protocols.] \emph{Sender-initiated} (default): the sender walks
    the source chain into an SGE list (each SGE tagged with the lkey of the MR
    its page lives in), announces \texttt{total\_bytes}; the receiver replies
    \texttt{SingleMr}\{dest\} or \texttt{UseMr2}\{addr,rkey\}; the sender
    RDMA-writes; a TCP ``done'' triggers any MR2 copy-back. A
    \emph{receiver-initiated} variant is also provided. The
    \texttt{execute\_remote\_send}/\texttt{execute\_remote\_recv} host functions
    drive these. One-sided \texttt{RemoteAtomicFetchAdd}/\texttt{CmpSwap}/
    \texttt{Push} target named atoms in the MR1 atomic arena.
\end{description}


\section{Guest Workloads (\texttt{guest})}
\label{sec:guest}

Workloads are Rust compiled to \texttt{wasm32-unknown-unknown} and invoked one
exported function per DAG node. They read and write SHM through a typed API and
never see raw pointers across the host boundary.

\begin{description}
  \item[\texttt{api/} (\texttt{ShmApi}).] Typed access to the SHM substrate:
    \texttt{stream\_area} / \texttt{io\_area} (read/append slot records),
    \texttt{output} (final results), \texttt{fan\_out} (split a stream into a
    base..base$+n$ range of consumer slots; \texttt{unpack\_fanout\_arg} decodes
    the packed base/consumer-count argument), \texttt{atomic\_arena} /
    \texttt{shared\_area} (named atomics and shared state), \texttt{barrier}
    (futex barrier), \texttt{remote} (per-round RDMA hooks for pipelines),
    \texttt{page\_allocator} (guest-side chain building), and \texttt{log\_arena}
    (diagnostics).
  \item[\texttt{workloads/}] Reference pipelines: word count
    (\texttt{wc\_distribute} $\to$ \texttt{wc\_map} $\to$ \texttt{wc\_reduce}),
    TF--IDF, FINRA audit, ML training, the image pipeline
    (\texttt{img\_load\_ppm}, \texttt{img\_rotate}, \texttt{img\_grayscale},
    \texttt{img\_equalize}, \texttt{img\_blur}, \texttt{img\_export\_ppm}), and
    streaming/routing tests.
\end{description}


\section{Correctness and Resource Properties}
\label{sec:properties}

\begin{itemize}
  \item \textbf{Exactly-once data coverage.} Data-parallel slicing
    (\S\ref{sec:partitioner}) partitions the input at line boundaries so the
    union over $N$ nodes is the whole file with no gaps or overlaps; the
    distributed result equals the single-node result ($1\times$).
  \item \textbf{Slot-lifetime safety.} Routed-away chains are metadata-cleared
    while owned chains are freed, and a cross-node \texttt{RemoteRecv} slot is
    retained until its consumer (of any kind) has run, preventing the
    use-after-free that silently drops a remote node's contribution.
  \item \textbf{Capacity-proportional fan-out.} Fan-out workers and the input
    slice are both apportioned by per-host capacity and bounded by the cluster
    core budget, so stronger hosts do proportionally more work without
    oversubscribing any node.
  \item \textbf{Bounded per-node memory.} Slicing keeps per-node input SHM at
    $\approx \text{corpus}/N$; memory accounting sums the whole executor process
    tree's private RSS with the shared SHM counted once.
\end{itemize}
