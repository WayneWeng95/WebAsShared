// setjmp.h shim for wasm32-unknown-unknown.
//
// wasm32 has no OS-provided setjmp; clang exposes it as compiler builtins.
// LLVM's __builtin_setjmp stores {frame-ptr, stack-ptr, return-addr} —
// 3 pointers minimum.  We allocate 5 uint32_t words for safety padding.
//
// Constraints:
//   - __builtin_longjmp second argument MUST be 1 (LLVM requirement).
//   - jmp_buf must be naturally aligned (pointer-aligned = 4 bytes on wasm32).
#pragma once
#include <stdint.h>

typedef uint32_t jmp_buf[5];

// Map standard names to compiler builtins.
#define setjmp(env)       __builtin_setjmp((void **)(env))
#define longjmp(env, val) __builtin_longjmp((void **)(env), 1)
