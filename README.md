# Ferrite

A modern web-based SDR application. Spectrum-centric, pleasant, fast.

Runs a thin Rust daemon (`ferrited`) next to the antenna — typically on an ARM
SBC — and a rich browser front end that does the demodulation and decoding in
WASM. The backend streams a wideband FFT for the waterfall plus narrowband IQ
slices for each active VFO; the browser owns everything downstream.

Decoders (ADS-B, APRS, digital voice, FT8, …) are built as shared blocks
(Rust + WASM, plus ported C cores) wired together by JSON flowgraph files. A
small optional Node sidecar can run the same flowgraphs headlessly on the SBC
when no browser is attached.

## Status

Pre-alpha. The repository currently contains design documents only. Code
starts landing once [Phase 0](docs/08-roadmap.md) (documentation) is complete.

## Documentation

Orient here, roughly in order:

- [00 — Context and goals](docs/00-context.md)
- [01 — Architecture](docs/01-architecture.md)
- [02 — Control API and WS frame format](docs/02-protocol.md)
- [03 — Block system](docs/03-blocks.md)
- [04 — Flowgraph JSON schema](docs/04-flowgraphs.md)
- [05 — Testing strategy](docs/05-testing.md)
- [06 — Build and dev setup](docs/06-build.md)
- [07 — Deployment](docs/07-deploy.md)
- [08 — Roadmap](docs/08-roadmap.md)
- [09 — Decision log](docs/09-decisions.md)
- [10 — Commit-level implementation plan](docs/10-commits.md)

## Target platform

- **OS:** Ubuntu 24.04 LTS (Noble) or newer, on the SBC and on dev machines.
  Other Linux distros probably work but are not tested. Non-Linux hosts are
  out of scope.
- **Hardware:** developed and validated against RTL-SDR (RTL2832U) and
  SDRPlay RSPduo via SoapySDR. Design keeps the door open for HackRF,
  Airspy, and anything else Soapy supports.

## License

See [LICENSE](LICENSE).
