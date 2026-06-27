// rdma_wc.c — user-space RDMA one-sided READ transport for the RMMap (RDMA) WordCount
// baseline. NO kernel module: uses librdmacm/libibverbs over the existing RoCE NIC.
//
// This realizes RMMap/DMERGE's *data path* for flat state: instead of serializing the
// inter-stage chunk through a KVS (the ES/Cloudburst/Faasm path), the mapper maps the
// producer's memory remotely and the NIC DMAs the bytes straight into the mapper's
// buffer — no pickle, no Redis staging, no CPU copy. (vs true MITOSIS it lacks pointer
// remapping + lazy paging, but for WordCount the big state is flat bytes, so it is the
// same zero-copy transfer; see README.)
//
//   serve: mmap the corpus, register ONE MR (REMOTE_READ) over it, rdma_listen. Each
//          mapper connects with its chunk index in private_data; we reply (in the accept
//          private_data) with {remote_addr=base+offset, rkey, len} for that newline-
//          aligned chunk. The RDMA READ is one-sided — the server CPU does nothing for
//          the transfer, it just keeps the QP/MR alive.
//   map:   connect, read {addr,rkey,len} from the established private_data, post one
//          RDMA_READ into a local buffer, then count [a-z]+ words (same tokenizer as
//          every other bar) and print "RESULT busy_ms=<f> read_ms=<f> occ=<d> unique=<d>".
//
// Build: gcc -O2 -o rdma_wc rdma_wc.c -lrdmacm -libverbs
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <arpa/inet.h>
#include <rdma/rdma_cma.h>

// MR info handed to a mapper in the accept private_data (packed, < 56B RoCE limit).
struct chunk_info {
    uint64_t addr;   // remote VA of this chunk (MR base + offset)
    uint32_t rkey;   // MR remote key
    uint32_t len;    // chunk length (bytes)
} __attribute__((packed));

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000.0 + t.tv_nsec / 1e6;
}

// Newline-aligned chunk boundaries — byte-for-byte the same as wc_ops.shard_bytes:
// even split, then push each interior cut forward to the next '\n'.
static void chunk_bounds(const char *data, size_t total, int n, size_t *bounds) {
    for (int i = 0; i <= n; i++) bounds[i] = (size_t)i * total / n;
    for (int i = 1; i < n; i++) {
        const char *nl = memchr(data + bounds[i], '\n', total - bounds[i]);
        bounds[i] = nl ? (size_t)(nl - data) + 1 : total;
    }
}

