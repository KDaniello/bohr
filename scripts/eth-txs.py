#!/usr/bin/env python3
"""Ethereum transfers signed offline, for an environment's workload, with no library beyond Python.

An environment's load sends transactions it never signs: they are made once, here, from keys that
are a function of their index, and written one base64 line each, as `[workload] txs` reads them.
A transfer is an EIP-155 legacy transaction, signed with secp256k1 and an RFC 6979 nonce, so the
same arguments always make the same bytes.

Usage:
  scripts/eth-txs.py genesis CHAIN_ID ACCOUNTS > genesis.json
  scripts/eth-txs.py txs CHAIN_ID ACCOUNTS COUNT GAS_PRICE_WEI > txs
  scripts/eth-txs.py check

`check` runs the known answers: Keccak-256 of nothing, the address of key 1, and EIP-155's own
example transaction.
"""
from __future__ import annotations

import base64
import hashlib
import hmac
import json
import sys
from typing import NamedTuple, Union

# Keccak-256, the pre-standard padding Ethereum kept: SHA3-256 of hashlib pads differently.
_RC = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808A, 0x8000000080008000,
    0x000000000000808B, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
    0x000000000000008A, 0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
    0x000000008000808B, 0x800000000000008B, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x000000000000800A, 0x800000008000000A,
    0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
]
_ROT = [
    [0, 36, 3, 41, 18], [1, 44, 10, 45, 2], [62, 6, 43, 15, 61], [28, 55, 25, 21, 56],
    [27, 20, 39, 8, 14],
]
_MASK = (1 << 64) - 1


def _rol(x: int, n: int) -> int:
    return ((x << n) | (x >> (64 - n))) & _MASK if n else x


def _keccak_f(a: list[list[int]]) -> list[list[int]]:
    for rc in _RC:
        c = [a[x][0] ^ a[x][1] ^ a[x][2] ^ a[x][3] ^ a[x][4] for x in range(5)]
        d = [c[(x - 1) % 5] ^ _rol(c[(x + 1) % 5], 1) for x in range(5)]
        a = [[a[x][y] ^ d[x] for y in range(5)] for x in range(5)]
        b = [[0] * 5 for _ in range(5)]
        for x in range(5):
            for y in range(5):
                b[y][(2 * x + 3 * y) % 5] = _rol(a[x][y], _ROT[x][y])
        a = [[b[x][y] ^ (~b[(x + 1) % 5][y] & b[(x + 2) % 5][y]) for y in range(5)]
             for x in range(5)]
        a[0][0] ^= rc
    return a


def keccak256(data: bytes) -> bytes:
    return _sponge(data, 0x01)


