# Reference projects

These are **references only** — do not link against them. The actual code lives in `../crates/`.

To populate, run (from the repo root):

```bash
git submodule add https://github.com/manio/aa-proxy-rs.git references/aa-proxy-rs
git submodule add https://github.com/f1xpl/aasdk.git           references/aasdk
git submodule add https://github.com/f1xpl/openauto.git        references/openauto
# milek7's notes — TODO: confirm exact repo URL before adding
```

## What each reference gives us

| Repo | Useful pieces |
|---|---|
| `aa-proxy-rs` | USB gadget + AOAv2 handshake, in Rust. The ConfigFS scripts under `setup/` are language-neutral and directly reusable. Closest reference for our source role. |
| `aasdk` | `aasdk_proto/*.proto` files — the wire format definitions. SSL handshake reference (C++). Channel ID enum. Service descriptors. **The proto files are what `aabox-proto/build.rs` consumes.** |
| `openauto` | Headunit-side reference implementation using aasdk. Useful for understanding how a complete AAP endpoint is structured even though we're on the other side. |
| milek7's notes | Channel-level protocol notes for things aasdk doesn't cover well — especially the Navigation Status channel which is the Phase 6 target. |

## Why submodules, not Cargo deps

We treat these as documentation. The wire protocol is small enough to reimplement cleanly. Linking against aasdk (C++) from a Rust project would mean an awkward FFI surface; linking against aa-proxy-rs would mean inheriting its proxy-shaped architecture which isn't what we want.
