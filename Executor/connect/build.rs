// Compile C wrappers for libibverbs inline functions and link the library.
//
// Several key libibverbs functions (ibv_post_send, ibv_poll_cq,
// ibv_post_recv, ibv_query_port) are declared `static inline` in
// <infiniband/verbs.h> — they dispatch through function pointers stored
// in the ibv_qp / ibv_cq / ibv_context structs rather than being exported
// symbols.  We export them as real C symbols via src/ibverbs_helpers.c so
// they are callable from Rust FFI.

fn main() {
    println!("cargo:rustc-link-lib=ibverbs");
    println!("cargo:rerun-if-changed=src/ibverbs_helpers.c");

    cc::Build::new()
        .file("src/ibverbs_helpers.c")
        .compile("ibverbs_helpers");
}
