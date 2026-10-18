//! 音声変換ユーティリティ。
//!
//! Discord (songbird) の受信 PCM は 48kHz ステレオ i16 インターリーブ。
//! STT へは 16kHz モノラル WAV に変換して渡す。依存を増やさないため
//! 変換は自前実装（音声認識用途では線形補間で十分）。

/// 48kHz ステレオ i16 → 16kHz モノラル i16。
///
/// ステレオはチャンネル平均でダウンミックスし、48k→16k は 3:1 の整数比
/// なので 3 サンプル平均で間引く（ローパスを兼ねる）。
pub fn downmix_48k_stereo_to_16k_mono(pcm: &[i16]) -> Vec<i16> {
    // インターリーブ [L R L R ...] → モノラル
    let mono: Vec<i32> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|lr| (lr[0] as i32 + lr[1] as i32) / 2)
        .collect();
    mono.chunks(3)
        .map(|w| (w.iter().sum::<i32>() / w.len() as i32) as i16)
        .collect()
}

/// 16bit PCM → WAV バイト列（44 バイトヘッダ + データ）。
pub fn pcm_to_wav(pcm: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// 発話セグメントの RMS（無音・ノイズだけの区間を STT に投げない判定用）。
pub fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / pcm.len() as f64).sqrt()
}

/// 1 フレーム（20ms・48kHz ステレオ）の RMS がこれ未満なら無音として数える（D-1085 P3）。
/// Discord は話し終えた後もしばらく小さな音のパケットを送ることがあるため。
pub const SILENT_FRAME_RMS: f64 = 250.0;
/// 話している間の途中認識の間隔（声のフレーム数、20ms 単位 = 200ms）。
pub const SNAPSHOT_EVERY_FRAMES: u32 = 10;
/// 無音がこのフレーム数（100ms）続いたら、それまでの全体を認識する（話し終わりの候補）。
pub const ONSET_SILENT_FRAMES: u32 = 5;
/// 無音がこのフレーム数（200ms）続き、話し終わり候補の認識結果が文の終わりらしければ確定する。
pub const CANDIDATE_END_SILENT_FRAMES: u32 = 10;
/// 無音がこのフレーム数（700ms）続いたら、認識結果によらず確定する。
pub const HARD_END_SILENT_FRAMES: u32 = 35;
/// これより短い（300ms 未満 = 48k*2ch*0.3s）発話は雑音として捨てる。
const MIN_UTTERANCE_SAMPLES: usize = 28_800;

/// 文の終わりらしい語尾（roleplay-demo `src/rp/shared/end-rule.ts` の SENTENCE_END と同じ一覧）。
const SENTENCE_ENDINGS: &[&str] = &[
    "です",
    "ます",
    "でした",
    "ました",
    "ません",
    "ませんか",
    "ください",
    "下さい",
    "ございます",
    "ですか",
    "ますか",
    "でしょうか",
    "ですね",
    "ますね",
    "ですよ",
    "ますよ",
    "か",
    "ね",
    "よ",
    "はい",
    "いいえ",
    "ええ",
    "うん",
    "そう",
    "なるほど",
    "わかりました",
    "分かりました",
];

/// 認識結果が文の終わりらしい（末尾の句読点・空白を除いて [`SENTENCE_ENDINGS`] のどれかで終わる）。
pub fn ends_like_sentence(text: &str) -> bool {
    let trimmed =
        text.trim_end_matches(|c: char| c.is_whitespace() || "。．.、,，！!？?…".contains(c));
    SENTENCE_ENDINGS
        .iter()
        .any(|ending| trimmed.ends_with(ending))
}

/// 話し終わり候補（無音 100ms）の認識結果が文の終わりらしいか。受信側の区切りと、認識する
/// 側（[`super::session::VoiceManager`]）で共有する。`seq` はその候補の [`SegmenterEvent::Snapshot`]。
#[derive(Clone, Default)]
pub struct EndVerdict(std::sync::Arc<std::sync::Mutex<Option<(u64, bool)>>>);

impl EndVerdict {
    pub fn set(&self, seq: u64, sentence_like: bool) {
        *self.0.lock().unwrap() = Some((seq, sentence_like));
    }

    fn sentence_like(&self, seq: u64) -> bool {
        *self.0.lock().unwrap() == Some((seq, true))
    }
}

/// 確定の理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// 無音 200ms 以上で、話し終わり候補の認識結果が文の終わりらしい。
    Candidate,
    /// 無音 700ms。
    Hard,
    /// 最大長に達した。
    MaxLength,
    /// VC から抜けた等で言い残しを確定した。
    Flush,
}

/// 区切りの出来事（48kHz ステレオ PCM）。`utt` は発話の番号、`seq` は認識依頼の番号（どちらも
/// その区切り内で増え続ける）。
pub enum SegmenterEvent {
    /// 途中までの音声を認識に回す。`onset` は無音 100ms（話し終わり候補）での全体。
    Snapshot {
        utt: u64,
        seq: u64,
        pcm: Vec<i16>,
        onset: bool,
        verdict: EndVerdict,
    },
    /// 発話の確定。`onset_seq` はこの発話の最後の話し終わり候補で、その後に声は無い
    /// （認識結果をそのまま使える）。最大長・言い残しの確定では `None`。
    End {
        utt: u64,
        pcm: Vec<i16>,
        onset_seq: Option<u64>,
        reason: EndReason,
        /// 最後の声の後、無音とみなした小さな音のパケットの数（Discord が送り続ける尾の長さ）。
        quiet_packets: u32,
    },
    /// 短すぎて捨てた発話。
    Discarded { utt: u64 },
}

