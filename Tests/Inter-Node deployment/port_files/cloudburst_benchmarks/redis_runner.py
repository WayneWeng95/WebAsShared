#  Redis-backed local runner for Cloudburst benchmarks.
#
#  Cloudburst normally runs a zmq scheduler + executor control plane over the
#  Anna KVS. That stack (protobuf-generated stubs, pyzmq, cloudpickle 0.6, the
#  Anna C++ client) is a 2019/Python-3.6 environment and isn't available on this
#  host (Python 3.14, no Anna). This runner provides a faithful, single-process
#  stand-in that:
#
#    * executes the benchmark's REAL function bodies and DAG topology unchanged
#      (e.g. wordcount.py's split / mapper / reducer), and
#    * routes every inter-stage value THROUGH Redis, serialized with the same
#      cloudpickle the Cloudburst serializer uses —
#
#  so the quantity the comparison cares about (state moved between DAG stages,
#  serialized through an external KVS) is reproduced. What it deliberately drops:
#  the zmq scheduler, multi-executor placement/scaling, and Anna's lattice
#  consistency — none of which the word-count data-transfer measurement needs.
#
#  `RedisCloudburst` implements both API levels the benchmarks touch:
#    - connection level: register / register_dag / put_object / get_object /
#      call_dag  (what `cloudburst_client` exposes), and
#    - in-function KVS level: put / get  (what the `cloudburst` arg exposes
#      inside each function body).
#  The same object is handed to functions as their `cloudburst` argument, mirror-
#  ing CloudburstUserLibrary.
#
#  Usage (see __main__): build a RedisCloudburst, then call a benchmark's
#  run(client, num_requests, sckt=None) exactly as server.py's run_bench would.

import os
import sys
import time
import uuid

import cloudpickle as cp

from cloudburst.shared.reference import CloudburstReference
from cloudburst.shared.redis_kvs import RedisKvsClient


class _Registered:
    '''Mimics the truthy handle CloudburstConnection.register returns.'''
    def __init__(self, name):
        self.name = name

    def __bool__(self):
        return True


class RedisCloudburst:
    def __init__(self, kvs=None):
        self.kvs_client = kvs or RedisKvsClient()
        self._funcs = {}     # name -> callable
        self._func_bytes = 0  # cloudpickle size of registered funcs (one-off)
        self._dags = {}      # name -> (functions, connections)
        self.last_result = None  # terminal result of the most recent call_dag

    # -- connection-level API --------------------------------------------------

    def register(self, function, name):
        # Cloudburst cloudpickles the function to ship it to executors; we keep
        # the live callable for in-process execution but record the serialized
        # size for parity.
        try:
            self._func_bytes += len(cp.dumps(function))
        except Exception:
            pass
        self._funcs[name] = function
        return _Registered(name)

    def register_dag(self, name, functions, connections,
                     gpu_functions=[], batching_functions=[], colocated=[]):
        fnames = [f[0] if isinstance(f, tuple) else f for f in functions]
        for fname in fnames:
            if fname not in self._funcs:
                return False, 'Function %s not registered.' % fname
        self._dags[name] = (fnames, list(connections))
        return True, None

    def put_object(self, key, value):
        # serializer.dump_lattice(value) -> kvs.put; payload is cloudpickle bytes.
        return self.kvs_client.put(key, cp.dumps(value))

    def get_object(self, key):
        raw = self.kvs_client.get(key)[key]
        return None if raw is None else cp.loads(raw)

    # -- in-function KVS API (CloudburstUserLibrary.put/get) -------------------

    def put(self, ref, value):
        return self.kvs_client.put(ref, cp.dumps(value))

    def get(self, ref, deserialize=True):
        single = not isinstance(ref, list)
        refs = [ref] if single else ref
        kv = self.kvs_client.get(refs)
        out = {}
        for k in refs:
            raw = kv.get(k)
            out[k] = None if raw is None else (cp.loads(raw) if deserialize
                                               else raw)
        return out[ref] if single else out

    # -- DAG execution ---------------------------------------------------------

    def call_dag(self, dname, arg_map, direct_response=False, **kwargs):
        '''Execute the DAG topologically, in-process, threading every inter-
        stage result through Redis (cloudpickled), and return the terminal
        function's result (direct_response semantics).'''
        if dname not in self._dags:
            raise RuntimeError('DAG %s not registered.' % dname)
        fnames, connections = self._dags[dname]

        # Predecessors in connection order (preserves mapper ordering for the
        # reducer's positional args).
        preds = {f: [] for f in fnames}
        succs = {f: [] for f in fnames}
        for src, sink in connections:
            preds[sink].append(src)
            succs[src].append(sink)

        # Explicit args supplied by the caller, with CloudburstReferences
        # dereferenced from the KVS exactly as the executor would.
        explicit = {}
        for fname, args in arg_map.items():
            if not isinstance(args, list):
                args = [args]
            explicit[fname] = [self._deref(a) for a in args]

        # Each function's result is stashed in Redis; downstream reads it back
        # (serialized round-trip = the inter-stage transfer we measure).
        result_keys = {}
        run_uid = uuid.uuid4().hex
        done = set()
        order = self._toposort(fnames, preds)
        terminal_result = None

        for fname in order:
            args = list(explicit.get(fname, []))
            for p in preds[fname]:
                rk = result_keys[p]
                raw = self.kvs_client.get(rk)[rk]
                args.append(cp.loads(raw))
            func = self._funcs[fname]
            result = func(self, *args)
            rk = '%s_%s_result' % (run_uid, fname)
            self.kvs_client.put(rk, cp.dumps(result))
            result_keys[fname] = rk
            done.add(fname)
            if not succs[fname]:
                terminal_result = result

        self.last_result = terminal_result
        return terminal_result

    # -- helpers ---------------------------------------------------------------

    def _deref(self, arg):
        if isinstance(arg, CloudburstReference):
            raw = self.kvs_client.get(arg.key)[arg.key]
            if raw is None:
                return None
            return cp.loads(raw) if arg.deserialize else raw
        return arg

    @staticmethod
    def _toposort(fnames, preds):
        indeg = {f: len(preds[f]) for f in fnames}
        ready = [f for f in fnames if indeg[f] == 0]
        order = []
        # Build successor counts from preds.
        succ = {f: [] for f in fnames}
        for f in fnames:
            for p in preds[f]:
                succ[p].append(f)
        while ready:
            f = ready.pop(0)
            order.append(f)
            for s in succ[f]:
                indeg[s] -= 1
                if indeg[s] == 0:
                    ready.append(s)
        if len(order) != len(fnames):
            raise RuntimeError('DAG has a cycle or disconnected node.')
        return order
