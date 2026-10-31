//! 読み上げの合成をまとめる（D-1085。D-1103 から VOICEVOX と ZEN-TTS）。合成は TTS キャッシュを先に引き、外れたら 1 本の合成役へ渡す。
//!
//! VOICEVOX は CPU で 1 本ずつしか速く合成できないため、合成役は 1 つだけにし、読み上げ（Live）を
//! 先行生成の文（Held）、先読み（Prewarm）の順に合成する。合成中の 1 件は途中で止めない。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::{oneshot, Notify};

use super::clients;
use super::settings::{TtsSettings, VoiceChoice};
use super::tts_cache::{choice_cache_key, TtsCache};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SynthesisPriority {
    /// いま読み上げる文。
    Live,
    /// 先行生成（provisional）の返事の文。再生はせず、確定に備えてキャッシュへ入れる（D-1085 P5）。
    Held,
    /// 相づち・よく使う言い回しの先読み。
    Prewarm,
}

struct Job {
    tts: TtsSettings,
    choice: VoiceChoice,
    text: String,
    reply: Option<oneshot::Sender<anyhow::Result<Vec<u8>>>>,
}

#[derive(Default)]
struct Jobs {
    live: VecDeque<Job>,
    held: VecDeque<Job>,
    prewarm: VecDeque<Job>,
}

pub(crate) struct Synthesizer {
    http: reqwest::Client,
    pub(crate) cache: TtsCache,
    jobs: Mutex<Jobs>,
    notify: Notify,
    /// 最後に先読みした TTS 設定。設定が変わったら確かめ直して先読みし直す。
    refreshed: Mutex<Option<TtsSettings>>,
    /// 先読みする本文（この bot の相づちと、よく使う短い返事。D-1101）。
    prewarm_phrases: Vec<String>,
}

impl Synthesizer {
    pub(crate) fn new(
        http: reqwest::Client,
        cache: TtsCache,
        prewarm_phrases: Vec<String>,
    ) -> Arc<Self> {
        let synthesizer = Arc::new(Self {
            http,
            cache,
            jobs: Mutex::new(Jobs::default()),
            notify: Notify::new(),
            refreshed: Mutex::new(None),
            prewarm_phrases,
        });
        tokio::spawn(run_worker(synthesizer.clone()));
        synthesizer
    }

    /// `choice` の声で `text` を合成した WAV。キャッシュにあれば合成しない。
    pub(crate) async fn synthesize(
        &self,
        tts: &TtsSettings,
        choice: &VoiceChoice,
        text: &str,
        priority: SynthesisPriority,
    ) -> anyhow::Result<Vec<u8>> {
        let text_chars = text.chars().count();
        let started = std::time::Instant::now();
        if let Some(wav) = self.cached(choice, text) {
            tracing::debug!(
                text_chars,
                elapsed_us = started.elapsed().as_micros() as u64,
                "TTS cache hit"
            );
            return Ok(wav);
        }
        tracing::debug!(
            text_chars,
            cache_enabled = self.cache.is_enabled(),
            "TTS cache miss"
        );
        let (reply, result) = oneshot::channel();
        self.push(
            Job {
                tts: tts.clone(),
                choice: choice.clone(),
                text: text.to_string(),
                reply: Some(reply),
            },
            priority,
        );
        result
            .await
            .map_err(|_| anyhow::anyhow!("TTS synthesis worker stopped"))?
    }

    /// キャッシュにある WAV だけを返す（合成しない）。
    pub(crate) fn cached(&self, choice: &VoiceChoice, text: &str) -> Option<Vec<u8>> {
        self.cache.get(&choice_cache_key(choice, text))
    }

    /// 設定が前回の先読みと違うときだけ [`Self::refresh`] を走らせる。
    pub(crate) fn refresh_if_changed(self: &Arc<Self>, tts: &TtsSettings) {
        if self.refreshed.lock().unwrap().as_ref() == Some(tts) {
            return;
        }
        let synthesizer = self.clone();
        let tts = tts.clone();
        tokio::spawn(async move { synthesizer.refresh(&tts).await });
    }

    /// エンジンの版を確かめてキャッシュを使えるようにし、設定にある全スタイルで相づちと
    /// よく使う言い回しを先読みする。版が読めなければキャッシュは使わない（合成は今まで通り）。
    pub(crate) async fn refresh(&self, tts: &TtsSettings) {
        *self.refreshed.lock().unwrap() = Some(tts.clone());
        let version = match clients::engine_version(&self.http, tts).await {
            Ok(version) => version,
            Err(error) => {
                self.cache.disable();
                // 次の読み上げ・join で確かめ直す。
                *self.refreshed.lock().unwrap() = None;
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "TTS cache disabled; a TTS engine version is unreadable"
                );
                return;
            }
        };
        if let Err(error) = self.cache.verify(&version) {
            self.cache.disable();
            *self.refreshed.lock().unwrap() = None;
            tracing::warn!(error = %format!("{error:#}"), "TTS cache disabled");
            return;
        }
        for choice in tts.all_choices() {
            for phrase in &self.prewarm_phrases {
                if self.cached(&choice, phrase).is_some() {
                    continue;
                }
                self.push(
                    Job {
                        tts: tts.clone(),
                        choice: choice.clone(),
                        text: phrase.to_string(),
                        reply: None,
                    },
                    SynthesisPriority::Prewarm,
                );
            }
        }
    }

    fn push(&self, job: Job, priority: SynthesisPriority) {
        {
            let mut jobs = self.jobs.lock().unwrap();
            match priority {
                SynthesisPriority::Live => jobs.live.push_back(job),
                SynthesisPriority::Held => jobs.held.push_back(job),
                SynthesisPriority::Prewarm => jobs.prewarm.push_back(job),
            }
        }
        self.notify.notify_one();
    }

    fn next_job(&self) -> Option<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.live
            .pop_front()
            .or_else(|| jobs.held.pop_front())
            .or_else(|| jobs.prewarm.pop_front())
    }
}

async fn run_worker(synthesizer: Arc<Synthesizer>) {
    loop {
        let Some(job) = synthesizer.next_job() else {
            synthesizer.notify.notified().await;
            continue;
        };
        let key = choice_cache_key(&job.choice, &job.text);
        let result = match synthesizer.cache.get(&key) {
            Some(wav) => Ok(wav),
            None => {
                let started = std::time::Instant::now();
                let text_chars = job.text.chars().count();
                tracing::debug!(text_chars, "TTS synthesis started");
                let result =
                    clients::synthesize_choice(&synthesizer.http, &job.tts, &job.choice, &job.text)
                        .await;
                tracing::debug!(
                    text_chars,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "TTS synthesis finished"
                );
                if let Ok(wav) = &result {
                    synthesizer.cache.insert(&key, wav);
                }
                result
            }
        };
        match job.reply {
            Some(reply) => {
                let _ = reply.send(result);
            }
            None => {
                if let Err(error) = result {
                    tracing::warn!(error = %format!("{error:#}"), "TTS prewarm failed");
                }
            }
        }
    }
}
