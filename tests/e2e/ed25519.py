"""Ed25519 for the e2e scripts: PowerShell 5.1's .NET has no Ed25519, and the
server really verifies the signatures on /keys/signatures/upload.

    python ed25519.py keygen                    -> <private base64> <public base64>
    python ed25519.py sign <private> <data b64> -> signature base64, over those bytes

The data travels as base64 because a PowerShell pipe does not hand a native
program the bytes it was given (it re-encodes them, and a signature over the
wrong bytes is simply invalid).

Base64 is Matrix's: standard alphabet, no padding. Only used by tests; a run on
a machine without `cryptography` skips the checks that need it.
"""

import base64
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


def unpadded(raw: bytes) -> str:
    return base64.b64encode(raw).decode().rstrip("=")


def decode(text: str) -> bytes:
    return base64.b64decode(text + "=" * (-len(text) % 4))


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2

    if sys.argv[1] == "keygen":
        private = Ed25519PrivateKey.generate()
        public = private.public_key().public_bytes(
            encoding=serialization.Encoding.Raw, format=serialization.PublicFormat.Raw
        )
        secret = private.private_bytes(
            encoding=serialization.Encoding.Raw,
            format=serialization.PrivateFormat.Raw,
            encryption_algorithm=serialization.NoEncryption(),
        )
        print(f"{unpadded(secret)} {unpadded(public)}")
        return 0

    if sys.argv[1] == "sign" and len(sys.argv) == 4:
        private = Ed25519PrivateKey.from_private_bytes(decode(sys.argv[2]))
        print(unpadded(private.sign(decode(sys.argv[3]))))
        return 0

    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
