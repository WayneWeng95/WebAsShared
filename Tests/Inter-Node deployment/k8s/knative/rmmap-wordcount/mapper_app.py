#!/usr/bin/env python3
# mapper_app.py — the RMMap WordCount *mapper* stage, deployed as a Knative Service.
#
# This is RMMap's ES (external-store) protocol — the byte-for-byte analogue of
# port_files/RMMap_exp_wordcount_app/functions.py:mapper_es, but the inter-stage
# state is routed through the shared cluster Redis (the MITOSIS/dmerge RDMA path
# needs a kernel module we are not authorized to load, so ES is the data plane).
#
# One HTTP POST == one mapper invocation. Knative autoscales the pool: warm runs
# pre-warm min-scale pods (steady-state reuse), cold runs scale-to-zero so the
# fan-out pays real pod-scheduling + container + app-init cold-start latency.
#
# Request : {"chunk_key": "<uid>_chunk_i", "result_key": "<uid>_res_i"}
# Response: {execute_ms, sd_ms, es_ms, busy_ms, occ, pod}
#   busy_ms = es(read) + sd(deserialize in) + execute + sd(serialize out) + es(write)
#   — the per-invocation busy span the driver sums into total_job_ms (Σ busy
#   node-seconds), exactly as the Cloudburst executor reports its own busy time.
import os
import pickle
import time

import redis
from flask import Flask, request, jsonify

import wc_ops

R = redis.Redis(host=os.environ.get('REDIS_HOST', 'redis.baselines.svc.cluster.local'),
                port=int(os.environ.get('REDIS_PORT', '6379')))
app = Flask(__name__)


def _ms():
    return time.time() * 1000.0


@app.route('/', methods=['POST'])
def mapper():
    task = request.get_json(force=True)
    chunk_key, result_key = task['chunk_key'], task['result_key']

    # @External store: pull the inbound chunk (KVS read — the cross-node transfer).
    t = _ms(); o = R.get(chunk_key); es = _ms() - t
    o = o if o is not None else b''
    # @Deserialize: chunk bytes -> text (the IR->data step of the ES protocol).
    t = _ms(); text = o.decode('utf-8', 'ignore'); sd = _ms() - t
    # @Execute: the real WordCount body (same tokenizer as every other baseline).
    t = _ms(); partial = wc_ops.wc_map(text); execute = _ms() - t
    # @Serialize: partial dict -> IR.
    t = _ms(); blob = pickle.dumps(partial); sd += _ms() - t
    # @External store: push the inter-stage state for the reducer (KVS write).
    t = _ms(); R.set(result_key, blob); es += _ms() - t

    busy = execute + sd + es
    return jsonify(execute_ms=execute, sd_ms=sd, es_ms=es, busy_ms=busy,
                   occ=sum(partial.values()), pod=os.environ.get('HOSTNAME', ''))


@app.route('/healthz')
def healthz():
    return 'ok'


if __name__ == '__main__':
    app.run(host='0.0.0.0', port=int(os.environ.get('PORT', '8080')), threaded=True)
