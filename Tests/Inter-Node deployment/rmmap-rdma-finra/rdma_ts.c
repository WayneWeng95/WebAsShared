// rdma_ts.c — user-space RDMA one-sided READ transport for the RMMap (RDMA) TeraSort
// baseline. Same mechanism as rdma_wc.c (no kernel module; librdmacm/libibverbs over the
// RoCE NIC), generalized so the all-to-all SHUFFLE is also one-sided RDMA-READ:
//
//   serve     <file> <fanout> <ip> <port>   — input read: mmap+reg_mr the records file,
//             serve chunk[idx] by NEWLINE-ALIGNED equal split (a partitioner RDMA-READs
//             its input chunk straight out of node 0's memory; identical to rdma_wc serve).
//   serve_idx <file> <bounds> <ip> <port>    — shuffle publish: mmap+reg_mr a partitioner's
//             BUCKET file, serve chunk[idx] by EXPLICIT boundaries read from <bounds>
//             (N+1 byte offsets, one per line). bucket[j] = [bounds[j], bounds[j+1]) is the
//             column a merger gathers. The producer registers its buckets ONCE and every
//             merger RDMA-READs its column — the serialization-free shuffle (vs KVS bucket
//             SET/GET) DMERGE gives for flat state.
//   read      <ip> <port> <idx>              — client: one-sided RDMA_READ of chunk[idx] from
//             whichever server, raw bytes → stdout (read_ms+len on stderr). Used unchanged
//             for BOTH the input read (from node 0) and the shuffle gather (from a producer).
//
// Build: gcc -O2 -o rdma_ts rdma_ts.c -lrdmacm -libverbs
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

struct chunk_info {           // handed to a client in the accept private_data (packed, <56B)
    uint64_t addr;            // remote VA of this chunk (MR base + offset)
    uint32_t rkey;            // MR remote key
    uint32_t len;             // chunk length (bytes)
} __attribute__((packed));

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000.0 + t.tv_nsec / 1e6;
}

// Newline-aligned equal split — byte-for-byte the same as wc_ops.shard_bytes /
// ts_ops.split_aligned: even split, then push each interior cut forward to the next '\n'.
static void chunk_bounds(const char *data, size_t total, int n, size_t *bounds) {
    for (int i = 0; i <= n; i++) bounds[i] = (size_t)i * total / n;
    for (int i = 1; i < n; i++) {
        const char *nl = memchr(data + bounds[i], '\n', total - bounds[i]);
        bounds[i] = nl ? (size_t)(nl - data) + 1 : total;
    }
}

// ── the shared serve loop: register ONE REMOTE_READ MR over [data,total), then hand each
//    connecting client its chunk[idx]=(data+bounds[idx], bounds[idx+1]-bounds[idx]). The
//    MR is registered lazily on the first connection (the one-time "publish" cost). ──
static int serve_loop(char *data, size_t total, size_t *bounds, int n,
                      const char *bind_ip, int port) {
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
    struct ibv_mr *mr = NULL;
    fprintf(stderr, "[serve] ready bind=%s:%d bytes=%zu n=%d\n", bind_ip, port, total, n);
    fflush(stderr);

    struct rdma_cm_event *ev;
    while (rdma_get_cm_event(ec, &ev) == 0) {
        struct rdma_cm_id *id = ev->id;
        enum rdma_cm_event_type etype = ev->event;
        int idx = -1;
        if (etype == RDMA_CM_EVENT_CONNECT_REQUEST &&
            ev->param.conn.private_data && ev->param.conn.private_data_len >= 4)
            memcpy(&idx, ev->param.conn.private_data, 4);
        rdma_ack_cm_event(ev);     // ack BEFORE acting (destroy blocks on ack → deadlock)

        if (etype == RDMA_CM_EVENT_CONNECT_REQUEST) {
            if (!pd) {             // first connection: bind PD + register the MR (publish)
                pd = ibv_alloc_pd(id->verbs);
                double mr0 = now_ms();
                mr = ibv_reg_mr(pd, data, total, IBV_ACCESS_REMOTE_READ);
                if (!mr) { perror("reg_mr"); return 1; }
                fprintf(stderr, "PUBLISH reg_mr_ms=%.3f rkey=0x%x bytes=%zu\n",
                        now_ms() - mr0, mr->rkey, total);
                fflush(stderr);
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

static char *map_file(const char *path, size_t *total_out) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { perror("open"); return NULL; }
    struct stat st; fstat(fd, &st);
    size_t total = st.st_size;
    char *data = mmap(NULL, total ? total : 1, PROT_READ, MAP_PRIVATE, fd, 0);
    if (data == MAP_FAILED) { perror("mmap"); return NULL; }
    *total_out = total;
    return data;
}

static int do_serve(const char *file, int n, const char *bind_ip, int port) {
    size_t total;
    char *data = map_file(file, &total);
    if (!data) return 1;
    size_t *bounds = malloc((n + 1) * sizeof(size_t));
    chunk_bounds(data, total, n, bounds);
    fprintf(stderr, "[serve] file=%s size=%zu fanout=%d\n", file, total, n);
    return serve_loop(data, total, bounds, n, bind_ip, port);
}

static int do_serve_idx(const char *file, const char *bounds_path,
                        const char *bind_ip, int port) {
    size_t total;
    char *data = map_file(file, &total);
    if (!data) return 1;
    FILE *bf = fopen(bounds_path, "r");
    if (!bf) { perror("open bounds"); return 1; }
    size_t cap = 16, n1 = 0, *bounds = malloc(cap * sizeof(size_t));
    while (fscanf(bf, "%zu", &bounds[n1]) == 1) {
        if (++n1 == cap) { cap *= 2; bounds = realloc(bounds, cap * sizeof(size_t)); }
    }
    fclose(bf);
    int n = (int)n1 - 1;       // N+1 offsets → N buckets
    fprintf(stderr, "[serve_idx] file=%s size=%zu buckets=%d last_bound=%zu\n",
            file, total, n, bounds[n1 - 1]);
    return serve_loop(data, total, bounds, n, bind_ip, port);
}

// ── client: one-sided RDMA_READ of chunk[idx], raw bytes → stdout ──
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
    if (ci.len == 0) {     // an empty bucket is legal (some owner got no records) → no bytes
        fprintf(stderr, "READ read_ms=0.000 len=0\n");
        rdma_disconnect(id);
        return 0;
    }
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
    fwrite(buf, 1, ci.len, stdout); fflush(stdout);
    rdma_disconnect(id);
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 6 && strcmp(argv[1], "serve") == 0)
        return do_serve(argv[2], atoi(argv[3]), argv[4], atoi(argv[5]));
    if (argc >= 6 && strcmp(argv[1], "serve_idx") == 0)
        return do_serve_idx(argv[2], argv[3], argv[4], atoi(argv[5]));
    if (argc >= 5 && strcmp(argv[1], "read") == 0)
        return do_read(argv[2], atoi(argv[3]), atoi(argv[4]));
    fprintf(stderr, "usage:\n"
            "  %s serve     <file> <fanout> <ip> <port>\n"
            "  %s serve_idx <file> <bounds_file> <ip> <port>\n"
            "  %s read      <ip> <port> <idx>\n", argv[0], argv[0], argv[0]);
    return 2;
}
