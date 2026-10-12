# Conformance golden

`golden/conformance.json` holds the Rust `ferrite-analysis` output for
every case in `conformance_cases.json`. Both implementations are tested
against it:

- `analysis/tests/conformance.rs` checks that the Rust output still
  matches, so a definition change there fails `cargo test`.
- `test_conformance.py` checks that the Python port matches the same
  golden.

After an intentional change to a definition in `analysis/src/`:

```sh
FERRITE_CONFORMANCE_WRITE=1 cargo test -p ferrite-analysis --test conformance
```

Then port the same change to `ferrite_analyze/dsp.py` until
`python3 -I tools/analysis/tests/test_conformance.py` passes.
`FERRITE_CONFORMANCE_LIVE=1` makes the Python test run cargo and compare
against fresh Rust output instead of the committed file.
