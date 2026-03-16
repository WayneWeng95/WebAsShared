// MicroPython embed port configuration for wasm32-unknown-unknown.
//
// Follows micropython/examples/embedding/mpconfigport.h.
// Type sizes (mp_int_t, mp_uint_t, mp_off_t) are intentionally left to
// mpconfigport_common.h / mpconfig.h — defining them here would conflict.

// Provide alloca before the common header tries to #include <alloca.h>.
#ifndef alloca
#  define alloca __builtin_alloca
#endif
#define _ALLOCA_H

#include <port/mpconfigport_common.h>

// Start from the bare minimum; only enable what workloads.py actually needs.
#define MICROPY_CONFIG_ROM_LEVEL        (MICROPY_CONFIG_ROM_LEVEL_MINIMUM)

// Core interpreter
#define MICROPY_ENABLE_COMPILER         (1)
#define MICROPY_ENABLE_GC               (1)
#define MICROPY_PY_GC                   (1)

// Built-in types used by workloads.py
#define MICROPY_PY_BUILTINS_BYTEARRAY   (1)
#define MICROPY_PY_BUILTINS_DICT        (1)
#define MICROPY_PY_BUILTINS_LIST        (1)
#define MICROPY_PY_BUILTINS_STR         (1)
#define MICROPY_PY_BUILTINS_ENUMERATE   (1)
#define MICROPY_PY_BUILTINS_SORTED      (1)
#define MICROPY_PY_BUILTINS_MIN_MAX     (1)
#define MICROPY_PY_BUILTINS_SUM         (1)

// Disable unneeded features
#define MICROPY_PY_SYS                  (0)
#define MICROPY_PY_IO                   (0)
#define MICROPY_PY_MATH                 (0)
#define MICROPY_FLOAT_IMPL              MICROPY_FLOAT_IMPL_NONE
#define MICROPY_LONGINT_IMPL            MICROPY_LONGINT_IMPL_NONE
#define MICROPY_PY_THREAD               (0)
#define MICROPY_VFS                     (0)