def _sponge(data: bytes, domain: int) -> bytes:
    """Keccak[512] over `data`: domain 0x01 is Keccak-256, 0x06 SHA3-256, which hashlib checks."""
    rate = 136
    padded = bytearray(data) + bytes([domain])
    padded += b'\x00' * (-len(padded) % rate)
    padded[-1] |= 0x80
    a = [[0] * 5 for _ in range(5)]
    for block in range(0, len(padded), rate):
        for i in range(rate // 8):
            x, y = i % 5, i // 5
            a[x][y] ^= int.from_bytes(padded[block + 8 * i:block + 8 * i + 8], 'little')
        a = _keccak_f(a)
    out = b''
    for i in range(4):
        out += a[i % 5][i // 5].to_bytes(8, 'little')
    return out


# secp256k1, affine arithmetic: slow and enough for a few thousand signatures.
P = 2**256 - 2**32 - 977
N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
G = (0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798,
     0x483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8)

Point = Union[tuple[int, int], None]


def _add(p: Point, q: Point) -> Point:
    if p is None:
        return q
    if q is None:
        return p
    if p[0] == q[0] and (p[1] + q[1]) % P == 0:
        return None
    if p == q:
        m = 3 * p[0] * p[0] * pow(2 * p[1], -1, P) % P
    else:
        m = (q[1] - p[1]) * pow(q[0] - p[0], -1, P) % P
    x = (m * m - p[0] - q[0]) % P
    return (x, (m * (p[0] - x) - p[1]) % P)


def _mul(k: int, p: Point = G) -> Point:
    r: Point = None
    while k:
        if k & 1:
            r = _add(r, p)
        p = _add(p, p)
        k >>= 1
    return r


def _rfc6979(key: int, digest: bytes) -> int:
    x = key.to_bytes(32, 'big')
    h = (int.from_bytes(digest, 'big') % N).to_bytes(32, 'big')
    v, k = b'\x01' * 32, b'\x00' * 32
    k = hmac.new(k, v + b'\x00' + x + h, hashlib.sha256).digest()
    v = hmac.new(k, v, hashlib.sha256).digest()
    k = hmac.new(k, v + b'\x01' + x + h, hashlib.sha256).digest()
    v = hmac.new(k, v, hashlib.sha256).digest()
    while True:
        v = hmac.new(k, v, hashlib.sha256).digest()
        t = int.from_bytes(v, 'big')
        if 1 <= t < N:
            return t
        k = hmac.new(k, v + b'\x00', hashlib.sha256).digest()
        v = hmac.new(k, v, hashlib.sha256).digest()


def sign(key: int, digest: bytes) -> tuple[int, int, int]:
    """(recovery id, r, s), with s in the lower half as EIP-2 requires."""
    z = int.from_bytes(digest, 'big')
    k = _rfc6979(key, digest)
    point = _mul(k)
    if point is None:
        raise AssertionError('k is never zero')
    r = point[0] % N
    s = pow(k, -1, N) * (z + r * key) % N
    rec = point[1] & 1
    if s > N // 2:
        s, rec = N - s, rec ^ 1
    return rec, r, s


def address(key: int) -> bytes:
    pub = _mul(key)
    if pub is None:
        raise AssertionError('a key is never zero')
    return keccak256(pub[0].to_bytes(32, 'big') + pub[1].to_bytes(32, 'big'))[12:]


# RLP, for what a legacy transaction holds: byte strings and lists of them.
_STRING, _LIST = 0x80, 0xC0
_SHORT = 56


def _rlp(item: bytes | list[bytes]) -> bytes:
    if isinstance(item, list):
        body = b''.join(_rlp(i) for i in item)
        return _length(len(body), _LIST) + body
    if len(item) == 1 and item[0] < _STRING:
        return item
    return _length(len(item), _STRING) + item


def _length(n: int, offset: int) -> bytes:
    if n < _SHORT:
        return bytes([offset + n])
    raw = n.to_bytes((n.bit_length() + 7) // 8, 'big')
    return bytes([offset + 55 + len(raw)]) + raw


def _int(n: int) -> bytes:
    return n.to_bytes((n.bit_length() + 7) // 8, 'big') if n else b''


class Transfer(NamedTuple):
    """A legacy transaction moving value, as EIP-155 signs it."""

    nonce: int
    gas_price: int
    gas: int
    to: bytes
    value: int
    chain_id: int


def transfer(key: int, tx: Transfer) -> bytes:
    fields = [_int(tx.nonce), _int(tx.gas_price), _int(tx.gas), tx.to, _int(tx.value), b'']
    digest = keccak256(_rlp([*fields, _int(tx.chain_id), b'', b'']))
    rec, r, s = sign(key, digest)
    return _rlp([*fields, _int(tx.chain_id * 2 + 35 + rec), _int(r), _int(s)])


def key_of(index: int) -> int:
    """The key of account `index`: a function of the index, so the state can be built again."""
    return int.from_bytes(keccak256(b'bohr account %d' % index), 'big') % (N - 1) + 1


def genesis(chain_id: int, accounts: int) -> dict[str, object]:
    """A chain merged from its first block and never past it: Paris at genesis and no later fork,
    so the engine API's first version drives it. Dated 2025-01-01, where the world's clock starts.
    """
    zero32 = '0x' + '00' * 32
    forks = ['homesteadBlock', 'eip150Block', 'eip155Block', 'eip158Block', 'byzantiumBlock',
             'constantinopleBlock', 'petersburgBlock', 'istanbulBlock', 'berlinBlock',
             'londonBlock', 'mergeNetsplitBlock']
    config: dict[str, object] = {'chainId': chain_id, 'terminalTotalDifficulty': 0,
                                 'terminalTotalDifficultyPassed': True}
    config.update(dict.fromkeys(forks, 0))
    return {
        'config': config,
        'nonce': '0x0',
        'timestamp': hex(1735689600),
        'extraData': '0x',
        'gasLimit': hex(30_000_000),
        'difficulty': '0x0',
        'mixHash': zero32,
        'coinbase': '0x' + '00' * 20,
        'baseFeePerGas': hex(10**9),
        'alloc': {'0x' + address(key_of(i)).hex(): {'balance': hex(10**24)}
                  for i in range(accounts)},
    }


def check() -> None:
    assert keccak256(b'').hex() == \
        'c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470'
    # The same sponge with SHA-3's domain, against hashlib, at every length a block boundary
    # touches: a padding of one byte, of a whole block, and of none.
    for n in range(300):
        data = bytes(range(256))[: n % 256] * (n // 256 + 1)
        data = data[:n]
        assert _sponge(data, 0x06) == hashlib.sha3_256(data).digest(), n
    assert address(1).hex() == '7e5f4552091a69125d5dfcb7b8c2659029395bdf'
    eip155 = transfer(int('46' * 32, 16),
                      Transfer(9, 20 * 10**9, 21000, bytes.fromhex('35' * 20), 10**18, 1))
    assert eip155.hex() == (
        'f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a7640000'
        '8025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f'
        '761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83'), eip155.hex()
    print('ok: keccak256, address, EIP-155')


def main(argv: list[str]) -> int:
    if argv[:1] == ['check']:
        check()
    elif argv[:1] == ['genesis']:
        chain_id, accounts = int(argv[1]), int(argv[2])
        print(json.dumps(genesis(chain_id, accounts), indent=1, sort_keys=True))
    elif argv[:1] == ['txs']:
        chain_id, accounts, count, gas_price = (int(a) for a in argv[1:5])
        keys = [key_of(i) for i in range(accounts)]
        addresses = [address(k) for k in keys]
        nonces = [0] * accounts
        for t in range(count):
            sender, receiver = t % accounts, (t * 7 + 1) % accounts
            raw = transfer(keys[sender], Transfer(nonces[sender], gas_price, 21000,
                                                  addresses[receiver], 10**15 + t, chain_id))
            nonces[sender] += 1
            print(base64.b64encode(raw).decode())
    else:
        print(__doc__, file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
