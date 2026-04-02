"""Python equivalents of the Rust routing-test guest functions.

produce_stream(slot)        — write 3 labeled records to stream slot
produce_stream_heavy(slot)  — write 150 structured records to stream slot
count_stream_records(slot)  — return record count (delegates to shm module)
"""

import shm


def produce_stream(slot: int) -> None:
    for seq in range(3):
        payload = f"StreamPayload_P{slot}_seq{seq}"
        shm.append_stream_data(slot, payload.encode("utf-8"))


def produce_stream_heavy(slot: int) -> None:
    for seq in range(150):
        payload = f"p={slot},s={seq:04},DATA_BLOCK_{seq:04}_XXXXXXXXXXXXXXXXXXXXXXXXXXX"
        shm.append_stream_data(slot, payload.encode("utf-8"))


def count_stream_records(slot: int) -> int:
    return shm.count_stream_records(slot)
