# mint_license

Offline minting tool for Hobbes Pro license keys. Standalone cargo project —
**not** a member of the repo's root workspace; build and run it from this
directory.

## License format

```
HOBBES-PRO.<base64url(payload_json)>.<base64url(ed25519_signature)>
```

- `payload_json` — `{"email": "...", "issued_at": "<rfc3339 utc>", "product": "pro"}`
- signature — ed25519 over the exact payload bytes embedded in the key
- base64url is unpadded (`URL_SAFE_NO_PAD`)

The app verifies with the public key constant `EMBEDDED_PUBLIC_KEY_B64` in
`src/entitlement.rs`.

## Usage

### 1. Generate the keypair (once)

```bash
cd scripts/mint_license
cargo run -- keygen
```

Writes `keys/license_signing.key` (private, gitignored — keep it offline and
back it up somewhere safe) and `keys/license_signing.pub`, then prints the
`EMBEDDED_PUBLIC_KEY_B64` constant to paste into `src/entitlement.rs`.

`keygen` refuses to overwrite an existing key; `keygen --force` regenerates.
Regenerating invalidates every previously minted license once the app ships
with the new public key.

### 2. Mint a license

```bash
cargo run -- mint --email customer@example.com
```

Prints the license string to stdout. Send it to the customer; they paste it in
**Settings → About → License**.

## Key rotation

1. `cargo run -- keygen --force`
2. Paste the newly printed `EMBEDDED_PUBLIC_KEY_B64` into `src/entitlement.rs`
3. Re-mint and re-issue licenses for existing customers
