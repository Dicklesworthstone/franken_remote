# Tailscale Sharing and Membership Fixture Matrix

This directory contains recorded and structured fixture projections for the Phase 0
Tailscale identity qualification gate (`fr-p0-tailnet-identity-q0h`) and the production
admission adapter (`fr-p1-fr-tailnet-602`).

## Matrix Rows

| Fixture | Host User / Tags | Peer User / Tags | Peer Markers | Scope::OwnUser | Scope::Tailnet | Verified Refusal / Grant |
|---|---|---|---|---|---|---|
| `positive_approved` | User 7 | User 7 | `MachineAuthorized: true` | Admitted | Admitted | Positive machine authorization + valid app grant |
| `different_owner` | User 7 | User 8 | `MachineAuthorized: true` | Refused | Admitted | `ScopeDenied` on own-user, admitted on tailnet scope |
| `tagged_host` | User 0, `tag:fr-host` | User 7 | `MachineAuthorized: true` | Refused | Admitted | `ExplicitScopeRequired` (tagged host has no user) |
| `tagged_peer` | User 7 | User 0, `tag:fr-client` | `MachineAuthorized: true` | Refused | Admitted | `ScopeDenied` on own-user, admitted on tailnet scope |
| `shared_in` | User 7 | User 11 | `ShareeNode: true`, `Sharer: 11` | Refused | Refused | `SharedPeer` (shared-in nodes strictly refused) |
| `multi_tailnet` | User 7, `fixture.invalid` | User 7, other tailnet | foreign key/IPs | Refused | Refused | `IdentityMismatch` (cross-tailnet not admitted) |

## Normative Verification Rules

1. `MachineAuthorized` is required and must be `Some(true)`. Absence or null returns `TailnetMembershipUnverifiable`.
2. Own-user default scope requires `Status.Self.UserID == WhoIs.Node.User`, and neither party may have `Tags`.
3. Tagged hosts must explicitly configure `Scope::Tailnet`; attempting `Scope::OwnUser` returns `ExplicitScopeRequired`.
4. Shared-in nodes (`ShareeNode`, `AltSharerUserID`, `Sharer`) are refused unconditionally as `SharedPeer`.
5. Subnet-routed or cross-tailnet IP addresses that do not match the exact node-owned addresses (`/32` or `/128`) are refused as `AddressMismatch` or `IdentityMismatch`.