/// ユーザー単位の発話セグメンテーション（D-1085 P3）。
///
/// Discord は 20ms ごとに音声パケットが届く。話している間は 200ms ごとに途中までを、無音 100ms で
/// 全体を認識に回す。無音 200ms で話し終わり候補の認識結果が文の終わりらしければ確定し、そうで
/// なければ無音 700ms で確定する。間に声が戻れば同じ発話の続き。`max_frames` を超えたら強制確定。
pub struct SpeechSegmenter {
    buf: Vec<i16>,
    silent_streak: u32,
    quiet_packets: u32,
    voiced_since_snapshot: u32,
    /// 1 発話の最大フレーム数（20ms 単位）。
    pub max_frames: usize,
    frames_in_buf: usize,
    utt: u64,
    next_seq: u64,
    last_onset: Option<u64>,
    verdict: EndVerdict,
}

impl SpeechSegmenter {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            silent_streak: 0,
            quiet_packets: 0,
            voiced_since_snapshot: 0,
            max_frames: 750, // 15s
            frames_in_buf: 0,
            utt: 0,
            next_seq: 0,
            last_onset: None,
            verdict: EndVerdict::default(),
        }
    }

    /// 20ms の音声パケットを追加する。RMS が [`SILENT_FRAME_RMS`] 未満なら無音として数える。
    pub fn push_frame(&mut self, frame: &[i16]) -> Option<SegmenterEvent> {
        if rms(frame) < SILENT_FRAME_RMS {
            if self.buf.is_empty() {
                return None;
            }
            self.buf.extend_from_slice(frame);
            self.frames_in_buf += 1;
            self.quiet_packets += 1;
            return self.silent_tick();
        }
        self.buf.extend_from_slice(frame);
        self.frames_in_buf += 1;
        self.silent_streak = 0;
        self.quiet_packets = 0;
        if self.frames_in_buf >= self.max_frames {
            return self.end(None, EndReason::MaxLength);
        }
        self.voiced_since_snapshot += 1;
        if self.voiced_since_snapshot >= SNAPSHOT_EVERY_FRAMES {
            return Some(self.snapshot(false));
        }
        None
    }

    /// このフレームで当該ユーザーが無音だったことを通知する。
    pub fn push_silence(&mut self) -> Option<SegmenterEvent> {
        if self.buf.is_empty() {
            return None;
        }
        self.silent_tick()
    }

    /// バッファに溜まっている音声を強制確定する（VC退出時など）。
    pub fn flush(&mut self) -> Option<SegmenterEvent> {
        if self.buf.is_empty() {
            return None;
        }
        self.end(None, EndReason::Flush)
    }

    fn silent_tick(&mut self) -> Option<SegmenterEvent> {
        self.silent_streak += 1;
        match self.silent_streak {
            ONSET_SILENT_FRAMES => Some(self.snapshot(true)),
            n if n >= HARD_END_SILENT_FRAMES => self.end(self.last_onset, EndReason::Hard),
            n if n >= CANDIDATE_END_SILENT_FRAMES
                && self
                    .last_onset
                    .is_some_and(|seq| self.verdict.sentence_like(seq)) =>
            {
                self.end(self.last_onset, EndReason::Candidate)
            }
            _ => None,
        }
    }

    fn snapshot(&mut self, onset: bool) -> SegmenterEvent {
        self.voiced_since_snapshot = 0;
        let seq = self.next_seq;
        self.next_seq += 1;
        if onset {
            self.last_onset = Some(seq);
        }
        SegmenterEvent::Snapshot {
            utt: self.utt,
            seq,
            pcm: self.buf.clone(),
            onset,
            verdict: self.verdict.clone(),
        }
    }

    fn end(&mut self, onset_seq: Option<u64>, reason: EndReason) -> Option<SegmenterEvent> {
        let utt = self.utt;
        let quiet_packets = self.quiet_packets;
        self.utt += 1;
        self.silent_streak = 0;
        self.quiet_packets = 0;
        self.voiced_since_snapshot = 0;
        self.frames_in_buf = 0;
        self.last_onset = None;
        let pcm = std::mem::take(&mut self.buf);
        if pcm.len() < MIN_UTTERANCE_SAMPLES {
            return Some(SegmenterEvent::Discarded { utt });
        }
        Some(SegmenterEvent::End {
            utt,
            pcm,
            onset_seq,
            reason,
            quiet_packets,
        })
    }
}

impl Default for SpeechSegmenter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downmix_ratio() {
        // 48kHz ステレオ 1 秒 = 96000 サンプル → 16kHz モノ 16000 サンプル
        let pcm = vec![1000i16; 96_000];
        let out = downmix_48k_stereo_to_16k_mono(&pcm);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|&s| s == 1000));
    }

    #[test]
    fn test_downmix_averages_channels() {
        // L=100, R=300 → 200
        let pcm: Vec<i16> = [100i16, 300].repeat(6);
        let out = downmix_48k_stereo_to_16k_mono(&pcm);
        assert!(out.iter().all(|&s| s == 200), "{out:?}");
    }

    #[test]
    fn test_wav_header() {
        let wav = pcm_to_wav(&[0i16; 16000], 16000, 1);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 32000);
        // sample rate field
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        // channels
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
    }

    #[test]
    fn test_rms() {
        assert_eq!(rms(&[]), 0.0);
        assert!(rms(&[0i16; 100]) < 1.0);
        assert!(rms(&[10000i16; 100]) > 9999.0);
    }
}
