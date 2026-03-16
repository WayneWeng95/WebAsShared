// shm_module.c — MicroPython native module `shm`
//
// Exposes the SHM streaming API to Python workloads.
// All heavy-lifting (pointer arithmetic, atomics, page chains) is done
// by the Rust shm_bridge functions declared below.
//
// Python usage inside workloads.py:
//   import shm
//   records = shm.read_all_stream_records(slot)   # -> list[(origin, bytes)]
//   shm.append_stream_data(slot, b"hello")
//   shm.write_output_str("result line")
//   shm.write_output(b"\x00\x01\x02")
//   lines = shm.read_all_inputs()                  # -> list[(origin, bytes)]
//   lines = shm.read_all_inputs_from(io_slot)

#include "py/runtime.h"
#include "py/objlist.h"
#include "py/objtuple.h"
#include "py/objstr.h"
#include <stdint.h>

// ── Rust-side shm_bridge functions (defined in src/shm_bridge.rs) ────────────

// Load all stream records for `slot` into an internal Rust buffer.
// Returns the number of records loaded.
extern uint32_t rust_load_stream_records(uint32_t slot);

// Load all I/O records for `io_slot` into the same internal buffer.
extern uint32_t rust_load_io_records(uint32_t io_slot);

// Query the record at index `idx` previously loaded by rust_load_*_records.
extern uint32_t      rust_get_record_origin(uint32_t idx);
extern const uint8_t *rust_get_record_ptr(uint32_t idx);
extern uint32_t      rust_get_record_len(uint32_t idx);

// Write operations — pass data as (ptr, len).
extern void rust_append_stream_data(uint32_t slot,
                                    const uint8_t *data, uint32_t len);
extern void rust_write_output(const uint8_t *data, uint32_t len);

// ── Helper: build a Python list of (origin: int, payload: bytes) tuples ──────

static mp_obj_t build_record_list(uint32_t count) {
    mp_obj_t list = mp_obj_new_list(0, NULL);
    for (uint32_t i = 0; i < count; i++) {
        uint32_t      origin = rust_get_record_origin(i);
        const uint8_t *ptr   = rust_get_record_ptr(i);
        uint32_t      len    = rust_get_record_len(i);

        mp_obj_t items[2] = {
            mp_obj_new_int(origin),
            mp_obj_new_bytes(ptr, len),
        };
        mp_obj_list_append(list, mp_obj_new_tuple(2, items));
    }
    return list;
}

// ── shm.read_all_stream_records(slot) ─────────────────────────────────────────
STATIC mp_obj_t shm_read_all_stream_records(mp_obj_t slot_obj) {
    uint32_t slot  = (uint32_t)mp_obj_get_int(slot_obj);
    uint32_t count = rust_load_stream_records(slot);
    return build_record_list(count);
}
STATIC MP_DEFINE_CONST_FUN_OBJ_1(shm_read_all_stream_records_obj,
                                  shm_read_all_stream_records);

// ── shm.read_all_inputs() — reads from default I/O slot 0 ────────────────────
STATIC mp_obj_t shm_read_all_inputs(void) {
    uint32_t count = rust_load_io_records(0);
    return build_record_list(count);
}
STATIC MP_DEFINE_CONST_FUN_OBJ_0(shm_read_all_inputs_obj, shm_read_all_inputs);

// ── shm.read_all_inputs_from(io_slot) ────────────────────────────────────────
STATIC mp_obj_t shm_read_all_inputs_from(mp_obj_t slot_obj) {
    uint32_t slot  = (uint32_t)mp_obj_get_int(slot_obj);
    uint32_t count = rust_load_io_records(slot);
    return build_record_list(count);
}
STATIC MP_DEFINE_CONST_FUN_OBJ_1(shm_read_all_inputs_from_obj,
                                  shm_read_all_inputs_from);

// ── shm.append_stream_data(slot, data: bytes) ────────────────────────────────
STATIC mp_obj_t shm_append_stream_data(mp_obj_t slot_obj, mp_obj_t data_obj) {
    uint32_t slot = (uint32_t)mp_obj_get_int(slot_obj);
    mp_buffer_info_t buf;
    mp_get_buffer_raise(data_obj, &buf, MP_BUFFER_READ);
    rust_append_stream_data(slot, (const uint8_t *)buf.buf, (uint32_t)buf.len);
    return mp_const_none;
}
STATIC MP_DEFINE_CONST_FUN_OBJ_2(shm_append_stream_data_obj,
                                  shm_append_stream_data);

// ── shm.write_output(data: bytes) ────────────────────────────────────────────
STATIC mp_obj_t shm_write_output(mp_obj_t data_obj) {
    mp_buffer_info_t buf;
    mp_get_buffer_raise(data_obj, &buf, MP_BUFFER_READ);
    rust_write_output((const uint8_t *)buf.buf, (uint32_t)buf.len);
    return mp_const_none;
}
STATIC MP_DEFINE_CONST_FUN_OBJ_1(shm_write_output_obj, shm_write_output);

// ── shm.write_output_str(s: str) ─────────────────────────────────────────────
STATIC mp_obj_t shm_write_output_str(mp_obj_t str_obj) {
    mp_buffer_info_t buf;
    mp_get_buffer_raise(str_obj, &buf, MP_BUFFER_READ);
    rust_write_output((const uint8_t *)buf.buf, (uint32_t)buf.len);
    return mp_const_none;
}
STATIC MP_DEFINE_CONST_FUN_OBJ_1(shm_write_output_str_obj, shm_write_output_str);

// ── Module table ──────────────────────────────────────────────────────────────
STATIC const mp_rom_map_elem_t shm_module_globals_table[] = {
    { MP_ROM_QSTR(MP_QSTR___name__),
      MP_ROM_QSTR(MP_QSTR_shm) },
    { MP_ROM_QSTR(MP_QSTR_read_all_stream_records),
      MP_ROM_PTR(&shm_read_all_stream_records_obj) },
    { MP_ROM_QSTR(MP_QSTR_read_all_inputs),
      MP_ROM_PTR(&shm_read_all_inputs_obj) },
    { MP_ROM_QSTR(MP_QSTR_read_all_inputs_from),
      MP_ROM_PTR(&shm_read_all_inputs_from_obj) },
    { MP_ROM_QSTR(MP_QSTR_append_stream_data),
      MP_ROM_PTR(&shm_append_stream_data_obj) },
    { MP_ROM_QSTR(MP_QSTR_write_output),
      MP_ROM_PTR(&shm_write_output_obj) },
    { MP_ROM_QSTR(MP_QSTR_write_output_str),
      MP_ROM_PTR(&shm_write_output_str_obj) },
};
STATIC MP_DEFINE_CONST_DICT(shm_module_globals, shm_module_globals_table);

const mp_obj_module_t shm_module = {
    .base    = { &mp_type_module },
    .globals = (mp_obj_dict_t *)&shm_module_globals,
};

MP_REGISTER_MODULE(MP_QSTR_shm, shm_module);
