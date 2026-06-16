#!/usr/bin/env python3
# driver.py — RMMap (dmerge) WordCount, ES protocol, single-box runner.
#
# RMMap's app normally runs splitter -> mapper x N -> reducer as Knative
# Eventing (Sequence/Parallel/Triggers) over its dmerge runtime. Knative isn't
# stood up here, so this driver replicates that DAG by calling RMMap's REAL ES
# handlers directly (functions.splitter/mapper/reducer, ES branch) inside the
# dmerge-wordcount container, threading state through real Redis with real
# pickle ser/deser. What's faithful: the ES data path and its profile
# (execute_time / es_time = Redis round-trip / sd_time = pickle ser-deser) — the
# serialized inter-stage transfer we contrast against our zero-copy page-chain.
# What's dropped: the HTTP/CloudEvent envelope (network nt_time) and Knative
# placement — neither is the serialization cost being compared.
#
# The kernel module (MITOSIS) is NOT needed: util sets SD=0 for ES, so no
# sopen()/RDMA syscalls run. (DMERGE would need the module — parked.)
#
# Env: PROTOCOL=ES, MAPPER_NUM=N, WC_CORPUS=<path>, REDIS_HOST/PORT/PASSWORD.
# Prints one CSV row (and --header) in a shape aligned with the other systems.
import copy
import os
import pickle
import resource
import sys
import time


def _run_mapper(payload):
    """Worker for the parallel (spawn) path — mirrors a separate Knative mapper
    pod: fresh process, own Redis connection, reads the whole corpus from Redis
    and counts its 1/N slice. Returns the mutated meta dict."""
    env, idx, split_out = payload
    os.environ.update(env)
    os.environ['ID'] = str(idx)
    import functions  # fresh import in the spawned process
    return functions.mapper(split_out)


def main():
    args = [a for a in sys.argv[1:] if a != '--header']
    want_header = '--header' in sys.argv
    N = int(args[0]) if args else int(os.environ.get('MAPPER_NUM', '4'))
    os.environ['PROTOCOL'] = 'ES'
    os.environ['MAPPER_NUM'] = str(N)
    corpus = os.environ.get('WC_CORPUS', '/data/corpus.txt')
    size_mb = round(os.path.getsize(corpus) / (1024 * 1024))

    import util
    import functions
    util.redis_client.flushdb()

    t0 = time.time()
    # ── splitter ──────────────────────────────────────────────────────────────
    split_out = functions.splitter({'loop': '0'})

    # ── mappers (fan-out): each reads its slice via Redis, writes partial_<i> ──
    # MAPPER_PARALLEL=1 runs them as concurrent processes (≈ Knative pods), so
    # the per-mapper regex AND the N× Redis reads overlap across cores — the
    # faithful parallel deployment. Default 0 = sequential (one process).
    parallel = os.environ.get('MAPPER_PARALLEL', '0') == '1'
    if parallel and N > 1:
        import multiprocessing as mp
        env = {k: os.environ[k] for k in
               ('PROTOCOL', 'MAPPER_NUM', 'WC_CORPUS', 'REDIS_HOST',
                'REDIS_PORT', 'REDIS_PASSWORD') if k in os.environ}
        payloads = [(env, i, copy.deepcopy(split_out)) for i in range(N)]
        with mp.get_context('spawn').Pool(N) as pool:
            mapper_outs = pool.map(_run_mapper, payloads)
    else:
        mapper_outs = []
        for i in range(N):
            os.environ['ID'] = str(i)
            mapper_outs.append(functions.mapper(copy.deepcopy(split_out)))

    # ── reducer (fan-in): reads the N partials via Redis, merges ──────────────
    os.environ['UPSTREAM_NUM'] = str(N)
    functions.reducer(mapper_outs)
    e2e_s = time.time() - t0

    # Profile: reducer_es mutates metas[-1]['profile'] in place.
    prof = mapper_outs[-1]['profile']

    def stage_get(stage, key):
        return prof.get(stage, {}).get(key, 0)

    # es/sd/execute summed across the stages (mapper es/sd summed over all N).
    map_es = sum(mo['profile'].get('mapper', {}).get('es_time', 0) for mo in mapper_outs)
    map_sd = sum(mo['profile'].get('mapper', {}).get('sd_time', 0) for mo in mapper_outs)
    map_exec = sum(mo['profile'].get('mapper', {}).get('execute_time', 0) for mo in mapper_outs)
    es_time = stage_get('splitter', 'es_time') + map_es + stage_get('reducer', 'es_time')
    sd_time = stage_get('splitter', 'sd_time') + map_sd + stage_get('reducer', 'sd_time')
    exec_time = stage_get('splitter', 'execute_time') + map_exec + stage_get('reducer', 'execute_time')

    # total_occurrences: re-merge the N partials straight from Redis (validates
    # the end-to-end Redis round-trip and the per-ID key fix).
    total = {}
    for mo in mapper_outs:
        part = pickle.loads(util.redis_client.get(mo['s3_obj_key']))
        for w, c in part.items():
            total[w] = total.get(w, 0) + c
    occ = sum(total.values())

    # Bytes serialized through Redis (the transfer we measure).
    kvs_mb = prof.get('sd_bytes_len', 0) / (1024 * 1024)
    # ru_maxrss in KB; add largest child (RUSAGE_CHILDREN) so parallel mode (N
    # mapper procs each loading the whole corpus — the broadcast) isn't undercounted.
    peak_mb = (resource.getrusage(resource.RUSAGE_SELF).ru_maxrss +
               resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss) / 1024.0
    e2e_ms = e2e_s * 1000.0
    mbps = size_mb / e2e_s if e2e_s else 0
    wps = occ / e2e_s if e2e_s else 0

    header = ('size_mb,workers,topo,e2e_ms,throughput_mb_s,words_per_s,'
              'peak_mem_mb,exec_ms,sd_ms,es_ms,kvs_ser_mb,total_occurrences')
    row = ('%d,%d,redis-es,%.1f,%.2f,%.0f,%.1f,%.1f,%.1f,%.1f,%.2f,%d' %
           (size_mb, N, e2e_ms, mbps, wps, peak_mb, exec_time, sd_time,
            es_time, kvs_mb, occ))
    if want_header:
        print(header)
    print(row)


if __name__ == '__main__':
    main()
