# Reference projects

These are **references only** — do not link against them. The actual code lives in `../crates/`.

Submodules already present (added 2026-05-15):

- `aa-proxy-rs/` → https://github.com/manio/aa-proxy-rs.git
- `aasdk/`      → https://github.com/f1xpl/aasdk.git
- `openauto/`   → https://github.com/f1xpl/openauto.git

After cloning, run:

```bash
git submodule update --init --recursive
```

### Milek7's AAP protocol notes (not in git)

milek7 publishes notes as a static web page rather than a git repo:

- https://milek7.pl/.stuff/galdocs/readme.md
- https://milek7.pl/.stuff/galdocs/huig13_cache.html

Their Wireshark dissector for AAP (`androidauto.lua`) is mentioned in those docs — useful when analyzing `captures/*.pcap`.

If you want a local copy, mirror with `wget -mk -np milek7.pl/.stuff/galdocs/` into `references/milek7-galdocs/` (added to `.gitignore` so it's not committed).

## What each reference gives us

| Repo | Useful pieces |
|---|---|
| `aa-proxy-rs` | USB gadget + AOAv2 handshake, in Rust. The ConfigFS scripts under `setup/` are language-neutral and directly reusable. Closest reference for our source role. |
| `aasdk` | `aasdk_proto/*.proto` files — the wire format definitions. SSL handshake reference (C++). Channel ID enum. Service descriptors. **The proto files are what `aabox-proto/build.rs` consumes.** |
| `openauto` | Headunit-side reference implementation using aasdk. Useful for understanding how a complete AAP endpoint is structured even though we're on the other side. |
| milek7's notes | Channel-level protocol notes for things aasdk doesn't cover well — especially the Navigation Status channel which is the Phase 6 target. |

## Why submodules, not Cargo deps

We treat these as documentation. The wire protocol is small enough to reimplement cleanly. Linking against aasdk (C++) from a Rust project would mean an awkward FFI surface; linking against aa-proxy-rs would mean inheriting its proxy-shaped architecture which isn't what we want.
