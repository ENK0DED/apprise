"""Record upstream's Nostr signing and encryption, for `notify/nostr/`.

Usage:
  python -I gen_nostr_vectors.py > upstream_vectors.json

Runs upstream `apprise.plugins.nostr.crypto` with fixed keys, timestamps
and `os.urandom()` output, so the port can check that it builds the very
same events, ids, signatures and NIP-44 / NIP-04 / AES-GCM payloads. A
gift wrap and a NIP-04 message made with real randomness are included for
the port to open. Long payloads are kept as their SHA-256.
"""
import json
import os
import sys
from unittest import mock

from apprise.plugins.nostr import crypto
from apprise.plugins.nostr.crypto import NostrKey, bech32_encode

SENDER = NostrKey(int("11" * 32, 16))
RECIPIENT = NostrKey(int("22" * 32, 16))
OTHER = NostrKey(int("33" * 32, 16))

out = {}
out["keys"] = {
    name: {
        "pubkey": key.public_key,
        "nsec": bech32_encode("nsec", key.secret.to_bytes(32, "big")),
        "npub": bech32_encode("npub", bytes.fromhex(key.public_key)),
    }
    for name, key in (("sender", SENDER), ("recipient", RECIPIENT), ("other", OTHER))
}

AUX = bytes(range(32))


def fixed(n):
    return (AUX * 4)[:n]


events = []
cases = [
    (SENDER, 1, "Hello & <world> é \"quoted\" \\ \n\t\x01   😀", [["t", "x"], ["p", RECIPIENT.public_key]], 1700000000),
    (RECIPIENT, 10050, "", [["relay", "wss://one.example.com"]], 1700000001),
    (OTHER, 24242, "Upload Blob", [["t", "upload"], ["x", "ab" * 32], ["expiration", "1700000300"]], 1700000000),
    (SENDER, 4, "", [], 0),
]
with mock.patch("os.urandom", side_effect=fixed):
    for key, kind, content, tags, created_at in cases:
        ev = key.sign(kind, content, tags, created_at)
        events.append(
            {
                "event": ev,
                "compact": json.dumps(["EVENT", ev], separators=(",", ":"), ensure_ascii=False),
                "py_default": json.dumps(ev, ensure_ascii=False),
                "py_default_ascii": json.dumps(ev),
            }
        )
out["events"] = events

ck = SENDER.conversation_key(RECIPIENT.public_key)
out["conversation_key"] = ck.hex()
out["shared_x"] = SENDER.shared_x(RECIPIENT.public_key).hex()
nonce = bytes(range(100, 132))
out["nip44"] = [
    {"plaintext": p, "nonce": nonce.hex(), "payload": crypto.nip44_encrypt(p, ck, nonce)}
    for p in ("a", "x" * 32, "y" * 33, "é" * 200, "😀" * 1000, "z" * 70000)
]
with mock.patch("os.urandom", side_effect=fixed):
    out["nip04"] = [
        {"plaintext": p, "payload": crypto.nip04_encrypt(p, SENDER.shared_x(RECIPIENT.public_key))}
        for p in ("Hello", "", "é" * 17, "x" * 16)
    ]

with mock.patch("os.urandom", side_effect=fixed), mock.patch(
    "cryptography.hazmat.primitives.ciphers.aead.AESGCM.generate_key",
    return_value=bytes(range(200, 232)),
):
    enc, k, n = crypto.encrypt_file(b"file contents \x00\xff" * 10)
    out["file"] = {"data": (b"file contents \x00\xff" * 10).hex(), "encrypted": enc.hex(), "key": k, "nonce": n}

# Random (not fixed) gift wraps from upstream for the port to open
message = crypto.rumor(SENDER.public_key, 14, "Hi from upstream é 😀", [["p", RECIPIENT.public_key]])
out["gift_wrap"] = {"message": message, "wrap": crypto.gift_wrap(SENDER, RECIPIENT.public_key, message)}
out["nip04_random"] = {"plaintext": "random iv é", "payload": crypto.nip04_encrypt("random iv é", SENDER.shared_x(RECIPIENT.public_key))}
import hashlib

for case in out["nip44"]:
    if len(case["payload"]) > 1500:
        case["payload_sha256"] = hashlib.sha256(case["payload"].encode()).hexdigest()
        del case["payload"]
    if len(case["plaintext"]) > 100:
        case["plaintext_repeat"] = [case["plaintext"][0], len(case["plaintext"])]
        del case["plaintext"]
json.dump(out, sys.stdout, indent=1, ensure_ascii=False)
