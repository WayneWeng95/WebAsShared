#  Redis-backed KVS for Cloudburst (drop-in for the Anna client).
#
#  Cloudburst's state plane is normally the Anna KVS (a C++ service whose Python
#  client is built from the hydro-project/anna repo). On hosts where Anna isn't
#  available, this module provides a Redis-backed client with the SAME put/get
#  surface Cloudburst's executor + user library call, so the data path — values
#  serialized and round-tripped through an external KVS — is preserved. That
#  serialized round-trip is exactly the inter-stage state-transfer cost the
#  WebAsShared / RMMap-DMERGE benchmarks contrast against (their zero-copy /
#  serialization-free RDMA path).
#
#  Fidelity notes:
#   - Values are wrapped in a trivial last-writer-wins payload and stored as raw
#     bytes under their key (Cloudburst's LWWPairLattice reveals to the same
#     bytes; for word-count we never rely on lattice merge semantics).
#   - get(keys)/put(keys, values) mirror AnnaTcpClient: list-in/dict-out for get
#     ({key: payload-or-None}), {key: bool} for put.
#   - Every payload byte crossing the client is tallied (bytes_put / bytes_get)
#     so the harness can report the serialized-transfer volume.

import threading

import redis


class RedisKvsClient:
    '''A minimal Anna-compatible KVS client backed by Redis.

    Only the surface Cloudburst's word-count path exercises is implemented:
    get(keys) and put(keys, values). Payloads are opaque bytes (already
    serialized by cloudburst.shared.serializer before they reach here).
    '''

    def __init__(self, host='127.0.0.1', port=6379, db=0):
        self.r = redis.Redis(host=host, port=port, db=db)
        self._lock = threading.Lock()
        self.bytes_put = 0
        self.bytes_get = 0
        self.n_put = 0
        self.n_get = 0

    # -- Anna-compatible surface ------------------------------------------------

    def get(self, keys):
        single = not isinstance(keys, list)
        if single:
            keys = [keys]

        kv_pairs = {}
        # MGET keeps the round-trip count comparable to Anna's batched get.
        vals = self.r.mget([self._enc(k) for k in keys]) if keys else []
        for key, val in zip(keys, vals):
            kv_pairs[key] = val  # bytes or None
            if val is not None:
                with self._lock:
                    self.bytes_get += len(val)
                    self.n_get += 1
        return kv_pairs

    def put(self, keys, values=None):
        # Accept put(key, value) and put({key: value}) / put([keys], [values]).
        if values is None and isinstance(keys, dict):
            items = list(keys.items())
        else:
            if not isinstance(keys, list):
                keys = [keys]
            if not isinstance(values, list):
                values = [values]
            items = list(zip(keys, values))

        result = {}
        pipe = self.r.pipeline(transaction=False)
        for key, val in items:
            payload = self._as_bytes(val)
            pipe.set(self._enc(key), payload)
            with self._lock:
                self.bytes_put += len(payload)
                self.n_put += 1
            result[key] = True
        if items:
            pipe.execute()
        return result

    # -- helpers ---------------------------------------------------------------

    @staticmethod
    def _enc(key):
        return key.encode() if isinstance(key, str) else key

    @staticmethod
    def _as_bytes(val):
        if isinstance(val, bytes):
            return val
        if isinstance(val, str):
            return val.encode()
        # Last resort: stash the repr. Cloudburst always hands us bytes (the
        # serializer runs first), so this only guards against misuse.
        return repr(val).encode()

    def transfer_stats(self):
        with self._lock:
            return {
                'bytes_put': self.bytes_put,
                'bytes_get': self.bytes_get,
                'n_put': self.n_put,
                'n_get': self.n_get,
            }

    def reset_stats(self):
        with self._lock:
            self.bytes_put = self.bytes_get = 0
            self.n_put = self.n_get = 0

    def flushdb(self):
        self.r.flushdb()