// ───────────────────────────── serve ─────────────────────────────
static int do_serve(const char *corpus, int n, const char *bind_ip, int port) {
    int fd = open(corpus, O_RDONLY);
    if (fd < 0) { perror("open corpus"); return 1; }
    struct stat st; fstat(fd, &st);
    size_t total = st.st_size;
    char *data = mmap(NULL, total, PROT_READ, MAP_PRIVATE, fd, 0);
    if (data == MAP_FAILED) { perror("mmap"); return 1; }
    size_t *bounds = malloc((n + 1) * sizeof(size_t));
    chunk_bounds(data, total, n, bounds);
    fprintf(stderr, "[serve] corpus=%s size=%zu fanout=%d listen=%s:%d\n",
            corpus, total, n, bind_ip, port);

    struct rdma_event_channel *ec = rdma_create_event_channel();
    struct rdma_cm_id *listen_id;
    rdma_create_id(ec, &listen_id, NULL, RDMA_PS_TCP);
    struct sockaddr_in sa = {0};
    sa.sin_family = AF_INET;
    sa.sin_port = htons(port);
    inet_pton(AF_INET, bind_ip, &sa.sin_addr);
    if (rdma_bind_addr(listen_id, (struct sockaddr *)&sa)) { perror("bind"); return 1; }
    if (rdma_listen(listen_id, 256)) { perror("listen"); return 1; }

    struct ibv_pd *pd = NULL;
    struct ibv_mr *mr = NULL;     // one MR over the whole corpus, registered lazily
    fprintf(stderr, "[serve] ready\n"); fflush(stderr);

    struct rdma_cm_event *ev;
    while (rdma_get_cm_event(ec, &ev) == 0) {
        // Copy out everything we need, then ACK before acting: rdma_destroy_id() blocks
        // until the event is acked, so destroying on DISCONNECTED before the ack
        // deadlocks the whole server (which is what once wedged it after one read).
        struct rdma_cm_id *id = ev->id;
        enum rdma_cm_event_type etype = ev->event;
        int idx = -1;
        if (etype == RDMA_CM_EVENT_CONNECT_REQUEST &&
            ev->param.conn.private_data && ev->param.conn.private_data_len >= 4)
            memcpy(&idx, ev->param.conn.private_data, 4);
        rdma_ack_cm_event(ev);

        if (etype == RDMA_CM_EVENT_CONNECT_REQUEST) {
            if (!pd) {                                  // first connection: bind PD+MR to the device
                pd = ibv_alloc_pd(id->verbs);
                // REMOTE_READ only — the corpus is mmap'd PROT_READ, so LOCAL_WRITE
                // (which needs writable pages) would EFAULT; an RDMA-READ source does
                // not need it. This MR registration is the one-time "publish" cost
                // (pin + map the whole corpus for remote read) — done ONCE, then every
                // mapper across every rep reads from it: register-once / read-many, the
                // DMERGE win vs the KVS bars that re-upload the corpus each rep.
                double mr0 = now_ms();
                mr = ibv_reg_mr(pd, data, total, IBV_ACCESS_REMOTE_READ);
                if (!mr) { perror("reg_mr"); return 1; }
                fprintf(stderr, "PUBLISH reg_mr_ms=%.3f rkey=0x%x bytes=%zu\n",
                        now_ms() - mr0, mr->rkey, total);
            }
            struct ibv_qp_init_attr qa = {0};
            qa.cap.max_send_wr = 1; qa.cap.max_recv_wr = 1;
            qa.cap.max_send_sge = 1; qa.cap.max_recv_sge = 1;
            qa.qp_type = IBV_QPT_RC;
            qa.send_cq = ibv_create_cq(id->verbs, 4, NULL, NULL, 0);
            qa.recv_cq = qa.send_cq;
            if (rdma_create_qp(id, pd, &qa)) { perror("create_qp(srv)"); rdma_reject(id, NULL, 0); }
            else {
                struct chunk_info ci = {0};
                if (idx >= 0 && idx < n) {
                    ci.addr = (uint64_t)(uintptr_t)data + bounds[idx];
                    ci.rkey = mr->rkey;
                    ci.len = (uint32_t)(bounds[idx + 1] - bounds[idx]);
                }
                struct rdma_conn_param cp = {0};
                cp.private_data = &ci; cp.private_data_len = sizeof(ci);
                cp.responder_resources = 16; cp.initiator_depth = 16;
                rdma_accept(id, &cp);
            }
        } else if (etype == RDMA_CM_EVENT_DISCONNECTED) {
            rdma_destroy_qp(id);
            rdma_destroy_id(id);
        }
    }
    return 0;
}

// ───────────────────────────── map ─────────────────────────────
// [a-z]+ word counter — same tokenization as wc_ops.wc_map (lowercase, runs of a-z).
// Open-addressing hash over interned word bytes; we only need occ (= Σ counts) and
// unique for the printout, so we store (hash, count) buckets and compare full words.
struct wc_map { char **keys; int *cnt; int cap, size; };
static void wc_init(struct wc_map *m, int cap) {
    m->cap = cap; m->size = 0;
    m->keys = calloc(cap, sizeof(char *));
    m->cnt = calloc(cap, sizeof(int));
}
static void wc_add(struct wc_map *m, const char *w, int len) {
    uint64_t h = 1469598103934665603ULL;                 // FNV-1a
    for (int i = 0; i < len; i++) { h ^= (unsigned char)w[i]; h *= 1099511628211ULL; }
    int j = h % m->cap;
    while (m->keys[j]) {
        if ((int)strlen(m->keys[j]) == len && memcmp(m->keys[j], w, len) == 0) { m->cnt[j]++; return; }
        j = (j + 1) % m->cap;
    }
    m->keys[j] = strndup(w, len); m->cnt[j] = 1; m->size++;
}

