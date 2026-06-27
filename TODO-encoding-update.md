# Encoding update TODO

## Direction

- Use vendored `senax-encoder` as rho's DB value encoding base.
- Keep redb key encoding per-key:
  - custom/order-preserving encodings for range or prefix keys,
  - senax pack-style raw encodings are fine for exact-lookup keys.
- Treat `Encode` as the evolvable format and `Pack` as raw compact data.

## Changes to make later

- Remove pack-format structure hashes.
  - `Pack` should not write a named-struct hash.
  - `Unpack` should not validate a named-struct hash.
  - Named struct pack should be just fields in declaration order.
  - Named enum variant pack should be variant id plus fields in declaration
    order.
  - If a caller wants an id/version/checksum, it should be in an explicit
    wrapper type.
- Revisit tuple/unnamed struct pack field counts.
  - If `Pack` is raw data, counts may also be wrapper-level policy rather than
    intrinsic struct data.
- Update `pack-specification.md` after the pack format is simplified.
- Add small golden-byte tests for rho's chosen key/value encodings before using
  them in the DB.

## Compatibility policy

- rho primarily cares about forward compatibility: newer rho should read older
  rho DBs.
- Older rho reading newer DBs is not a hard requirement.
- Missing fields in evolvable records should use `Option` or `#[senax(default)]`.
- Dense/frozen payloads may use raw pack when the lifecycle is controlled.
