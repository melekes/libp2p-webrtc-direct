# libp2p-webrtc-direct

WebRTC transport for libp2p.

┌───────────────┐  ┌───────────────┐
│               │  │               │
│ TcpTransport  │  │ UdpTransport  │
│               │  │               │
└────────────┬──┘  └────┬──────────┘
             │          │
             │          │
         ┌───▼──────────▼──┐
         │                 │
         │ WebRTCTransport │
         │                 │
         └─────────────────┘

TODO:

- [ ] Transport interface
- [ ] UDP transport
