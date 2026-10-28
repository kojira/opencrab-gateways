//! 生成途中の返信（core の partial）の読み上げ状態（D-1083）。テキストチャンネルごとに 1 つ持つ。
//!
//! partial は完成した文だけの累積本文。まだ読んでいない文を順に読み、最終 say では読んだ文との
//! 最長共通接頭辞より後ろだけを読む（読んだ文を同じ形で読み直さない）。読み上げの長さに上限は
//! 無い（D-1096）。

use super::tts_text::{clean_for_tts, split_sentences};

pub(crate) struct DraftState {
    pub(crate) stream_id: String,
    /// partial から読み上げに回した文（partial の文分割の順）。
    spoken: Vec<String>,
}

/// 読み上げ本文の文分割。partial と最終 say で同じ規則を使い、文どうしを比べられるようにする。
pub(crate) fn speech_segments(text: &str) -> Vec<String> {
    split_sentences(&clean_for_tts(text))
}

impl DraftState {
    pub(crate) fn new(stream_id: &str) -> Self {
        Self {
            stream_id: stream_id.to_string(),
            spoken: Vec::new(),
        }
    }

    /// partial の累積本文から、まだ読んでいない文を読み上げる分として返す。
    pub(crate) fn advance(&mut self, partial_text: &str) -> Vec<String> {
        let segments = speech_segments(partial_text);
        let speech: Vec<String> = segments.into_iter().skip(self.spoken.len()).collect();
        self.spoken.extend(speech.iter().cloned());
        speech
    }

    /// 最終 say の本文のうち、読んだ文との最長共通接頭辞より後ろを読み上げる分として返す。
    pub(crate) fn finish(self, final_text: &str) -> Vec<String> {
        let segments = speech_segments(final_text);
        let common = segments
            .iter()
            .zip(&self.spoken)
            .take_while(|(final_segment, spoken)| final_segment == spoken)
            .count();
        segments.into_iter().skip(common).collect()
    }
}
