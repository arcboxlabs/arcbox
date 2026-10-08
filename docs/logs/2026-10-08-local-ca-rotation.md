# Local CA rotation and recovery

## Scope

Root rotation must retain the current identity until the replacement has native trust. The daemon creates private keys. The login-session CLI authorizes the replacement and revokes the exact previous certificate. The daemon then publishes the replacement and removes the retired identity.

The host and guest protocol remains 7/7. These changes add no old-version negotiation or compatibility branch.

## Durable state

The rotation journal records `stable`, `preparing`, `prepared`, and `committing`. Each recorded root contains a Keychain slot and public DER. The two slots separate private keys and avoid ambiguous certificate lookup by public-key hash. The journal retains the previous DER until cleanup finishes.

Only `preparing` may create a candidate key or certificate. A malformed journal must fail without changing the selected identity. A durability error must stop cleanup; recovery must persist the commit record successfully before deleting the previous key.

The initial journal commit validates persistence and rejected state transitions. Keychain integration, CLI orchestration, Desktop actions, and native rotation acceptance are subsequent steps; that commit does not expose a rotation command.

## Validation

The crate tests cover reloads at each journal phase, retention of the previous identity during cleanup, malformed records, duplicate identities, and invalid target slots. Native trust and browser acceptance are not covered by these tests.

## Certificate lifetime and paired code generation

`certificate_state` reports missing, valid, renewal due within 30 days, expired, or not yet valid independently of trust. Unknown means inspection has not completed or failed. Desktop disables ordinary trust for inactive roots and directs a not-yet-valid root to the Mac's date and time. Browser observation stops while the root is inactive and starts a new cycle when the same root becomes valid.

Regenerating the complete paired Swift client exposed `MachineService.Import` as an unescaped Swift keyword. The RPC is named `ImportMachine`, matching its request and response types. The runtime and clients change together. `generate.sh --local` reads `ARCBOX_DIR` so code generation uses the same checkout as the runnable bundle.

## Mutation ordering

The CLI and daemon use the same per-profile file lock for mutations. The CLI holds the lock through native trust changes. Daemon preparation holds the lock through publication and signer activation. Uninstall holds the lock across revocation and identity deletion, then removes the lock directory through its inventory.
