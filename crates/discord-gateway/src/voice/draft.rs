//! 生成途中の返信（core の partial）の読み上げ状態（D-1083）。テキストチャンネルごとに 1 つ持つ。
//!
//! partial は完成した文だけの累積本文。まだ読んでいない文を順に読み、最終 say では読んだ文との
//! 最長共通接頭辞より後ろだけを読む（読んだ文を同じ形で読み直さない）。読み上げの上限
//! [`MAX_TTS_CHARS`] は 1 つの stream で読んだ累計に掛け、超えたら「以下省略」を 1 回だけ読む。

use super::tts_text::{clean_for_tts_uncapped, split_sentences, MAX_TTS_CHARS, TTS_CAP_SUFFIX};

pub(crate) struct DraftState {
    pub(crate) stream_id: String,
    /// partial から読み上げに回した文（partial の文分割の順）。
    spoken: Vec<String>,
    /// この stream で読み上げに回した文字数の累計（上限判定用）。
    spoken_chars: usize,
    /// 上限に達して「以下省略」を読んだ。以後は何も読まない。
    capped: bool,
}

/// 読み上げ本文の文分割。partial と最終 say で同じ規則を使い、文どうしを比べられるようにする。
pub(crate) fn speech_segments(text: &str) -> Vec<String> {
    split_sentences(&clean_for_tts_uncapped(text))
}

impl DraftState {
    pub(crate) fn new(stream_id: &str) -> Self {
        Self {
            stream_id: stream_id.to_string(),
            spoken: Vec::new(),
            spoken_chars: 0,
            capped: false,
        }
    }

    /// partial の累積本文から、まだ読んでいない文を読み上げる分として返す。
    pub(crate) fn advance(&mut self, partial_text: &str) -> Vec<String> {
        let segments = speech_segments(partial_text);
        let mut speech = Vec::new();
        for segment in segments.into_iter().skip(self.spoken.len()) {
            if let Some(text) = self.within_cap(&segment) {
                speech.push(text);
            }
            self.spoken.push(segment);
        }
        speech
    }

    /// 最終 say の本文のうち、読んだ文との最長共通接頭辞より後ろを読み上げる分として返す。
    pub(crate) fn finish(mut self, final_text: &str) -> Vec<String> {
        let segments = speech_segments(final_text);
        let common = segments
            .iter()
            .zip(&self.spoken)
            .take_while(|(final_segment, spoken)| final_segment == spoken)
            .count();
        segments
            .into_iter()
            .skip(common)
            .filter_map(|segment| self.within_cap(&segment))
            .collect()
    }

    fn within_cap(&mut self, segment: &str) -> Option<String> {
        if self.capped {
            return None;
        }
        let chars = segment.chars().count();
        let remaining = MAX_TTS_CHARS - self.spoken_chars;
        if chars <= remaining {
            self.spoken_chars += chars;
            return Some(segment.to_string());
        }
        self.capped = true;
        self.spoken_chars = MAX_TTS_CHARS;
        let cut: String = segment.chars().take(remaining).collect();
        Some(format!("{cut}{TTS_CAP_SUFFIX}"))
    }
}
