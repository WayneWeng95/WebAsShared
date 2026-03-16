// Minimal alloca.h shim for wasm32-unknown-unknown (-nostdlib).
// clang provides __builtin_alloca as a compiler intrinsic on all targets.
#pragma once
#ifndef alloca
#  define alloca __builtin_alloca
#endif
