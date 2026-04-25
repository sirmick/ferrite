# Vendored multimon-ng

Source of `vendor/` in this crate.

| field        | value |
|--------------|-------|
| upstream     | https://github.com/EliasOenal/multimon-ng |
| pinned at    | `2e8ed0285cc01f5ce2ad5e7c4b030ea8931b9d6e` |
| license      | GPL-2.0-or-later (compatible with this codebase's GPL-3.0-or-later) |
| trimmed dirs | `.git/`, `test/`, `demo/`, `example/`, `unsupported/` (not built) |

## What we don't compile

- `unixinput.c` — upstream's `main()` + stdin/stdout I/O. We replace its
  `_verbprintf` and the dozen-or-so module-static globals it owns with
  our `shim/multimon_shim.c` so the decoders run as a library.
- `gen_*.c` — modulators (test signal generators). We never need them.
- `demod_sdl_scope.c` — SDL3-based oscilloscope. Drags in libSDL3.
- Demods we haven't wrapped yet — see `build.rs::decoder_sources()`.
  Every supported decoder requires (a) its `demod_<x>.c` (and any
  per-family support file like `pocsag.c`), (b) an `allowlist_var`
  bindgen entry, and (c) a Rust `Decoder::<X>` variant.

## Resyncing upstream

When bumping to a new upstream commit:

1. `git -C ../../../research/multimon-ng pull` (or re-clone)
2. `cp -r ../../../research/multimon-ng vendor` (overwrite)
3. `rm -rf vendor/{.git,test,demo,example,unsupported}`
4. Update the pinned hash above
5. `cargo test -p ferrite-multimon-ng` — bindgen + the C build will
   surface any header changes. New direct `printf` calls in demods may
   need additions to `shim/multimon_shim.c`.
