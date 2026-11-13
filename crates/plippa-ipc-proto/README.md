# plippa-ipc-proto

Binary IPC message types and **Unix domain socket + Postcard** transport shared by
`plippa-engine`, `plippa-ui`, and `plippa-server`.

## Owns

- Message enums/structs (`IpcMessage` vocabulary)
- Length-prefixed Postcard framing (`encode_frame` / `try_pop_frame`)
- UDS helpers (`IpcListener`, `IpcConnection`) and path resolution (`resolve_paths`)

## Topology

**Engine listens** on `{PLIPPA_IPC_DIR}/engine.sock`. UI and server **connect**.

Engine broadcasts `PlaybackStatus` / `UserNotice` to all clients and relays
`AssociateRequest` / `AssociateResponse` between peers.

Full write-up: [`docs/design/ipc.md`](../../docs/design/ipc.md).

## Env

| Variable | Meaning |
|----------|---------|
| `PLIPPA_IPC_DIR` | Directory for sockets (launcher sets this) |
| `PLIPPA_IPC_ENGINE_SOCK` | Absolute override for the engine socket path |

## Invalid frames

- Bad Postcard body: error returned, frame drained, **connection stays usable**
- Length &gt; 1 MiB: error, buffer cleared, **caller should reconnect**
