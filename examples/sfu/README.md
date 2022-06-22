# SFU

This is a simplified example of how you may build an SFU (Selective Forwarding Unit) with `webrtc-rs`. It demonstrates the following:

- Other peers joining the call
- Trickle ICE
- Re-negotiation
- Audio with Opus and video with VP8

## Building

Run the build:
```
cargo build --example sfu
```

Navigate to `http://localhost:8081` in one browser/tab, and then open it again in another browser/tab.
