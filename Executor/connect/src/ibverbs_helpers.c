/*
 * Thin C wrappers that turn static-inline libibverbs functions into real
 * exported symbols callable from Rust FFI.
 *
 * ibv_post_send, ibv_poll_cq, ibv_post_recv, and ibv_query_port dispatch
 * through function pointers inside the opaque ibv_qp / ibv_cq / ibv_context
 * structs.  They are declared `static inline` in <infiniband/verbs.h> and
 * are therefore not exported by libibverbs.so.  Compiling them here forces
 * the compiler to emit real object-file symbols.
 */

#include <infiniband/verbs.h>

int wrap_ibv_query_port(struct ibv_context *ctx, uint8_t port,
                        struct ibv_port_attr *attr)
{
    return ibv_query_port(ctx, port, attr);
}

int wrap_ibv_post_send(struct ibv_qp *qp, struct ibv_send_wr *wr,
                       struct ibv_send_wr **bad_wr)
{
    return ibv_post_send(qp, wr, bad_wr);
}

int wrap_ibv_poll_cq(struct ibv_cq *cq, int ne, struct ibv_wc *wc)
{
    return ibv_poll_cq(cq, ne, wc);
}

int wrap_ibv_post_recv(struct ibv_qp *qp, struct ibv_recv_wr *wr,
                       struct ibv_recv_wr **bad_wr)
{
    return ibv_post_recv(qp, wr, bad_wr);
}
