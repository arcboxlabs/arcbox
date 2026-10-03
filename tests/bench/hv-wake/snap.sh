#!/bin/zsh
# Usage: snap.sh <grpc-socket> -> JSON {kick,unpark,kicks_received,kicks_v0,vsock_irqs,vsock_rx_used}
set -eu
SOCK=$1
PROTO=$(cd "$(dirname "$0")/../../.." && pwd)/rpc/arcbox-protocol/proto
grpcurl -plaintext -unix -import-path "$PROTO" -proto api.proto "$SOCK" arcbox.v1.SystemService/GetVirtioDebug \
 | jq -c '{kick:(.kickBroadcasts//"0"|tonumber), unpark:(.unparkBroadcasts//"0"|tonumber),
           kicks_received:([.vcpus[]?.kicksReceived//"0"|tonumber]|add//0),
           kicks_v0:([.vcpus[]?|select((.vcpu//0)==0)|.kicksReceived//"0"|tonumber]|add//0),
           vsock_irqs:([.devices[]?|select(.deviceType=="VirtioVsock")|.interrupts//"0"|tonumber]|add//0),
           vsock_rx_used:([.devices[]?|select(.deviceType=="VirtioVsock")|.queues[]?|select((.index//0)==0)|.usedIdx//0|tonumber]|add//0)}'
