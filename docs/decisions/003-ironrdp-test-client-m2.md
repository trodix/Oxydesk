# 003 — IronRDP back end of the test client (M2)

Status: accepted (M2).

## Decision
`bins/rdp-test-client` (feature `ironrdp`) drives the connection with the low-level crates (`ironrdp-connector`, `-blocking`,
`-session`, `-egfx`, `-dvc`, `-cliprdr`, `-input`) in a worker thread. It does **not** use `ironrdp-client`.

## Why not `ironrdp-client` 0.1.0 (read in the source, not only the docs)
- Library only (the GUI is `ironrdp-viewer`), output through a bounded `mpsc::Sender<RdpOutputEvent>`.
- **No EGFX at all**: only DisplayControl and Echo are attached as DVCs (`rdp.rs`). Our server emits nothing but H.264 over
  EGFX: this client would receive no image.
- **No CLIPRDR injection point**: the factory is built internally from `ClipboardType` (`Enable | Disable | Stub`); no
  in-memory back end is possible.
- `ironrdp-tls` (rustls, unverified certificate) could replace our small TLS code; low gain, postponed.

## Technical findings
- The client-side EGFX decoder has to be supplied (`H264Decoder`); ours uses libopenh264 loaded at run time and accepts
  Annex-B (what the server sends) or length-prefixed AVC.
- rustls: `ring` and `aws-lc-rs` coexist in the tree; the provider must be chosen explicitly (server `install_default`, client
  `builder_with_provider`), otherwise it panics at run time.
- TLS only by default; CredSSP/NLA is enabled on the client side if `RDP_TEST_PASSWORD` is set.
- The server certificate is never verified: local tests only.

## Measured results (synthetic, libopenh264 2.6)
- Local 1280x720 and Debian 13 VM 1920x1080: 6/6 checks, key→pixel p95 ≈ 35 ms, ~34 fps.
