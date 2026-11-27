// Adapted from upstream `sqlite-simple-tokenizer` (Jieba FTS5 tokenizer v0.6.0,
// https://github.com/miyayaLora/sqlite-simple-tokenizer)
// Copyright (c) Individual contributors / Cathy Luking <cathyluking@foxmail.com>
// Licensed under the MIT License (see LICENSES/sqlite-simple-tokenizer-MIT and NOTICE.md).

use super::fts5_ffi::{register_tokenizer, TokenizeReason, Tokenizer};
use rusqlite::Connection;
use sqlite_chinese_stopword::STOPWORD;
use sqlite_english_stemmer::{is_space_or_ascii_punctuation_str, make_lowercase, EN_STEMMER};
use std::ffi::CStr;
use std::ops::Range;

/// In-process FTS5 `jieba` tokenizer sharing the global dictionary in
/// [`crate::util::text::shared_jieba`].
pub struct JiebaTokenizer {
    enable_stopword: bool,
}

impl Default for JiebaTokenizer {
    fn default() -> Self {
        Self {
            enable_stopword: true,
        }
    }
}

impl JiebaTokenizer {
    pub(crate) fn tokenize_with<F>(&self, text: &[u8], mut emit: F) -> Result<(), rusqlite::Error>
    where
        F: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        let jieba = crate::util::text::shared_jieba();
        let mut word_buf = String::new();
        let mut chunk_offset = 0usize;

        for chunk in text.utf8_chunks() {
            let valid = chunk.valid();
            for token in jieba.cut(valid, true) {
                let word = token.word;
                if is_space_or_ascii_punctuation_str(word) {
                    continue;
                }
                let need_stem = make_lowercase(word, &mut word_buf);
                if self.enable_stopword && STOPWORD.contains(word_buf.as_str()) {
                    continue;
                }
                let range = (chunk_offset + token.byte_start)..(chunk_offset + token.byte_end);
                if need_stem {
                    let stemmed = EN_STEMMER.stem(word_buf.as_str());
                    emit(stemmed.as_bytes(), range, false)?;
                } else {
                    emit(word_buf.as_bytes(), range, false)?;
                }
            }
            chunk_offset += valid.len() + chunk.invalid().len();
        }
        Ok(())
    }
}

impl Tokenizer for JiebaTokenizer {
    type Global = ();

    fn name() -> &'static CStr {
        c"jieba"
    }

    fn new(_global: &Self::Global, args: Vec<String>) -> Result<Self, rusqlite::Error> {
        let mut tokenizer = Self::default();
        for arg in args {
            if arg == "disable_stopword" {
                tokenizer.enable_stopword = false;
            }
        }
        Ok(tokenizer)
    }

    fn tokenize<TKF>(
        &mut self,
        _reason: TokenizeReason,
        text: &[u8],
        push_token: TKF,
    ) -> Result<(), rusqlite::Error>
    where
        TKF: FnMut(&[u8], Range<usize>, bool) -> Result<(), rusqlite::Error>,
    {
        self.tokenize_with(text, push_token)
    }
}

/// Registers the `jieba` FTS5 tokenizer on the given SQLite connection.
pub fn register(conn: &Connection) -> anyhow::Result<()> {
    register_tokenizer::<JiebaTokenizer>(conn, ())
        .map_err(|e| anyhow::anyhow!("jieba tokenizer: {e}"))
}

#[cfg(test)]
#[path = "jieba_tokenizer_tests.rs"]
mod tests;
