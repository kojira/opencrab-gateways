//! D-1085 P3（E1–E4）と §11 の途中認識: 受信側の区切り（SpeechSegmenter）の出来事。
use crate::voice::audio::{
    ends_like_sentence, EndReason, EndVerdict, SegmenterEvent, SpeechSegmenter,
};

fn voiced() -> Vec<i16> {
    vec![3000; 1920] // 20ms・48kHz ステレオ
}

fn quiet() -> Vec<i16> {
    vec![10; 1920]
}

/// 1 秒話し、話し終わり候補（無音 100ms）の認識結果を `sentence_like` として返す。
/// その時点までの出来事と、話し終わり候補の判定の置き場を返す。
fn speak_then_onset(seg: &mut SpeechSegmenter, sentence_like: bool) -> Vec<SegmenterEvent> {
    let mut events: Vec<SegmenterEvent> =
        (0..50).filter_map(|_| seg.push_frame(&voiced())).collect();
    for tick in 1..=5 {
        if let Some(event) = seg.push_silence() {
            assert_eq!(tick, 5, "無音 100ms までは出来事なし");
            events.push(event);
        }
    }
    let Some(SegmenterEvent::Snapshot {
        onset: true,
        seq,
        verdict,
        ..
    }) = events.last()
    else {
        panic!("無音 100ms で全体の認識を依頼する");
    };
    verdict.set(*seq, sentence_like);
    events
}

fn end_tick(
    seg: &mut SpeechSegmenter,
    from: u32,
    to: u32,
) -> Option<(u32, EndReason, Option<u64>)> {
    for tick in from..=to {
        if let Some(SegmenterEvent::End {
            reason, onset_seq, ..
        }) = seg.push_silence()
        {
            return Some((tick, reason, onset_seq));
        }
    }
    None
}

#[test]
fn snapshots_are_taken_every_200ms_while_speaking() {
    let mut seg = SpeechSegmenter::new();
    let snapshots: Vec<(u64, usize, bool)> = (0..50)
        .filter_map(|_| seg.push_frame(&voiced()))
        .map(|event| match event {
            SegmenterEvent::Snapshot {
                seq, pcm, onset, ..
            } => (seq, pcm.len(), onset),
            _ => panic!("話している間は途中認識だけ"),
        })
        .collect();
    assert_eq!(
        snapshots,
        (0..5)
            .map(|i| (i as u64, (i + 1) * 10 * 1920, false))
            .collect::<Vec<_>>()
    );
}

#[test]
fn e1_a_sentence_like_ending_commits_at_200ms() {
    let mut seg = SpeechSegmenter::new();
    let events = speak_then_onset(&mut seg, true);
    let onset = match events.last() {
        Some(SegmenterEvent::Snapshot { seq, .. }) => *seq,
        _ => unreachable!(),
    };
    assert_eq!(
        end_tick(&mut seg, 6, 40),
        Some((10, EndReason::Candidate, Some(onset)))
    );
}

#[test]
fn e2_another_ending_waits_until_700ms() {
    let mut seg = SpeechSegmenter::new();
    let events = speak_then_onset(&mut seg, false);
    let onset = match events.last() {
        Some(SegmenterEvent::Snapshot { seq, .. }) => *seq,
        _ => unreachable!(),
    };
    assert_eq!(
        end_tick(&mut seg, 6, 40),
        Some((35, EndReason::Hard, Some(onset)))
    );
}

#[test]
fn e2_an_unanswered_candidate_waits_until_700ms() {
    // 話し終わり候補の認識がまだ返っていなければ 200ms では確定しない。
    let mut seg = SpeechSegmenter::new();
    for _ in 0..50 {
        seg.push_frame(&voiced());
    }
    assert_eq!(
        end_tick(&mut seg, 1, 40).map(|(tick, reason, _)| (tick, reason)),
        Some((35, EndReason::Hard))
    );
}

#[test]
fn e3_voice_resuming_at_400ms_continues_the_same_utterance() {
    let mut seg = SpeechSegmenter::new();
    let events = speak_then_onset(&mut seg, false);
    let utt = match events.last() {
        Some(SegmenterEvent::Snapshot { utt, .. }) => *utt,
        _ => unreachable!(),
    };
    assert_eq!(end_tick(&mut seg, 6, 20), None);
    for _ in 0..25 {
        if let Some(event) = seg.push_frame(&voiced()) {
            assert!(matches!(event, SegmenterEvent::Snapshot { utt: u, .. } if u == utt));
        }
    }
    // 続きの後の無音で新しい話し終わり候補が出て、同じ発話として確定する（声は両方入り、パケットの
    // 無い無音は入らない）。
    let mut onset = None;
    let mut end = None;
    for _ in 0..40 {
        match seg.push_silence() {
            Some(SegmenterEvent::Snapshot {
                onset: true,
                seq,
                verdict,
                ..
            }) => {
                verdict.set(seq, true);
                onset = Some(seq);
            }
            Some(SegmenterEvent::End {
                utt: u,
                pcm,
                onset_seq,
                reason,
                ..
            }) => {
                end = Some((u, pcm.len(), onset_seq, reason));
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        end,
        Some((utt, (50 + 25) * 1920, onset, EndReason::Candidate))
    );
}

#[test]
fn e4_low_energy_packets_count_as_silence() {
    let mut seg = SpeechSegmenter::new();
    for _ in 0..50 {
        seg.push_frame(&voiced());
    }
    let mut verdict: Option<EndVerdict> = None;
    let mut end = None;
    for tick in 1..=40 {
        match seg.push_frame(&quiet()) {
            Some(SegmenterEvent::Snapshot {
                onset: true,
                seq,
                verdict: v,
                ..
            }) => {
                assert_eq!(tick, 5);
                v.set(seq, true);
                verdict = Some(v);
            }
            Some(SegmenterEvent::End {
                reason,
                quiet_packets,
                ..
            }) => {
                end = Some((tick, reason, quiet_packets));
                break;
            }
            _ => {}
        }
    }
    assert!(verdict.is_some());
    assert_eq!(end, Some((10, EndReason::Candidate, 10)));
    // 小さな音だけでは発話を始めない。
    let mut seg = SpeechSegmenter::new();
    assert!((0..60).all(|_| seg.push_frame(&quiet()).is_none()));
}

#[test]
fn too_short_speech_is_discarded() {
    let mut seg = SpeechSegmenter::new();
    for _ in 0..10 {
        seg.push_frame(&voiced());
    }
    let mut last = None;
    for _ in 0..40 {
        if let Some(event) = seg.push_silence() {
            last = Some(event);
        }
    }
    assert!(matches!(last, Some(SegmenterEvent::Discarded { utt: 0 })));
}

#[test]
fn speech_is_force_committed_at_the_max_length() {
    let mut seg = SpeechSegmenter::new();
    seg.max_frames = 50;
    let mut end = None;
    for i in 0..60 {
        if let Some(SegmenterEvent::End { pcm, reason, .. }) = seg.push_frame(&voiced()) {
            end = Some((i, pcm.len(), reason));
            break;
        }
    }
    assert_eq!(end, Some((49, 1920 * 50, EndReason::MaxLength)));
}

#[test]
fn sentence_endings_follow_the_roleplay_demo_list() {
    for text in [
        "そうですね",
        "行きます。",
        "本当ですか？",
        "うん",
        "わかりました！",
        "それでいいよ",
    ] {
        assert!(ends_like_sentence(text), "{text}");
    }
    for text in ["駅まで", "それで、", "明日は", ""] {
        assert!(!ends_like_sentence(text), "{text}");
    }
}
