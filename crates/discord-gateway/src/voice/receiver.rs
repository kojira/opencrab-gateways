//! 受信音声の話者別区切り。songbird の VoiceTick は SSRC 単位なので、SSRC ごとに
//! [`SpeechSegmenter`] を持ち、確定した発話を Speaking で知った Discord user へ結びつける。

use std::collections::HashMap;

use super::audio::SpeechSegmenter;

/// 確定した発話（話者の user id と 48kHz ステレオ PCM）。
pub type FinishedSpeech = (u64, Vec<i16>);

#[derive(Default)]
pub struct SsrcSegments {
    users: HashMap<u32, u64>,
    segmenters: HashMap<u32, SpeechSegmenter>,
}

impl SsrcSegments {
    /// Speaking 通知で SSRC と話者を結びつける。
    pub fn on_speaking(&mut self, ssrc: u32, user_id: u64) {
        self.users.insert(ssrc, user_id);
    }

    pub fn on_frame(&mut self, ssrc: u32, pcm: &[i16]) -> Option<FinishedSpeech> {
        let segment = self.segmenters.entry(ssrc).or_default().push_frame(pcm)?;
        self.attribute(ssrc, segment.pcm_48k_stereo)
    }

    pub fn on_silent(&mut self, ssrc: u32) -> Option<FinishedSpeech> {
        let segment = self.segmenters.get_mut(&ssrc)?.push_silence()?;
        self.attribute(ssrc, segment.pcm_48k_stereo)
    }

    /// 話者が VC から抜けたら言い残しを確定する（以後その SSRC は届かない）。
    pub fn on_disconnect(&mut self, user_id: u64) -> Vec<FinishedSpeech> {
        let ssrcs: Vec<u32> = self
            .users
            .iter()
            .filter(|(_, user)| **user == user_id)
            .map(|(ssrc, _)| *ssrc)
            .collect();
        let mut finished = Vec::new();
        for ssrc in ssrcs {
            if let Some(segment) = self
                .segmenters
                .remove(&ssrc)
                .and_then(|mut segmenter| segmenter.flush())
            {
                finished.push((user_id, segment.pcm_48k_stereo));
            }
            self.users.remove(&ssrc);
        }
        finished
    }

    fn attribute(&self, ssrc: u32, pcm: Vec<i16>) -> Option<FinishedSpeech> {
        match self.users.get(&ssrc) {
            Some(user) => Some((*user, pcm)),
            None => {
                tracing::debug!(ssrc, "speech from an unmapped SSRC dropped");
                None
            }
        }
    }
}
