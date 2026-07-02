import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import ts from 'typescript-eslint';

export default ts.config(
  js.configs.recommended,
  ...ts.configs.recommended,
  ...svelte.configs['flat/recommended'],
  prettier,
  ...svelte.configs['flat/prettier'],
  {
    languageOptions: {
      globals: {
        ...globals.browser,
        ...globals.node,
      },
    },
    rules: {
      // Accept the `_`-prefix convention for intentionally unused args
      // / catch bindings / destructured slots. Matches TSLint's old
      // default and is the standard signal for "kept for compatibility,
      // not used here."
      '@typescript-eslint/no-unused-vars': [
        'error',
        { argsIgnorePattern: '^_', varsIgnorePattern: '^_', caughtErrorsIgnorePattern: '^_' },
      ],
    },
  },
  {
    files: ['**/*.svelte'],
    languageOptions: {
      parserOptions: {
        parser: ts.parser,
      },
    },
    rules: {
      // TypeScript itself resolves identifiers, so core `no-undef` is a
      // false positive on type-land — `<script generics="…">` params
      // and ambient namespace types (e.g. `GeoJSON.FeatureCollection`
      // from @types/geojson) read as "undefined" to it. typescript-
      // eslint disables this rule for its own TS-matched files for the
      // same reason; the .svelte override needs it explicitly.
      'no-undef': 'off',
    },
  },
  {
    ignores: [
      'build/',
      '.svelte-kit/',
      'dist/',
      'src/lib/wasm/blocks/',
      'src/lib/wasm/runtime/',
      // Generated Emscripten module; fldigiBridge.ts stays linted.
      'src/lib/wasm/fldigi/fldigi.mjs',
      // Generated whisper.cpp Emscripten glue (minified, `@ts-nocheck`);
      // whisper.d.ts in the same dir is hand-written and stays linted.
      'src/lib/wasm/whisper/whisper.mjs',
      // Generated sherpa-onnx ASR Emscripten glue (minified); the whole
      // dir is machine-emitted by blocks/native/sherpa/emscripten/build.sh.
      'src/lib/wasm/sherpa/',
    ],
  },
);
