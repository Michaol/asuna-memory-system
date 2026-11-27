# Third-Party Notices and License Attributions

## Inline FTS5 `jieba` Tokenizer (`src/index/jieba_tokenizer.rs`)

`src/index/jieba_tokenizer.rs` is adapted from **`sqlite-jieba-tokenizer` `0.6.0`** (upstream repository: <https://github.com/miyayaLora/sqlite-simple-tokenizer>, Copyright (c) Individual contributors / Cathy Luking `<cathyluking@foxmail.com>`).

Upstream `sqlite-simple-tokenizer` is dual-licensed under `MIT OR Apache-2.0`. This repository elects the **MIT License** option; the verbatim upstream MIT license text is preserved in [LICENSES/sqlite-simple-tokenizer-MIT](LICENSES/sqlite-simple-tokenizer-MIT).

## External Dependencies and FFI Attribution

- **`src/index/fts5_ffi.rs`**: Hardened in-process FTS5 v2 tokenizer FFI shim adapted from upstream `sqlite-simple-tokenizer` / ColonelThirtyTwo's FTS5 v2 wrapper gist (<https://gist.github.com/ColonelThirtyTwo/3dd1fe04e4cff0502fa70d12f3a6e72e>), dual-licensed `MIT OR Apache-2.0`. This repository elects the **MIT License** option; the verbatim upstream MIT license text is preserved in [LICENSES/sqlite-simple-tokenizer-MIT](LICENSES/sqlite-simple-tokenizer-MIT).
- **`sqlite-chinese-stopword` `0.1.0`**: Consumed strictly as an external crates.io dependency. Upstream's wordlist originates from `endNone/stopwords`, which ships without a `LICENSE` file; therefore, this repository intentionally does **not** copy, vendor, or inline any `stopword.txt` file from that upstream source.
- **`sqlite-english-stemmer` `0.1.0`**: Consumed as an external crates.io dependency for ASCII lowercase normalization, whitespace/punctuation filtering, and Snowball English stemming.
- **`jieba-rs` `0.11.0`**: Consumed as an external crates.io dependency and shared in-process via `crate::util::text::shared_jieba()`.