static int do_map(const char *srv_ip, int port, int idx) {
    struct rdma_event_channel *ec = rdma_create_event_channel();
    struct rdma_cm_id *id;
    rdma_create_id(ec, &id, NULL, RDMA_PS_TCP);
    struct sockaddr_in sa = {0};
    sa.sin_family = AF_INET; sa.sin_port = htons(port);
    inet_pton(AF_INET, srv_ip, &sa.sin_addr);

    struct rdma_cm_event *ev;
    rdma_resolve_addr(id, NULL, (struct sockaddr *)&sa, 2000);
    rdma_get_cm_event(ec, &ev); rdma_ack_cm_event(ev);    // ADDR_RESOLVED
    rdma_resolve_route(id, 2000);
    rdma_get_cm_event(ec, &ev); rdma_ack_cm_event(ev);    // ROUTE_RESOLVED

    struct ibv_pd *pd = ibv_alloc_pd(id->verbs);
    struct ibv_cq *cq = ibv_create_cq(id->verbs, 4, NULL, NULL, 0);
    struct ibv_qp_init_attr qa = {0};
    qa.cap.max_send_wr = 4; qa.cap.max_recv_wr = 1;
    qa.cap.max_send_sge = 1; qa.cap.max_recv_sge = 1;
    qa.qp_type = IBV_QPT_RC; qa.send_cq = cq; qa.recv_cq = cq;
    rdma_create_qp(id, pd, &qa);

    struct rdma_conn_param cp = {0};
    cp.private_data = &idx; cp.private_data_len = sizeof(idx);
    cp.responder_resources = 16; cp.initiator_depth = 16;
    rdma_connect(id, &cp);
    if (rdma_get_cm_event(ec, &ev)) { perror("get established"); return 1; }
    if (ev->event != RDMA_CM_EVENT_ESTABLISHED) {
        fprintf(stderr, "[map] connect failed: event=%d\n", ev->event); return 1;
    }
    struct chunk_info ci;
    memcpy(&ci, ev->param.conn.private_data, sizeof(ci));
    rdma_ack_cm_event(ev);
    if (ci.len == 0) { fprintf(stderr, "[map] bad chunk idx=%d\n", idx); return 1; }

    char *buf = malloc(ci.len);
    struct ibv_mr *lmr = ibv_reg_mr(pd, buf, ci.len, IBV_ACCESS_LOCAL_WRITE);

    double t0 = now_ms();
    struct ibv_sge sge = { .addr = (uint64_t)(uintptr_t)buf, .length = ci.len, .lkey = lmr->lkey };
    struct ibv_send_wr wr = {0}, *bad;
    wr.opcode = IBV_WR_RDMA_READ; wr.sg_list = &sge; wr.num_sge = 1;
    wr.send_flags = IBV_SEND_SIGNALED;
    wr.wr.rdma.remote_addr = ci.addr; wr.wr.rdma.rkey = ci.rkey;
    if (ibv_post_send(id->qp, &wr, &bad)) { perror("post RDMA_READ"); return 1; }
    struct ibv_wc wc;
    int ne; do { ne = ibv_poll_cq(cq, 1, &wc); } while (ne == 0);
    if (wc.status != IBV_WC_SUCCESS) { fprintf(stderr, "[map] read wc status=%d\n", wc.status); return 1; }
    double read_ms = now_ms() - t0;

    // count [a-z]+ words directly on the DMA'd buffer (no copy-out).
    struct wc_map m; wc_init(&m, 8192);
    long occ = 0;
    uint32_t i = 0;
    while (i < ci.len) {
        unsigned char c = buf[i];
        unsigned char lc = (c >= 'A' && c <= 'Z') ? c + 32 : c;
        if (lc >= 'a' && lc <= 'z') {
            uint32_t s = i; char word[256]; int wl = 0;
            while (i < ci.len) {
                c = buf[i]; lc = (c >= 'A' && c <= 'Z') ? c + 32 : c;
                if (lc < 'a' || lc > 'z') break;
                if (wl < 255) word[wl++] = lc;
                i++;
            }
            (void)s; wc_add(&m, word, wl); occ++;
        } else i++;
    }
    double busy_ms = now_ms() - t0;
    printf("RESULT busy_ms=%.3f read_ms=%.3f occ=%ld unique=%d len=%u\n",
           busy_ms, read_ms, occ, m.size, ci.len);

    rdma_disconnect(id);
    return 0;
}

