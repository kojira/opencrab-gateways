use super::super::receiver::SsrcSegments;

fn frame(v: i16) -> Vec<i16> {
    vec![v; 1920] // 20ms・48kHz ステレオ
}

#[test]
fn segments_are_split_per_ssrc_and_attributed_to_the_speaker() {
    let mut s = SsrcSegments::default();
    s.on_speaking(1, 100);
    s.on_speaking(2, 200);
    for _ in 0..20 {
        assert!(s.on_frame(1, &frame(1000)).is_none());
        assert!(s.on_frame(2, &frame(2000)).is_none());
    }
    // ssrc 1 だけ 800ms 無音で確定する。ssrc 2 はまだ話している。
    let mut done = Vec::new();
    for _ in 0..40 {
        done.extend(s.on_silent(1));
        assert!(s.on_frame(2, &frame(2000)).is_none());
    }
    assert_eq!(done.len(), 1);
    let (user, pcm) = &done[0];
    assert_eq!(*user, 100);
    assert_eq!(pcm.len(), 20 * 1920);
    assert!(pcm.iter().all(|&x| x == 1000));
}

#[test]
fn unmapped_ssrc_is_dropped_and_disconnect_flushes_the_rest() {
    let mut s = SsrcSegments::default();
    for _ in 0..20 {
        s.on_frame(9, &frame(500));
    }
    let mut done = Vec::new();
    for _ in 0..40 {
        done.extend(s.on_silent(9));
    }
    assert!(done.is_empty(), "話者不明の SSRC は捨てる");

    s.on_speaking(3, 300);
    for _ in 0..20 {
        s.on_frame(3, &frame(700));
    }
    let flushed = s.on_disconnect(300);
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0].0, 300);
    assert!(s.on_disconnect(300).is_empty(), "二重に出さない");
}
