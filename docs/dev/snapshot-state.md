# Snapshot-Only State Dump

## Overview

Anvil provides two mechanisms for persisting EVM state to disk:

| Method | Includes block history | Use case |
|---|---|---|
| `anvil_dumpState` | ✅ Yes | Full state export with chain history |
| `anvil_dumpStateSnapshot` | ❌ No | Fast export of the latest state only |

The snapshot-only dump is significantly smaller and faster to produce and restore when you only
need the latest account and contract state (balances, nonces, bytecode, storage slots) and do not
require historical blocks, transactions, or rollback capability.

---

## Anvil JSON-RPC: `anvil_dumpStateSnapshot`

Serializes only the current chain state (accounts, storage, block environment) into a
gzip-compressed JSON blob. Block history, transaction history, and historical EVM state snapshots
are **not** included.

### Request

```json
{
  "jsonrpc": "2.0",
  "method": "anvil_dumpStateSnapshot",
  "params": [],
  "id": 1
}
```

### Response

The result is a `0x`-prefixed hex-encoded, gzip-compressed JSON blob (same format as
`anvil_dumpState`).

```json
{
  "jsonrpc": "2.0",
  "result": "0x1f8b...",
  "id": 1
}
```

### Loading the snapshot

A snapshot produced by `anvil_dumpStateSnapshot` can be loaded with `anvil_loadState` or by
starting Anvil with `--load-state`:

```sh
# Save snapshot
curl -s -X POST http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"anvil_dumpStateSnapshot","params":[],"id":1}' \
  | jq -r .result > snapshot.hex

# Start a fresh node pre-loaded with the snapshot
anvil --load-state snapshot.hex
```

### JavaScript / viem example

```typescript
import { createPublicClient, http, toHex } from "viem";
import { foundry } from "viem/chains";
import { writeFileSync, readFileSync } from "fs";

const client = createPublicClient({ chain: foundry, transport: http() });

// Dump only the latest snapshot (no chain history)
const snapshot = await client.request({
  method: "anvil_dumpStateSnapshot",
  params: [],
});
writeFileSync("snapshot.gz.hex", snapshot);

// Restore on a different node
const raw = readFileSync("snapshot.gz.hex", "utf8");
await client.request({ method: "anvil_loadState", params: [raw] });
```

---

## Cheatcodes: `snapshotStateToFile` / `loadSnapshotFromFile`

Two new cheatcodes complement the RPC method, enabling Forge tests to persist and restore the
complete EVM state (including the journaled state) between test runs or test files.

### `snapshotStateToFile(string pathToSnapshot)`

Captures the entire in-test EVM state — account balances, nonces, contract code, storage, and the
current block environment — and writes it to `pathToSnapshot` as a gzip-compressed JSON file.

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";

contract MyTest is Test {
    Counter counter;

    function setUp() public {
        counter = new Counter();
        counter.increment(); // value is now 1
    }

    function test_SaveState() public {
        string memory path = string.concat(vm.projectRoot(), "/state.json.gz");
        vm.snapshotStateToFile(path);
        // state.json.gz now contains a full snapshot
    }
}
```

The written file is gzip-compressed JSON and carries the `0x1f 0x8b` magic bytes.

### `loadSnapshotFromFile(string pathToSnapshot)`

Restores the EVM to a previously saved snapshot. The cheatcode transparently handles:

- **Gzip-compressed** `PersistedStateSnapshot` files (written by `snapshotStateToFile`)
- **Gzip-compressed** `CompatibleStateSnapshot` files (written by `anvil_dumpStateSnapshot`)
- **Plain JSON** variants of both formats

This allows tests to load snapshots that were captured either in another Forge test or via
the Anvil JSON-RPC API.

```solidity
function test_LoadState() public {
    string memory path = string.concat(vm.projectRoot(), "/state.json.gz");
    vm.loadSnapshotFromFile(path);
    // EVM is now restored; call contracts as if setUp() had just run
    assertEq(counter.value(), 1);
}
```

### Round-trip example

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";

contract RoundTripTest is Test {
    Storage store;

    function setUp() public {
        store = new Storage();
        store.slot0 = 10;
        store.slot1 = 20;
    }

    function test_RoundTrip() public {
        string memory path = string.concat(vm.projectRoot(), "/snapshot.json.gz");

        // capture state
        vm.snapshotStateToFile(path);

        // mutate state
        store.slot0 = 300;
        vm.warp(1337);

        // restore
        vm.loadSnapshotFromFile(path);

        assertEq(store.slot0, 10);
        assertEq(block.timestamp, 1); // restored
    }
}
```

---

## File format

The snapshot file is a gzip-compressed JSON document. The top-level fields are:

| Field | Type | Description |
|---|---|---|
| `block` | object | Block environment (`number`, `timestamp`, `basefee`, …) |
| `accounts` | object | Map of `address → { nonce, balance, code, storage }` |
| `best_block_number` | uint / hex string | Block number of the snapshot |
| `blocks` | array | **Always empty** for snapshot-only dumps |
| `transactions` | array | **Always empty** for snapshot-only dumps |
| `historical_states` | null | **Always null** for snapshot-only dumps |
| `foundry_snapshot` | object | Present only when written by `snapshotStateToFile`; carries internal Foundry EVM state required for in-test restoration |

The `foundry_snapshot` field is omitted when a snapshot is produced via `anvil_dumpStateSnapshot`.
Such files are still valid for `loadSnapshotFromFile`; Foundry reconstructs the EVM state from the
`accounts` and `block` fields.