// ───────────────────────────── read ─────────────────────────────
// Same one-sided RDMA READ as `map`, but emit the raw chunk bytes on stdout (no
// counting) — used by the Python mapper so the WordCount COMPUTE stays identical to
// every other bar (wc_ops.wc_map). Prints read_ms + len on stderr. Refactors the
// connect+read out of do_map by reusing it (do_map keeps its in-C counter for the
// transport self-test); here we duplicate the small connect+read path and dump bytes.
static int do_read(const char *srv_ip, int port, int idx) {
    struct rdma_event_channel *ec = rdma_create_event_channel();
    struct rdma_cm_id *id;
    rdma_create_id(ec, &id, NULL, RDMA_PS_TCP);
    struct sockaddr_in sa = {0};
    sa.sin_family = AF_INET; sa.sin_port = htons(port);
    inet_pton(AF_INET, srv_ip, &sa.sin_addr);
    struct rdma_cm_event *ev;
    rdma_resolve_addr(id, NULL, (struct sockaddr *)&sa, 2000);
    rdma_get_cm_event(ec, &ev); rdma_ack_cm_event(ev);
    rdma_resolve_route(id, 2000);
    rdma_get_cm_event(ec, &ev); rdma_ack_cm_event(ev);
    struct ibv_pd *pd = ibv_alloc_pd(id->verbs);
    struct ibv_cq *cq = ibv_create_cq(id->verbs, 4, NULL, NULL, 0);
    struct ibv_qp_init_attr qa = {0};
    qa.cap.max_send_wr = 4; qa.cap.max_recv_wr = 1;
    qa.cap.max_send_sge = 1; qa.cap.max_recv_sge = 1;
    qa.qp_type = IBV_QPT_RC; qa.send_cq = cq; qa.recv_cq = cq;
    rdma_create_qp(id, pd, &qa);
    struct rdma_conn_param cp = {0};
    cp.private_data = &idx; cp.private_data_len = sizeof(idx);
    cp.responder_resources = 16; cp.initiator_depth = 16;
    rdma_connect(id, &cp);
    if (rdma_get_cm_event(ec, &ev) || ev->event != RDMA_CM_EVENT_ESTABLISHED) {
        fprintf(stderr, "[read] connect failed\n"); return 1; }
    struct chunk_info ci; memcpy(&ci, ev->param.conn.private_data, sizeof(ci));
    rdma_ack_cm_event(ev);
    if (ci.len == 0) { fprintf(stderr, "[read] bad chunk idx=%d\n", idx); return 1; }
    char *buf = malloc(ci.len);
    struct ibv_mr *lmr = ibv_reg_mr(pd, buf, ci.len, IBV_ACCESS_LOCAL_WRITE);
    double t0 = now_ms();
    struct ibv_sge sge = { .addr = (uint64_t)(uintptr_t)buf, .length = ci.len, .lkey = lmr->lkey };
    struct ibv_send_wr wr = {0}, *bad;
    wr.opcode = IBV_WR_RDMA_READ; wr.sg_list = &sge; wr.num_sge = 1;
    wr.send_flags = IBV_SEND_SIGNALED;
    wr.wr.rdma.remote_addr = ci.addr; wr.wr.rdma.rkey = ci.rkey;
    if (ibv_post_send(id->qp, &wr, &bad)) { perror("post RDMA_READ"); return 1; }
    struct ibv_wc wc; int ne; do { ne = ibv_poll_cq(cq, 1, &wc); } while (ne == 0);
    if (wc.status != IBV_WC_SUCCESS) { fprintf(stderr, "[read] wc status=%d\n", wc.status); return 1; }
    double read_ms = now_ms() - t0;
    fprintf(stderr, "READ read_ms=%.3f len=%u\n", read_ms, ci.len);
    fwrite(buf, 1, ci.len, stdout); fflush(stdout);     // raw chunk → Python counts it
    rdma_disconnect(id);
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "serve") == 0) {
        // serve <corpus> <fanout> <bind_ip> <port>
        return do_serve(argv[2], atoi(argv[3]), argv[4], atoi(argv[5]));
    } else if (argc >= 2 && strcmp(argv[1], "map") == 0) {
        // map <server_ip> <port> <chunk_idx>  (self-test: RDMA read + count in C)
        return do_map(argv[2], atoi(argv[3]), atoi(argv[4]));
    } else if (argc >= 2 && strcmp(argv[1], "read") == 0) {
        // read <server_ip> <port> <chunk_idx>  (RDMA read, raw bytes -> stdout)
        return do_read(argv[2], atoi(argv[3]), atoi(argv[4]));
    }
    fprintf(stderr, "usage:\n  %s serve <corpus> <fanout> <bind_ip> <port>\n"
                    "  %s map <server_ip> <port> <chunk_idx>\n"
                    "  %s read <server_ip> <port> <chunk_idx>\n", argv[0], argv[0], argv[0]);
    return 2;
}
