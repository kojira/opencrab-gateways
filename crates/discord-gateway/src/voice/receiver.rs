//! 受信音声の話者別区切り。songbird の VoiceTick は SSRC 単位なので、SSRC ごとに
//! [`SpeechSegmenter`] を持ち、区切りの出来事を Speaking で知った Discord user へ結びつける。

use std::collections::HashMap;

use super::audio::{SegmenterEvent, SpeechSegmenter};

/// 話者の user id と、その話者の区切りの出来事。
pub type SpeakerEvent = (u64, SegmenterEvent);

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

    pub fn on_frame(&mut self, ssrc: u32, pcm: &[i16]) -> Option<SpeakerEvent> {
        let event = self.segmenters.entry(ssrc).or_default().push_frame(pcm)?;
        self.attribute(ssrc, event)
    }

    pub fn on_silent(&mut self, ssrc: u32) -> Option<SpeakerEvent> {
        let event = self.segmenters.get_mut(&ssrc)?.push_silence()?;
        self.attribute(ssrc, event)
    }

    /// 話者が VC から抜けたら言い残しを確定する（以後その SSRC は届かない）。
    pub fn on_disconnect(&mut self, user_id: u64) -> Vec<SpeakerEvent> {
        let ssrcs: Vec<u32> = self
            .users
            .iter()
            .filter(|(_, user)| **user == user_id)
            .map(|(ssrc, _)| *ssrc)
            .collect();
        let mut finished = Vec::new();
        for ssrc in ssrcs {
            if let Some(event) = self
                .segmenters
                .remove(&ssrc)
                .and_then(|mut segmenter| segmenter.flush())
            {
                finished.push((user_id, event));
            }
            self.users.remove(&ssrc);
        }
        finished
    }

    fn attribute(&self, ssrc: u32, event: SegmenterEvent) -> Option<SpeakerEvent> {
        match self.users.get(&ssrc) {
            Some(user) => Some((*user, event)),
            None => {
                tracing::debug!(ssrc, "speech from an unmapped SSRC dropped");
                None
            }
        }
    }
}
