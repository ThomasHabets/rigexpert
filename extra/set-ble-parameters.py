#!/usr/bin/env python3
"""Set temporary kernel connection parameters for this AA-650 only."""
import ctypes
import os
import socket
import struct
import time

address = bytes.fromhex('049162AEBAED')[::-1]
# BlueZ MGMT Load Connection Parameters (0x0035): one LE public peer,
# interval 30–50 ms, zero peripheral latency, supervision timeout 4 seconds.
payload = struct.pack('<H6sBHHHH', 1, address, 1, 24, 40, 0, 400)
command = struct.pack('<HHH', 0x0035, 0, len(payload)) + payload
with socket.socket(socket.AF_BLUETOOTH, socket.SOCK_RAW, socket.BTPROTO_HCI) as management:
    # Python's HCI address parser only accepts the device ID on some versions.
    # Use the Linux sockaddr_hci layout to select HCI_CHANNEL_CONTROL explicitly.
    native = ctypes.CDLL(None, use_errno=True)
    native.bind.argtypes = (ctypes.c_int, ctypes.c_void_p, ctypes.c_uint)
    native.bind.restype = ctypes.c_int
    address_buffer = ctypes.create_string_buffer(struct.pack('HHH', socket.AF_BLUETOOTH, 0xffff, 3))
    if native.bind(management.fileno(), address_buffer, 6) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    management.settimeout(5)
    management.sendall(command)
    deadline = time.monotonic() + 5
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SystemExit('Timed out waiting for the Bluetooth management response')
        management.settimeout(remaining)
        packet = management.recv(4096)
        if len(packet) < 9:
            continue
        event, index, length = struct.unpack_from('<HHH', packet)
        if length != len(packet) - 6:
            continue
        opcode, status = struct.unpack_from('<HB', packet, 6)
        if event not in (1, 2) or index != 0 or opcode != 0x0035:
            continue
        if status:
            raise SystemExit(f'BlueZ MGMT rejected the parameters: status 0x{status:02x}')
        print('AA-650 connection parameters loaded: 30–50 ms interval, 4 s supervision timeout.')
        break
