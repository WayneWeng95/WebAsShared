// build.rs — compile MicroPython embed port + shm_module C sources.
//
// Prerequisites (run once before building):
//   git clone https://github.com/micropython/micropython.git  \
//       py_guest/micropython
//   cd py_guest/micropython && make -C mpy-cross   # build bytecode compiler
//
// MicroPython embed port lives at micropython/ports/embed.
// We compile only the files needed for a minimal interpreter with no OS I/O.

use std::{env, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let mp_root  = manifest.join("micropython");
    let mp_embed = mp_root.join("ports/embed");
    let c_src    = manifest.join("c_src");

    // ── Abort with a clear message if MicroPython is not cloned yet ──────────
    if !mp_root.exists() {
        panic!(
            "\n\n[py_guest] MicroPython source not found at {mp_root:?}.\n\
             Run:\n  git clone https://github.com/micropython/micropython.git \
             py_guest/micropython\n\
             then rebuild.\n"
        );
    }

    // ── Include paths used by all C sources ───────────────────────────────────
    let includes = [
        mp_root.to_str().unwrap(),              // micropython/
        mp_embed.to_str().unwrap(),             // micropython/ports/embed/
        c_src.to_str().unwrap(),                // c_src/  (mpconfigport.h lives here)
    ];

    // ── MicroPython embed port C files ────────────────────────────────────────
    // The embed port is a pre-selected minimal subset; see ports/embed/Makefile.
    let mp_srcs: Vec<PathBuf> = [
        "py/mpstate.c", "py/nlr.c", "py/nlrx86.c", "py/nlrsetjmp.c",
        "py/malloc.c", "py/gc.c", "py/pystack.c",
        "py/qstr.c", "py/vstr.c", "py/mpprint.c", "py/unicode.c",
        "py/mpz.c", "py/reader.c", "py/lexer.c", "py/parse.c",
        "py/scope.c", "py/compile.c", "py/emitcommon.c", "py/emitbc.c",
        "py/asmbase.c", "py/asmx64.c", "py/asmx86.c", "py/asmthumb.c",
        "py/asmarm.c", "py/asmxtensa.c", "py/asmrv32.c",
        "py/emitinlinethumb.c", "py/emitinlinextensa.c", "py/emitinlinerv32.c",
        "py/formatfloat.c", "py/parsenumbase.c", "py/parsenum.c",
        "py/emitglue.c", "py/persistentcode.c",
        "py/runtime.c", "py/runtime_utils.c", "py/scheduler.c",
        "py/nativeglue.c", "py/pairheap.c", "py/ringbuf.c",
        "py/stackctrl.c", "py/argcheck.c", "py/warning.c", "py/profile.c",
        "py/map.c", "py/obj.c", "py/objarray.c", "py/objattrtuple.c",
        "py/objbool.c", "py/objboundmeth.c", "py/objcell.c",
        "py/objclosure.c", "py/objcomplex.c", "py/objdeque.c",
        "py/objdict.c", "py/objenumerate.c", "py/objexcept.c",
        "py/objfilter.c", "py/objfloat.c", "py/objfun.c", "py/objgenerator.c",
        "py/objgetitemiter.c", "py/objint.c", "py/objint_longlong.c",
        "py/objint_mpz.c", "py/objlist.c", "py/objmap.c", "py/objmodule.c",
        "py/objobject.c", "py/objpolyiter.c", "py/objproperty.c",
        "py/objnone.c", "py/objnamedtuple.c", "py/objrange.c",
        "py/objreversed.c", "py/objset.c", "py/objsingleton.c",
        "py/objslice.c", "py/objstr.c", "py/objstrunicode.c",
        "py/objstringio.c", "py/objtuple.c", "py/objtype.c", "py/objzip.c",
        "py/opmethods.c", "py/sequence.c", "py/stream.c", "py/binary.c",
        "py/builtinimport.c", "py/builtinevex.c", "py/builtinhelp.c",
        "py/modarray.c", "py/modbuiltins.c", "py/modcollections.c",
        "py/modgc.c", "py/modio.c", "py/modmath.c", "py/modcmath.c",
        "py/modmicropython.c", "py/modstruct.c", "py/modsys.c",
        "py/moduerrno.c", "py/moduselect.c", "py/modthread.c",
        "extmod/modubinascii.c", "extmod/moducryptolib.c",
        "extmod/moductypes.c", "extmod/moduhashlib.c",
        "extmod/moduheapq.c", "extmod/modujson.c", "extmod/moduos.c",
        "extmod/moduplatform.c", "extmod/modurandom.c", "extmod/modure.c",
        "extmod/moduselect.c", "extmod/modusocket.c", "extmod/modussl.c",
        "extmod/modutime.c", "extmod/moduwebsocket.c", "extmod/moduasyncio.c",
        "extmod/modwebrepl.c", "extmod/modframebuf.c",
        "extmod/vfs.c", "extmod/vfs_blockdev.c", "extmod/vfs_reader.c",
        "extmod/vfs_posix.c", "extmod/vfs_posix_file.c",
        "extmod/vfs_fat.c", "extmod/vfs_fat_diskio.c", "extmod/vfs_fat_file.c",
        "extmod/vfs_lfs.c",
        "shared/libc/abort_.c", "shared/libc/printf.c",
        "ports/embed/port/micropython_embed.c",
        "ports/embed/port/mphalport.c",
    ]
    .iter()
    .map(|p| mp_root.join(p))
    .filter(|p| p.exists())   // skip optional files absent in some versions
    .collect();

    // ── Compile ───────────────────────────────────────────────────────────────
    let mut build = cc::Build::new();
    build
        .files(mp_srcs)
        .file(c_src.join("shm_module.c"))
        .includes(&includes)
        .define("MICROPY_CONFIG_FILE", "\"mpconfigport.h\"")
        .define("MICROPY_PY_SYS_PLATFORM", "\"wasm\"")
        .flag("-Wno-unused-parameter")
        .flag("-Wno-unused-variable")
        .flag("-Wno-missing-field-initializers")
        .flag("-Wno-implicit-function-declaration");

    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        // wasi-libc provides string.h, stdlib.h, setjmp.h, etc. for wasm32.
        // Headers are in /usr/include/wasm32-wasi; the static lib is linked below.
        build
            .flag("-isystem/usr/include/wasm32-wasi")
            .flag("-fno-exceptions")
            .define("MP_ENDIANNESS_LITTLE", "1");

        // Link against wasi-libc so MicroPython's use of memcpy/memset/etc. resolves.
        println!("cargo:rustc-link-search=native=/usr/lib/wasm32-wasi");
        println!("cargo:rustc-link-lib=static=c");
    }

    build.compile("micropython");

    // Rebuild if any C source or Python script changes
    println!("cargo:rerun-if-changed=c_src/shm_module.c");
    println!("cargo:rerun-if-changed=c_src/mpconfigport.h");
    println!("cargo:rerun-if-changed=python/workloads.py");
}
