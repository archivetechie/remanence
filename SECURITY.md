# Security Policy

Remanence stores archives for the long term, and some stored copies are
encrypted. Security reports are taken seriously — especially anything
touching:

- the encryption envelope (key wrapping, key derivation, AEAD framing,
  nonce handling, the key frame parser);
- integrity guarantees (digest verification, parity recovery, the
  verification chain from stored bytes to per-file hashes);
- parsing of hostile media (tar/pax parsing, manifest CBOR, headers —
  anything a crafted tape or object file could exploit in a reader).

## Reporting

Email **specs@archivetech.org** with the details. Please do not open a
public GitHub issue for a suspected vulnerability. You can expect an
acknowledgment within a few days.

There is currently no bug bounty; credit is given in the changelog unless
you prefer otherwise.

## Scope notes

- The project is pre-production (no stable release yet); there are no
  supported-version windows — reports are assessed against the current
  `main`.
- The published specifications have their own security-considerations
  sections. Ambiguities or weaknesses in the *specifications* are security
  reports too, and in some ways the more important kind: implementations
  can be patched, published formats are forever.
