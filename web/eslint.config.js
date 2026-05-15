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
    ],
  },
);
